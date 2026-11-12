//! Passkeys for guest passes (WebAuthn, ES256). A link made with `--passkey` makes the
//! device that opens it register a passkey; from then on the pass alone isn't enough, the
//! browser must also sign a fresh challenge with that passkey.
//!
//! The page may be this host's tailnet address or glassdesk.dev/app, so the relying party
//! isn't pinned; what binds the passkey is the stored public key, checked on every sign-in
//! against a challenge this host just made.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tracing::info;
use wtransport::{RecvStream, SendStream};

use crate::control::{self, ClientMsg, ServerMsg};
use crate::remote::{b64url, b64url_decode};

/// How long the browser's passkey sheet may take.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);
/// COSE ES256: ECDSA P-256 with SHA-256.
const ES256: i64 = -7;
/// DER SubjectPublicKeyInfo header for an uncompressed P-256 key (id-ecPublicKey, prime256v1).
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
    0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

/// Sent to the browser: create a passkey, or sign in with one.
#[derive(Debug, Clone, Serialize)]
pub struct Challenge {
    /// "create" or "get".
    pub mode: &'static str,
    /// Random bytes, base64url.
    pub challenge: String,
    /// Name shown by the browser's passkey sheet.
    pub user: String,
    /// For "get": the credential to use, base64url.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// The browser must verify the user (device posture), not just see a tap.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub uv: bool,
}

/// The browser's answer; binary fields are base64url. `mode: "error"` when the browser
/// declined (no passkey, cancelled).
#[derive(Debug, Clone, Deserialize)]
pub struct Response {
    pub mode: String,
    /// Credential id.
    #[serde(default)]
    pub id: String,
    /// clientDataJSON.
    #[serde(default)]
    pub client_data: String,
    /// For "get".
    #[serde(default)]
    pub authenticator_data: Option<String>,
    #[serde(default)]
    pub signature: Option<String>,
    /// For "create": the public key as SubjectPublicKeyInfo DER (getPublicKey()).
    #[serde(default)]
    pub public_key: Option<String>,
    /// COSE algorithm (getPublicKeyAlgorithm()); only -7 (ES256) is accepted.
    #[serde(default)]
    pub algorithm: Option<i64>,
}

/// Has the device that just opened a link register a passkey, and binds it to `pass`.
/// False if the browser declined or the answer didn't check out.
pub async fn register(
    send: &mut SendStream,
    recv: &mut RecvStream,
    access_file: &Path,
    pass: &str,
    user: &str,
) -> Result<bool> {
    let challenge = new_challenge()?;
    let msg = ServerMsg::Passkey(Challenge {
        mode: "create",
        challenge: challenge.clone(),
        user: format!("Glass guest ({user})"),
        credential: None,
        uv: crate::settings::load().posture_passkey_uv,
    });
    control::write_json(send, &msg).await?;
    let Some(r) = answer(recv).await? else {
        return Ok(false);
    };
    ensure!(r.mode == "create", "expected a new passkey, got {:?}", r.mode);
    let origin = check_client_data(&decode(&r.client_data)?, "webauthn.create", &challenge)?;
    ensure!(r.algorithm == Some(ES256), "only ES256 passkeys are supported");
    let spki = decode(r.public_key.as_deref().context("no public key")?)?;
    let point = spki_point(&spki).context("not a P-256 public key")?;
    ensure!(
        decode(&r.id).is_ok_and(|id| !id.is_empty()),
        "bad credential id"
    );
    let bound = crate::access::bind_passkey(
        access_file,
        pass,
        crate::access::Passkey {
            id: r.id,
            public_key: b64url(&point),
            counter: 0,
        },
    )?;
    if bound {
        info!("passkey registered for a guest pass (origin {origin})");
    }
    Ok(bound)
}

/// Asks the browser to sign a fresh challenge with the passkey bound to `pass`.
pub async fn verify(
    send: &mut SendStream,
    recv: &mut RecvStream,
    access_file: &Path,
    pass: &str,
    key: &crate::access::Passkey,
) -> Result<bool> {
    let challenge = new_challenge()?;
    let msg = ServerMsg::Passkey(Challenge {
        mode: "get",
        challenge: challenge.clone(),
        user: String::new(),
        credential: Some(key.id.clone()),
        uv: crate::settings::load().posture_passkey_uv,
    });
    control::write_json(send, &msg).await?;
    let Some(r) = answer(recv).await? else {
        return Ok(false);
    };
    ensure!(r.mode == "get", "expected a passkey signature, got {:?}", r.mode);
    ensure!(r.id == key.id, "a different passkey answered");
    let point = decode(&key.public_key)?;
    let counter = check_assertion(
        &point,
        &decode(r.authenticator_data.as_deref().context("no authenticator data")?)?,
        &decode(&r.client_data)?,
        &decode(r.signature.as_deref().context("no signature")?)?,
        &challenge,
    )?;
    let auth_data = decode(r.authenticator_data.as_deref().unwrap_or_default())?;
    check_user_verified(&auth_data, crate::settings::load().posture_passkey_uv)?;
    ensure!(
        crate::access::advance_passkey_counter(access_file, pass, counter)?,
        "the passkey's counter went backwards (a copied key?)"
    );
    Ok(true)
}

/// The browser's next passkey answer, skipping anything else it sends meanwhile; `None`
/// when it declined.
async fn answer(recv: &mut RecvStream) -> Result<Option<Response>> {
    let read = async {
        loop {
            let Some(buf) = control::read_frame(recv).await? else {
                bail!("the browser left before answering");
            };
            if let Ok(ClientMsg::Passkey(r)) = serde_json::from_slice::<ClientMsg>(&buf) {
                return Ok(r);
            }
        }
    };
    let r = tokio::time::timeout(ANSWER_TIMEOUT, read)
        .await
        .context("no passkey answer in time")??;
    Ok((r.mode != "error").then_some(r))
}

fn new_challenge() -> Result<String> {
    let mut bytes = [0u8; 32];
    crate::control::getrandom(&mut bytes)?;
    Ok(b64url(&bytes))
}

fn decode(s: &str) -> Result<Vec<u8>> {
    b64url_decode(s).context("bad base64url")
}

/// The 65-byte uncompressed point of a P-256 SubjectPublicKeyInfo.
fn spki_point(spki: &[u8]) -> Option<[u8; 65]> {
    let point = spki.strip_prefix(&P256_SPKI_PREFIX[..])?;
    (point.len() == 65 && point[0] == 0x04).then(|| point.try_into().unwrap())
}

/// Checks clientDataJSON's type and challenge; returns its origin.
fn check_client_data(raw: &[u8], kind: &str, challenge: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct ClientData {
        #[serde(rename = "type")]
        kind: String,
        challenge: String,
        origin: String,
    }
    let data: ClientData = serde_json::from_slice(raw).context("bad clientDataJSON")?;
    ensure!(data.kind == kind, "clientData type is {:?}", data.kind);
    ensure!(
        control::token_matches(data.challenge.trim_end_matches('='), challenge),
        "the passkey signed a different challenge"
    );
    let local = data.origin == "http://localhost" || data.origin.starts_with("http://localhost:");
    ensure!(
        data.origin.starts_with("https://") || (local && crate::testmode::enabled()),
        "unexpected origin {}",
        data.origin
    );
    Ok(data.origin)
}

/// Device posture (`glass config posture-require-passkey-uv on`): the authenticator must
/// have verified the user (Face ID, Touch ID, a PIN), not just seen a tap.
fn check_user_verified(auth_data: &[u8], required: bool) -> Result<()> {
    // flags bit 2: UV.
    ensure!(
        !required || auth_data.get(32).is_some_and(|f| f & 0x04 != 0),
        "this computer requires a passkey with Face ID, Touch ID or a PIN"
    );
    Ok(())
}

/// Verifies a WebAuthn assertion; returns the authenticator's signature counter.
fn check_assertion(
    point: &[u8],
    auth_data: &[u8],
    client_data: &[u8],
    signature: &[u8],
    challenge: &str,
) -> Result<u32> {
    let origin = check_client_data(client_data, "webauthn.get", challenge)?;
    // rpIdHash (32) | flags (1) | signCount (4, big-endian) | …
    ensure!(auth_data.len() >= 37, "authenticator data too short");
    ensure!(auth_data[32] & 0x01 != 0, "the user wasn't present");
    let mut signed = auth_data.to_vec();
    signed.extend_from_slice(ring::digest::digest(&ring::digest::SHA256, client_data).as_ref());
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, point)
        .verify(&signed, signature)
        .map_err(|_| anyhow::anyhow!("bad passkey signature"))?;
    info!("passkey verified (origin {origin})");
    Ok(u32::from_be_bytes(auth_data[33..37].try_into().unwrap()))
}

#[cfg(test)]
mod uv_tests {
    use super::check_user_verified;

    #[test]
    fn user_verification_only_when_required() {
        let mut auth = vec![0u8; 37];
        auth[32] = 0x01; // present, not verified
        assert!(check_user_verified(&auth, false).is_ok());
        assert!(check_user_verified(&auth, true).is_err());
        auth[32] = 0x05;
        assert!(check_user_verified(&auth, true).is_ok());
        assert!(check_user_verified(&[], true).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};

    struct Authenticator {
        key: EcdsaKeyPair,
        rng: SystemRandom,
    }

    impl Authenticator {
        fn new() -> Self {
            let rng = SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
            let key =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
                    .unwrap();
            Self { key, rng }
        }

        fn spki(&self) -> Vec<u8> {
            let mut spki = P256_SPKI_PREFIX.to_vec();
            spki.extend_from_slice(self.key.public_key().as_ref());
            spki
        }

        /// (authenticatorData, clientDataJSON, signature)
        fn sign(&self, challenge: &str, counter: u32, flags: u8) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
            let mut auth = vec![7u8; 32];
            auth.push(flags);
            auth.extend_from_slice(&counter.to_be_bytes());
            let client = format!(
                r#"{{"type":"webauthn.get","challenge":"{challenge}","origin":"https://mac.tail1234.ts.net","crossOrigin":false}}"#
            )
            .into_bytes();
            let mut signed = auth.clone();
            signed.extend_from_slice(ring::digest::digest(&ring::digest::SHA256, &client).as_ref());
            let sig = self.key.sign(&self.rng, &signed).unwrap().as_ref().to_vec();
            (auth, client, sig)
        }
    }

    #[test]
    fn spki_must_be_p256() {
        let a = Authenticator::new();
        let point = spki_point(&a.spki()).unwrap();
        assert_eq!(&point[..], a.key.public_key().as_ref());
        assert!(spki_point(&a.spki()[1..]).is_none());
        let mut wrong_curve = a.spki();
        wrong_curve[22] ^= 1;
        assert!(spki_point(&wrong_curve).is_none());
    }

    #[test]
    fn client_data_checks_type_challenge_and_origin() {
        let ok = br#"{"type":"webauthn.create","challenge":"abc","origin":"https://glassdesk.dev"}"#;
        assert_eq!(
            check_client_data(ok, "webauthn.create", "abc").unwrap(),
            "https://glassdesk.dev"
        );
        assert!(check_client_data(ok, "webauthn.get", "abc").is_err());
        assert!(check_client_data(ok, "webauthn.create", "abd").is_err());
        let plain =
            br#"{"type":"webauthn.create","challenge":"abc","origin":"http://evil.example"}"#;
        assert!(check_client_data(plain, "webauthn.create", "abc").is_err());
    }

    #[test]
    fn assertions_verify_and_tampering_fails() {
        let a = Authenticator::new();
        let point = spki_point(&a.spki()).unwrap();
        let challenge = new_challenge().unwrap();
        let (auth, client, sig) = a.sign(&challenge, 5, 0x05);
        assert_eq!(
            check_assertion(&point, &auth, &client, &sig, &challenge).unwrap(),
            5
        );

        // A signature over an old challenge can't be replayed against a new one.
        let fresh = new_challenge().unwrap();
        assert!(check_assertion(&point, &auth, &client, &sig, &fresh).is_err());
        // Tampered counter, signature.
        let mut bumped = auth.clone();
        bumped[36] ^= 1;
        assert!(check_assertion(&point, &bumped, &client, &sig, &challenge).is_err());
        let mut bad_sig = sig.clone();
        let last = bad_sig.len() - 1;
        bad_sig[last] ^= 1;
        assert!(check_assertion(&point, &auth, &client, &bad_sig, &challenge).is_err());
        // Another key's signature.
        let other = Authenticator::new();
        let (auth2, client2, sig2) = other.sign(&challenge, 1, 0x05);
        assert!(check_assertion(&point, &auth2, &client2, &sig2, &challenge).is_err());
        // User presence is required.
        let (auth3, client3, sig3) = a.sign(&challenge, 6, 0x04);
        assert!(check_assertion(&point, &auth3, &client3, &sig3, &challenge).is_err());
    }
}
