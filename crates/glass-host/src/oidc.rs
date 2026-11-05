//! Single sign-on with OpenID Connect: people sign in with their Google, Microsoft, Okta…
//! account instead of a token or a link, if the access rules allow them (`glass acl`, or
//! the older `glass config oidc-allow`; see acl.rs) and SCIM hasn't deactivated them.
//! Signing in mints a guest pass, as a link would.
//!
//! Authorization code flow with PKCE. The state, nonce and code verifier ride in a
//! short-lived cookie signed with a key derived from the access token. The ID token is
//! checked (RS256 or ES256) against the provider's published keys.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use axum::extract::Query;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use tracing::{info, warn};

use crate::remote::{b64url, b64url_decode};

const COOKIE: &str = "glass_sso";
/// How long a sign-in may take at the provider.
const FLOW_TTL: u64 = 600;
/// Clock skew allowed on `exp`.
const SKEW: u64 = 60;
const DISCOVERY_TTL: Duration = Duration::from_secs(3600);

/// SSO as configured; `None` when it's off (issuer, client id or secret missing).
#[derive(Clone)]
struct Config {
    issuer: String,
    client_id: String,
    secret: String,
}

fn config() -> Option<Config> {
    let s = crate::settings::load();
    let issuer = s.oidc_issuer?;
    // Plain HTTP only for a local test provider.
    if !issuer.starts_with("https://") && !crate::testmode::enabled() {
        return None;
    }
    Some(Config {
        issuer,
        client_id: s.oidc_client_id?,
        secret: crate::settings::oidc_secret()?,
    })
}

/// Whether the sign-in screen should offer SSO.
pub fn enabled() -> bool {
    config().is_some()
}

pub fn routes() -> axum::Router {
    axum::Router::new()
        .route("/sso/start", get(start))
        .route("/sso/callback", get(callback))
}

// ---- Provider metadata ----

#[derive(Clone, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

static DISCOVERY: Mutex<Option<(String, Discovery, Instant)>> = Mutex::new(None);
static JWKS: Mutex<Option<(String, Vec<Jwk>)>> = Mutex::new(None);

async fn discovery(issuer: &str) -> Result<Discovery> {
    if let Some((i, d, at)) = DISCOVERY.lock().unwrap().as_ref()
        && i == issuer
        && at.elapsed() < DISCOVERY_TTL
    {
        return Ok(d.clone());
    }
    let url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
    let d: Discovery = serde_json::from_slice(&curl_get(&url).await?).context("discovery document")?;
    *DISCOVERY.lock().unwrap() = Some((issuer.to_string(), d.clone(), Instant::now()));
    Ok(d)
}

/// The provider's signing keys; fetched again when a token names a key we don't have.
async fn keys(jwks_uri: &str, kid: Option<&str>) -> Result<Vec<Jwk>> {
    if let Some((uri, keys)) = JWKS.lock().unwrap().as_ref()
        && uri == jwks_uri
        && kid.is_none_or(|k| keys.iter().any(|j| j.kid.as_deref() == Some(k)))
    {
        return Ok(keys.clone());
    }
    #[derive(Deserialize)]
    struct Set {
        keys: Vec<Jwk>,
    }
    let set: Set = serde_json::from_slice(&curl_get(jwks_uri).await?).context("key set")?;
    *JWKS.lock().unwrap() = Some((jwks_uri.to_string(), set.keys.clone()));
    Ok(set.keys)
}

async fn curl_get(url: &str) -> Result<Vec<u8>> {
    let out = tokio::process::Command::new("curl")
        .args(["-fsS", "--max-time", "10", "--proto", "=https,http", url])
        .output()
        .await?;
    ensure!(out.status.success(), "GET {url} failed");
    Ok(out.stdout)
}

/// POSTs a form; the body (with the client secret) goes through stdin, never argv.
async fn curl_post_form(url: &str, form: &[(&str, &str)]) -> Result<Vec<u8>> {
    use tokio::io::AsyncWriteExt;
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let mut child = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "10",
            "--proto",
            "=https,http",
            "-H",
            "content-type: application/x-www-form-urlencoded",
            "-H",
            "accept: application/json",
            "--data-binary",
            "@-",
            url,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    child.stdin.take().context("curl stdin")?.write_all(body.as_bytes()).await?;
    let out = child.wait_with_output().await?;
    ensure!(out.status.success(), "POST {url} failed");
    Ok(out.stdout)
}

pub(crate) fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ---- The flow ----

/// Where the provider sends the browser back: this host, as the browser reached it.
fn redirect_uri(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    if host.is_empty() || host.contains(['/', '@', ' ', '?', '#']) {
        return None;
    }
    let local = host.starts_with("localhost") || host.starts_with("127.0.0.1") || host.starts_with("[::1]");
    let scheme = if local { "http" } else { "https" };
    Some(format!("{scheme}://{host}/sso/callback"))
}

async fn start(headers: HeaderMap) -> Response {
    let Some(cfg) = config() else {
        return page(StatusCode::NOT_FOUND, "Single sign-on isn't set up on this computer.");
    };
    let Some(redirect) = redirect_uri(&headers) else {
        return page(StatusCode::BAD_REQUEST, "Sign-in didn't work. Open this computer's address again.");
    };
    let d = match discovery(&cfg.issuer).await {
        Ok(d) => d,
        Err(e) => {
            warn!("sso: {e:#}");
            return page(StatusCode::BAD_GATEWAY, "Couldn't reach the sign-in provider. Try again in a moment.");
        }
    };
    let (Ok(state), Ok(nonce), Ok(verifier)) = (random(16), random(16), random(32)) else {
        return page(StatusCode::INTERNAL_SERVER_ERROR, "Sign-in didn't work. Try again.");
    };
    let Some(cookie) = flow_cookie(&state, &nonce, &verifier, crate::access::now() + FLOW_TTL) else {
        return page(StatusCode::INTERNAL_SERVER_ERROR, "Sign-in didn't work. Try again.");
    };
    let sep = if d.authorization_endpoint.contains('?') { '&' } else { '?' };
    let location = format!(
        "{}{sep}response_type=code&client_id={}&redirect_uri={}&scope=openid%20email%20profile&state={state}&nonce={nonce}&code_challenge={}&code_challenge_method=S256",
        d.authorization_endpoint,
        urlencode(&cfg.client_id),
        urlencode(&redirect),
        pkce_challenge(&verifier),
    );
    let secure = if redirect.starts_with("https://") { "; Secure" } else { "" };
    let mut r = StatusCode::FOUND.into_response();
    if let (Ok(loc), Ok(c)) = (
        HeaderValue::from_str(&location),
        HeaderValue::from_str(&format!("{COOKIE}={cookie}; Path=/sso; HttpOnly; SameSite=Lax; Max-Age={FLOW_TTL}{secure}")),
    ) {
        r.headers_mut().insert(header::LOCATION, loc);
        r.headers_mut().insert(header::SET_COOKIE, c);
    }
    r
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback(headers: HeaderMap, Query(q): Query<CallbackQuery>) -> Response {
    let Some(cfg) = config() else {
        return page(StatusCode::NOT_FOUND, "Single sign-on isn't set up on this computer.");
    };
    if q.error.is_some() {
        return page(StatusCode::FORBIDDEN, "Sign-in was cancelled.");
    }
    match finish(&cfg, &headers, q).await {
        Ok((email, granted)) => {
            info!("sso: {email} signed in ({})", granted.scope.describe());
            crate::connlog::append(&crate::connlog::Entry {
                start: crate::access::now(),
                secs: 0,
                device: email.clone(),
                how: "sso".into(),
                remote: false,
                peer: String::new(),
                detail: format!("sso {email}"),
            });
            let mut r = StatusCode::FOUND.into_response();
            let to = format!("/#p={}&e={}", granted.pass, granted.expires);
            if let Ok(v) = HeaderValue::from_str(&to) {
                r.headers_mut().insert(header::LOCATION, v);
            }
            // The flow cookie is spent.
            r.headers_mut().insert(
                header::SET_COOKIE,
                HeaderValue::from_static("glass_sso=; Path=/sso; HttpOnly; Max-Age=0"),
            );
            r
        }
        Err(Refusal::NotAllowed(email)) => {
            warn!("sso: {email} isn't on the allow-list");
            page(StatusCode::FORBIDDEN, "That account isn't allowed to use this computer.")
        }
        Err(Refusal::Failed(e)) => {
            warn!("sso: {e:#}");
            page(StatusCode::FORBIDDEN, "Sign-in didn't work. Go back and try again.")
        }
    }
}

enum Refusal {
    NotAllowed(String),
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for Refusal {
    fn from(e: anyhow::Error) -> Self {
        Self::Failed(e)
    }
}

async fn finish(cfg: &Config, headers: &HeaderMap, q: CallbackQuery) -> std::result::Result<(String, crate::access::Granted), Refusal> {
    let (code, state) = q.code.zip(q.state).context("no code in the callback")?;
    let cookie = cookie(headers).context("no sign-in in progress (cookie missing)")?;
    let flow = check_flow_cookie(&cookie, crate::access::now()).context("bad or expired sign-in cookie")?;
    if !crate::control::token_matches(&flow.state, &state) {
        return Err(anyhow::anyhow!("state mismatch").into());
    }
    let redirect = redirect_uri(headers).context("no host")?;
    let d = discovery(&cfg.issuer).await?;
    let body = curl_post_form(
        &d.token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect),
            ("client_id", &cfg.client_id),
            ("client_secret", &cfg.secret),
            ("code_verifier", &flow.verifier),
        ],
    )
    .await?;
    #[derive(Deserialize)]
    struct Tokens {
        id_token: Option<String>,
    }
    let tokens: Tokens = serde_json::from_slice(&body).context("token response")?;
    let id_token = tokens.id_token.context("no id_token in the token response")?;
    let kid = header_of(&id_token).ok().and_then(|h| h.kid);
    let jwks = keys(&d.jwks_uri, kid.as_deref()).await?;
    let claims = verify_id_token(&id_token, &jwks, &d.issuer, &cfg.client_id, &flow.nonce, crate::access::now())?;
    let email = claims.email.to_ascii_lowercase();
    // The access rules (glass acl, or oidc-allow) and the SCIM directory decide.
    let person = crate::ldap::Person {
        email: email.clone(),
        groups: Vec::new(),
    };
    match crate::ldap::grant(&person)? {
        Some(granted) => Ok((email, granted)),
        None => Err(Refusal::NotAllowed(email)),
    }
}

/// A short plain page for errors (no details: those go to the host log).
fn page(status: StatusCode, msg: &str) -> Response {
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'><title>Glass</title>\
         <body style='font:16px system-ui;background:#000;color:#eee;display:grid;place-items:center;height:100vh;margin:0'>\
         <main style='text-align:center'><p>{}</p><p><a style='color:#5cb2ff' href='/'>Back to Glass</a></p></main>",
        msg.replace('<', "&lt;")
    );
    (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

fn cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_string())
}

// ---- Pure parts (unit tested) ----

fn random(n: usize) -> Result<String> {
    let mut bytes = vec![0u8; n];
    crate::control::getrandom(&mut bytes)?;
    Ok(b64url(&bytes))
}

/// PKCE S256: base64url(SHA-256(verifier)).
pub fn pkce_challenge(verifier: &str) -> String {
    b64url(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref())
}


/// Key for the flow cookie: derived from the access token, so a token reset voids it.
fn flow_key() -> Option<ring::hmac::Key> {
    let shared = crate::server::shared()?;
    let token = crate::control::load_or_create_token(&shared.token_file).ok()?;
    Some(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, format!("glass sso {token}").as_bytes()))
}

fn flow_cookie(state: &str, nonce: &str, verifier: &str, expires: u64) -> Option<String> {
    Some(sign_flow(&flow_key()?, state, nonce, verifier, expires))
}

fn sign_flow(key: &ring::hmac::Key, state: &str, nonce: &str, verifier: &str, expires: u64) -> String {
    let body = format!("{state}.{nonce}.{verifier}.{expires}");
    let tag = ring::hmac::sign(key, body.as_bytes());
    format!("{body}.{}", b64url(tag.as_ref()))
}

struct Flow {
    state: String,
    nonce: String,
    verifier: String,
}

fn check_flow_cookie(cookie: &str, now: u64) -> Option<Flow> {
    check_flow(&flow_key()?, cookie, now)
}

fn check_flow(key: &ring::hmac::Key, cookie: &str, now: u64) -> Option<Flow> {
    let (body, tag) = cookie.rsplit_once('.')?;
    ring::hmac::verify(key, body.as_bytes(), &b64url_decode(tag)?).ok()?;
    let parts: Vec<&str> = body.split('.').collect();
    let [state, nonce, verifier, expires] = parts[..] else { return None };
    (expires.parse::<u64>().ok()? > now).then(|| Flow {
        state: state.into(),
        nonce: nonce.into(),
        verifier: verifier.into(),
    })
}

#[derive(Clone, Deserialize)]
pub struct Jwk {
    kty: String,
    #[serde(default)]
    pub(crate) kid: Option<String>,
    #[serde(default)]
    pub(crate) alg: Option<String>,
    // RSA
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    // EC
    #[serde(default)]
    crv: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct JwtHeader {
    pub(crate) alg: String,
    #[serde(default)]
    pub(crate) kid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub aud: serde_json::Value,
    pub exp: u64,
    #[serde(default)]
    pub nonce: Option<String>,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub email_verified: Option<serde_json::Value>,
}

pub(crate) fn header_of(jwt: &str) -> Result<JwtHeader> {
    let h = jwt.split('.').next().context("not a JWT")?;
    Ok(serde_json::from_slice(&b64url_decode(h).context("bad JWT header")?)?)
}

/// Checks an ID token's signature and claims; returns them if it's good.
pub fn verify_id_token(jwt: &str, keys: &[Jwk], issuer: &str, client_id: &str, nonce: &str, now: u64) -> Result<Claims> {
    let parts: Vec<&str> = jwt.split('.').collect();
    let [h, p, sig] = parts[..] else { bail!("not a JWT") };
    let header = header_of(jwt)?;
    let sig = b64url_decode(sig).context("bad signature encoding")?;
    let signed = format!("{h}.{p}");
    let candidates: Vec<&Jwk> = keys
        .iter()
        .filter(|k| header.kid.is_none() || k.kid == header.kid)
        .filter(|k| k.alg.as_deref().is_none_or(|a| a == header.alg))
        .collect();
    ensure!(!candidates.is_empty(), "no key for this token");
    let good = candidates.iter().any(|k| verify_sig(k, &header.alg, signed.as_bytes(), &sig));
    ensure!(good, "bad signature");
    let claims: Claims = serde_json::from_slice(&b64url_decode(p).context("bad payload")?)?;
    ensure!(claims.iss.trim_end_matches('/') == issuer.trim_end_matches('/'), "wrong issuer");
    let aud_ok = match &claims.aud {
        serde_json::Value::String(a) => a == client_id,
        serde_json::Value::Array(list) => list.iter().any(|a| a.as_str() == Some(client_id)),
        _ => false,
    };
    ensure!(aud_ok, "token is for another client");
    ensure!(claims.exp + SKEW > now, "token expired");
    ensure!(
        claims.nonce.as_deref().is_some_and(|n| crate::control::token_matches(n, nonce)),
        "wrong nonce"
    );
    ensure!(!claims.email.is_empty(), "no email in the token");
    // Some providers (Microsoft) don't say; only an explicit "not verified" is refused.
    let unverified = matches!(&claims.email_verified, Some(serde_json::Value::Bool(false)))
        || matches!(&claims.email_verified, Some(serde_json::Value::String(s)) if s == "false");
    ensure!(!unverified, "email not verified");
    Ok(claims)
}

pub(crate) fn verify_sig(key: &Jwk, alg: &str, msg: &[u8], sig: &[u8]) -> bool {
    use ring::signature;
    match (alg, key.kty.as_str()) {
        ("RS256", "RSA") => {
            let (Some(n), Some(e)) = (key.n.as_deref().and_then(b64url_decode), key.e.as_deref().and_then(b64url_decode)) else {
                return false;
            };
            signature::RsaPublicKeyComponents { n: &n, e: &e }
                .verify(&signature::RSA_PKCS1_2048_8192_SHA256, msg, sig)
                .is_ok()
        }
        ("ES256", "EC") if key.crv.as_deref() == Some("P-256") => {
            let (Some(x), Some(y)) = (key.x.as_deref().and_then(b64url_decode), key.y.as_deref().and_then(b64url_decode)) else {
                return false;
            };
            if x.len() != 32 || y.len() != 32 {
                return false;
            }
            let mut point = vec![4u8];
            point.extend_from_slice(&x);
            point.extend_from_slice(&y);
            signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point)
                .verify(msg, sig)
                .is_ok()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};

    struct Provider {
        pair: EcdsaKeyPair,
        jwk: Jwk,
    }

    fn provider() -> Provider {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let point = pair.public_key().as_ref().to_vec();
        let jwk = Jwk {
            kty: "EC".into(),
            kid: Some("k1".into()),
            alg: Some("ES256".into()),
            n: None,
            e: None,
            crv: Some("P-256".into()),
            x: Some(b64url(&point[1..33])),
            y: Some(b64url(&point[33..65])),
        };
        Provider { pair, jwk }
    }

    fn sign(p: &Provider, claims: serde_json::Value) -> String {
        let h = b64url(br#"{"alg":"ES256","kid":"k1","typ":"JWT"}"#);
        let c = b64url(claims.to_string().as_bytes());
        let msg = format!("{h}.{c}");
        let sig = p.pair.sign(&ring::rand::SystemRandom::new(), msg.as_bytes()).unwrap();
        format!("{msg}.{}", b64url(sig.as_ref()))
    }

    fn claims(now: u64) -> serde_json::Value {
        serde_json::json!({
            "iss": "https://id.example.com", "aud": "glass", "exp": now + 300,
            "nonce": "n1", "email": "Ana@Example.com", "email_verified": true
        })
    }

    #[test]
    fn accepts_a_good_token_and_rejects_bad_ones() {
        let p = provider();
        let keys = [p.jwk.clone()];
        let now = 1_000_000;
        let ok = |jwt: &str| verify_id_token(jwt, &keys, "https://id.example.com/", "glass", "n1", now);
        assert_eq!(ok(&sign(&p, claims(now))).unwrap().email, "Ana@Example.com");

        let mut c = claims(now);
        c["aud"] = "other".into();
        assert!(ok(&sign(&p, c)).is_err(), "wrong audience");
        let mut c = claims(now);
        c["aud"] = serde_json::json!(["x", "glass"]);
        assert!(ok(&sign(&p, c)).is_ok(), "audience list");
        let mut c = claims(now);
        c["exp"] = (now - 120).into();
        assert!(ok(&sign(&p, c)).is_err(), "expired");
        let mut c = claims(now);
        c["nonce"] = "n2".into();
        assert!(ok(&sign(&p, c)).is_err(), "wrong nonce");
        let mut c = claims(now);
        c["iss"] = "https://evil.example.com".into();
        assert!(ok(&sign(&p, c)).is_err(), "wrong issuer");
        let mut c = claims(now);
        c["email_verified"] = false.into();
        assert!(ok(&sign(&p, c)).is_err(), "unverified email");

        // Tampered payload, and a token signed by someone else.
        let jwt = sign(&p, claims(now));
        let parts: Vec<&str> = jwt.split('.').collect();
        let mut c = claims(now);
        c["email"] = "mallory@example.com".into();
        let forged = format!("{}.{}.{}", parts[0], b64url(c.to_string().as_bytes()), parts[2]);
        assert!(ok(&forged).is_err(), "bad signature");
        let other = provider();
        assert!(ok(&sign(&other, claims(now))).is_err(), "unknown signer");
    }


    #[test]
    fn pkce_matches_the_rfc_example() {
        // RFC 7636, appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn flow_cookie_is_signed_and_expires() {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"k");
        let c = sign_flow(&key, "s", "n", "v", 100);
        let f = check_flow(&key, &c, 50).unwrap();
        assert_eq!((f.state.as_str(), f.nonce.as_str(), f.verifier.as_str()), ("s", "n", "v"));
        assert!(check_flow(&key, &c, 100).is_none(), "expired");
        assert!(check_flow(&key, &c.replace(".n.", ".x."), 50).is_none(), "tampered");
        let other = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"other");
        assert!(check_flow(&other, &c, 50).is_none());
    }
}
