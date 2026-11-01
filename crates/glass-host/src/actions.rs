//! Fleet actions: what your glassdesk.dev account may ask this computer to do (refresh its
//! details, update or restart Glass, restart the computer, run a script you put here).
//!
//! glassdesk.dev only relays. Every action is a command signed by a passkey of yours, and
//! this computer checks the signature itself against passkeys it pinned when you set it up
//! (`glass fleet manage`), so the website can't make one up. Commands expire in minutes and
//! work once. The list of actions is fixed; scripts must be files in `fleet/scripts` that
//! you own and nobody else can write. Everything that runs is logged in `fleet/actions.log`.
//!
//! Pinning: `glass fleet manage` prints a link holding a one-time code that never goes to
//! glassdesk.dev. You open it where you're signed in; the page has a passkey vouch for itself
//! with a MAC keyed by that code, which only the link's holder (you) can make, and the
//! computer pins the key. Only passkeys using ES256 (nearly all) can manage computers.

#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use ring::{digest, hmac, signature};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::remote::{HostKey, b64url, b64url_decode};

const ENROLL_SECS: u64 = 15 * 60;
/// A command must expire within this long of now.
const COMMAND_SECS: u64 = 10 * 60;
const MANAGERS_MAX: usize = 8;
const SCRIPT_SECS: u64 = 60;
/// How much of a script's output goes back.
const OUTPUT_MAX: usize = 2000;
/// SubjectPublicKeyInfo header of a P-256 key; the 65-byte point follows.
const SPKI_P256: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

fn dir() -> PathBuf {
    crate::fleet::dir()
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

/// A passkey this computer takes commands from.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Manager {
    /// The passkey's credential id (base64url).
    pub cred: String,
    /// Its public key as a raw P-256 point (base64url).
    pub key: String,
    pub added: u64,
}

fn managers() -> Vec<Manager> {
    std::fs::read(dir().join("managers.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_managers(list: &[Manager]) -> Result<()> {
    crate::fleet::ensure_dir()?;
    let path = dir().join("managers.json");
    let tmp = path.with_extension("tmp");
    crate::fleet::write_private(&tmp, &serde_json::to_vec_pretty(list)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Is any passkey pinned, so that actions work?
pub fn managed() -> bool {
    !managers().is_empty()
}

pub fn managers_count() -> usize {
    managers().len()
}

/// Stops taking commands: forgets every pinned passkey.
pub fn forget_managers() -> Result<usize> {
    let n = managers().len();
    let _ = std::fs::remove_file(dir().join("managers.json"));
    let _ = std::fs::remove_file(dir().join("enroll.json"));
    Ok(n)
}

// ---- Pinning a passkey ----

#[derive(Serialize, Deserialize)]
struct Enrollment {
    pin: String,
    expires: u64,
}

/// `glass fleet manage`: a link for the page that pins a passkey, good for 15 minutes.
pub fn start_enrollment() -> Result<String> {
    let mut raw = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut raw).map_err(|_| anyhow::anyhow!("random"))?;
    let pin = b64url(&raw);
    crate::fleet::ensure_dir()?;
    crate::fleet::write_private(&dir().join("enroll.json"), &serde_json::to_vec(&Enrollment { pin: pin.clone(), expires: now() + ENROLL_SECS })?)?;
    let id = HostKey::load_or_create()?.id();
    Ok(format!("{}/computers#manage={id}.{pin}", crate::remote::base().trim_end_matches('/')))
}

/// What the page vouches for: this passkey, for this computer.
fn vouch_message(host_id: &str, cred: &str, spki: &str) -> String {
    format!("glass-manage-v1\n{host_id}\n{cred}\n{spki}")
}

/// The raw point inside a P-256 SubjectPublicKeyInfo.
fn point_from_spki(spki: &[u8]) -> Result<[u8; 65]> {
    ensure!(spki.len() == SPKI_P256.len() + 65 && spki.starts_with(&SPKI_P256) && spki[SPKI_P256.len()] == 4, "not an ES256 passkey");
    spki[SPKI_P256.len()..].try_into().context("key length")
}

/// A passkey asks to be pinned (relayed by glassdesk.dev, vouched for with the link's code).
pub fn enroll(cred: &str, spki_b64: &str, mac_b64: &str) -> Result<String> {
    let path = dir().join("enroll.json");
    let e: Enrollment = serde_json::from_slice(&std::fs::read(&path).context("no enrollment is open: run glass fleet manage")?)?;
    ensure!(e.expires > now(), "that link has expired: run glass fleet manage again");
    ensure!(!cred.is_empty() && cred.len() <= 1024 && b64url_decode(cred).is_some(), "bad credential id");
    let spki = b64url_decode(spki_b64).context("bad key")?;
    let point = point_from_spki(&spki)?;
    let host_id = HostKey::load_or_create()?.id();
    let pin = b64url_decode(&e.pin).context("bad pin")?;
    let mac = b64url_decode(mac_b64).context("bad mac")?;
    hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, &pin), vouch_message(&host_id, cred, spki_b64).as_bytes(), &mac)
        .map_err(|_| anyhow::anyhow!("that wasn't vouched for by this computer's link"))?;
    let mut list = managers();
    ensure!(list.len() < MANAGERS_MAX || list.iter().any(|m| m.cred == cred), "as many passkeys as it can hold: glass fleet manage off first");
    list.retain(|m| m.cred != cred);
    list.push(Manager { cred: cred.into(), key: b64url(&point), added: now() });
    save_managers(&list)?;
    let _ = std::fs::remove_file(path); // one link, one passkey
    audit("enroll", "", cred, true, "passkey pinned");
    // Tell the account it can be managed now, so its page offers the actions at once.
    if crate::fleet::enabled() {
        std::thread::spawn(|| {
            let _ = crate::fleet::send(false);
        });
    }
    Ok("This account can now manage this computer.".into())
}

// ---- Checking a command ----

/// A passkey's answer to "sign this command" (what `navigator.credentials.get` returns).
#[derive(Deserialize, Debug, Clone)]
pub struct Assertion {
    pub id: String,
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    #[serde(rename = "authenticatorData")]
    pub authenticator_data: String,
    pub signature: String,
}

#[derive(Deserialize)]
struct Command {
    v: u8,
    host: String,
    action: String,
    #[serde(default)]
    arg: String,
    nonce: String,
    exp: u64,
}

/// The website's name the passkeys belong to (`glassdesk.dev`, or a self-hosted relay's).
fn rp_id() -> String {
    let base = crate::remote::base();
    let host = base.split("://").nth(1).unwrap_or(&base).split('/').next().unwrap_or("");
    host.split(':').next().unwrap_or(host).to_string()
}

fn origin() -> String {
    crate::remote::base().trim_end_matches('/').to_string()
}

/// Did a pinned passkey sign `cmd` (the exact text) for this website? User verification (a
/// fingerprint, face or PIN) must have happened.
fn verify_assertion(cmd: &str, a: &Assertion, managers: &[Manager], rp_id: &str, origin: &str) -> Result<()> {
    let client = b64url_decode(&a.client_data_json).context("bad clientDataJSON")?;
    let auth = b64url_decode(&a.authenticator_data).context("bad authenticatorData")?;
    let sig = b64url_decode(&a.signature).context("bad signature")?;
    let data: serde_json::Value = serde_json::from_slice(&client).context("clientDataJSON isn't JSON")?;
    ensure!(data["type"] == "webauthn.get", "not a sign-in assertion");
    ensure!(data["challenge"] == b64url(digest::digest(&digest::SHA256, cmd.as_bytes()).as_ref()), "that passkey signed something else");
    ensure!(data["origin"] == origin, "signed for another site");
    ensure!(auth.len() >= 37, "short authenticatorData");
    ensure!(auth[..32] == *digest::digest(&digest::SHA256, rp_id.as_bytes()).as_ref(), "signed for another site");
    ensure!(auth[32] & 0x01 != 0 && auth[32] & 0x04 != 0, "the passkey wasn't verified by you (fingerprint, face or PIN)");
    let manager = managers.iter().find(|m| m.cred == a.id).context("that passkey isn't pinned on this computer")?;
    let point = b64url_decode(&manager.key).context("bad pinned key")?;
    let mut signed = auth.clone();
    signed.extend_from_slice(digest::digest(&digest::SHA256, &client).as_ref());
    signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_ASN1, point)
        .verify(&signed, &sig)
        .map_err(|_| anyhow::anyhow!("bad signature"))
}

/// Commands already run (nonces), so a copy can't run twice.
fn first_use(nonce: &str, exp: u64) -> Result<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = dir().join("nonces.json");
    let mut seen: Vec<(String, u64)> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    seen.retain(|(_, e)| *e > now());
    ensure!(!seen.iter().any(|(n, _)| n == nonce), "that command was already run");
    seen.push((nonce.into(), exp));
    crate::fleet::ensure_dir()?;
    crate::fleet::write_private(&path, &serde_json::to_vec(&seen)?)?;
    Ok(())
}

/// What to do once the answer has gone back (it would cut the connection first).
#[derive(Debug, PartialEq)]
pub enum Deferred {
    RestartGlass,
    RestartComputer,
}

pub fn finish(d: Deferred) {
    let _ = match d {
        Deferred::RestartGlass => crate::power::restart_glass(),
        Deferred::RestartComputer => crate::power::restart_computer(),
    };
}

/// Checks and runs a command. Never fails outward: `(ok, what to tell the page, what's left to do)`.
pub fn command(cmd: &str, a: &Assertion) -> (bool, String, Option<Deferred>) {
    let who = a.id.chars().take(8).collect::<String>();
    match check_and_run(cmd, a) {
        Ok((msg, deferred)) => {
            let action = serde_json::from_str::<Command>(cmd).map(|c| (c.action, c.arg)).unwrap_or_default();
            audit(&action.0, &action.1, &who, true, &msg);
            (true, msg, deferred)
        }
        Err(e) => {
            audit("refused", "", &who, false, &format!("{e:#}"));
            (false, format!("{e:#}"), None)
        }
    }
}

fn check_and_run(cmd: &str, a: &Assertion) -> Result<(String, Option<Deferred>)> {
    ensure!(cmd.len() <= 1024, "command too long");
    let list = managers();
    ensure!(!list.is_empty(), "this computer isn't set up for actions: run glass fleet manage on it");
    let c: Command = serde_json::from_str(cmd).context("not a command")?;
    ensure!(c.v == 1, "unknown command version");
    ensure!(c.host == HostKey::load_or_create()?.id(), "meant for another computer");
    ensure!(c.exp > now() && c.exp <= now() + COMMAND_SECS, "that command has expired");
    verify_assertion(cmd, a, &list, &rp_id(), &origin())?;
    first_use(&c.nonce, c.exp)?;
    run(&c.action, &c.arg)
}

fn run(action: &str, arg: &str) -> Result<(String, Option<Deferred>)> {
    match action {
        "report" => {
            let inv = crate::fleet::send(false)?;
            Ok((format!("Details sent: {} {}.", inv.model, inv.os).replace("  ", " "), None))
        }
        "update_glass" => {
            if crate::testmode::enabled() {
                return Ok(("Updating Glass (test mode).".into(), None));
            }
            crate::update::start_self_update()?;
            Ok(("Updating Glass; it restarts when it's done.".into(), None))
        }
        "restart_glass" => Ok(("Restarting Glass; it's back in a few seconds.".into(), Some(Deferred::RestartGlass))),
        "restart_computer" => Ok(("Restarting the computer; Glass is back once someone is logged in.".into(), Some(Deferred::RestartComputer))),
        "run_script" => Ok((run_script(arg)?, None)),
        _ => bail!("unknown action"),
    }
}

/// Runs `fleet/scripts/<name>`: a plain file you own, executable, not writable by others, in a
/// folder you own that others can't write either (or they could swap it).
fn run_script(name: &str) -> Result<String> {
    run_script_for(name, Duration::from_secs(SCRIPT_SECS))
}

#[cfg(not(unix))]
fn run_script_for(name: &str, limit: Duration) -> Result<String> {
    let _ = (name, limit);
    bail!("fleet scripts aren't available on Windows yet")
}

#[cfg(unix)]
fn run_script_for(name: &str, limit: Duration) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::process::CommandExt;
    ensure!(
        !name.is_empty() && name.len() <= 64 && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)),
        "a script name is letters, digits, dots, dashes and underscores"
    );
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    let scripts = dir().join("scripts");
    let dir_meta = std::fs::symlink_metadata(&scripts).with_context(|| format!("no folder {}", scripts.display()))?;
    ensure!(dir_meta.is_dir(), "{} isn't a plain folder", scripts.display());
    ensure!(dir_meta.uid() == me, "{} isn't yours", scripts.display());
    ensure!(dir_meta.permissions().mode() & 0o022 == 0, "{} can be written by others (chmod go-w)", scripts.display());
    let path = scripts.join(name);
    let meta = std::fs::symlink_metadata(&path).with_context(|| format!("no script {name} in {}", scripts.display()))?;
    ensure!(meta.is_file(), "{name} isn't a plain file");
    let mode = meta.permissions().mode();
    ensure!(mode & 0o111 != 0, "{name} isn't executable (chmod +x)");
    ensure!(mode & 0o022 == 0, "{name} can be written by others (chmod go-w)");
    ensure!(meta.uid() == me, "{name} isn't yours");
    // Its output goes to a file only this user can read, one per run.
    crate::fleet::ensure_dir()?;
    let out_path = dir().join(format!("script-{}-{}.out", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default()));
    crate::fleet::write_private(&out_path, b"")?;
    let out = std::fs::OpenOptions::new().append(true).open(&out_path)?;
    let mut child = std::process::Command::new(&path)
        .current_dir(&scripts)
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        // Its own process group, so a stop takes everything it started.
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting {name}"))?;
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if started.elapsed() > limit {
            // SAFETY: killpg on the group we just created for this child.
            unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut text = String::new();
    std::fs::File::open(&out_path)?.take(1 << 20).read_to_string(&mut text).ok();
    let _ = std::fs::remove_file(&out_path);
    let tail: String = text.chars().rev().take(OUTPUT_MAX).collect::<Vec<_>>().into_iter().rev().collect();
    match status {
        Some(s) if s.success() => Ok(format!("{name} finished.\n{}", tail.trim())),
        Some(s) => bail!("{name} exited with {}.\n{}", s.code().map_or("a signal".into(), |c| c.to_string()), tail.trim()),
        None => bail!("{name} took longer than {} s and was stopped.\n{}", limit.as_secs().max(1), tail.trim()),
    }
}

// ---- The log ----

fn audit(action: &str, arg: &str, who: &str, ok: bool, message: &str) {
    use std::io::Write;
    let line = json!({ "t": now(), "action": action, "arg": arg, "passkey": who, "ok": ok, "message": message.chars().take(200).collect::<String>() });
    if crate::fleet::ensure_dir().is_err() {
        return;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    if let Ok(mut f) = opts.open(dir().join("actions.log")) {
        let _ = writeln!(f, "{line}");
    }
}

/// The last `n` log lines, newest last.
pub fn recent(n: usize) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(dir().join("actions.log")).unwrap_or_default();
    let mut lines: Vec<serde_json::Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let skip = lines.len().saturating_sub(n);
    lines.drain(..skip);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};

    const RP: &str = "glassdesk.dev";
    const ORIGIN: &str = "https://glassdesk.dev";

    struct Passkey {
        pair: EcdsaKeyPair,
        cred: String,
    }

    impl Passkey {
        fn new() -> Self {
            let rng = SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
            let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap();
            Passkey { pair, cred: b64url(b"credential-1") }
        }

        fn manager(&self) -> Manager {
            Manager { cred: self.cred.clone(), key: b64url(self.pair.public_key().as_ref()), added: 0 }
        }

        fn spki(&self) -> String {
            let mut spki = SPKI_P256.to_vec();
            spki.extend_from_slice(self.pair.public_key().as_ref());
            b64url(&spki)
        }

        /// What a browser answers for `cmd`; `flags` are the authenticator's (UP 1, UV 4).
        fn sign(&self, cmd: &str, rp: &str, origin: &str, flags: u8, ty: &str) -> Assertion {
            let client = json!({ "type": ty, "challenge": b64url(digest::digest(&digest::SHA256, cmd.as_bytes()).as_ref()), "origin": origin }).to_string();
            let mut auth = digest::digest(&digest::SHA256, rp.as_bytes()).as_ref().to_vec();
            auth.extend_from_slice(&[flags, 0, 0, 0, 0]);
            let mut signed = auth.clone();
            signed.extend_from_slice(digest::digest(&digest::SHA256, client.as_bytes()).as_ref());
            let sig = self.pair.sign(&SystemRandom::new(), &signed).unwrap();
            Assertion { id: self.cred.clone(), client_data_json: b64url(client.as_bytes()), authenticator_data: b64url(&auth), signature: b64url(sig.as_ref()) }
        }

        fn ok(&self, cmd: &str) -> Assertion {
            self.sign(cmd, RP, ORIGIN, 0x05, "webauthn.get")
        }
    }

    #[test]
    fn a_pinned_passkey_that_you_verified_signs_a_command() {
        let p = Passkey::new();
        assert!(verify_assertion("{\"a\":1}", &p.ok("{\"a\":1}"), &[p.manager()], RP, ORIGIN).is_ok());
    }

    #[test]
    fn everything_else_is_refused() {
        let p = Passkey::new();
        let m = [p.manager()];
        let cmd = "{\"a\":1}";
        // Another command than the one signed.
        assert!(verify_assertion("{\"a\":2}", &p.ok(cmd), &m, RP, ORIGIN).is_err());
        // Another website, in the origin or in the authenticator's rp hash.
        assert!(verify_assertion(cmd, &p.sign(cmd, RP, "https://evil.example", 0x05, "webauthn.get"), &m, RP, ORIGIN).is_err());
        assert!(verify_assertion(cmd, &p.sign(cmd, "evil.example", ORIGIN, 0x05, "webauthn.get"), &m, RP, ORIGIN).is_err());
        // Not verified by the person (no UV), or not even present.
        assert!(verify_assertion(cmd, &p.sign(cmd, RP, ORIGIN, 0x01, "webauthn.get"), &m, RP, ORIGIN).is_err());
        assert!(verify_assertion(cmd, &p.sign(cmd, RP, ORIGIN, 0x04, "webauthn.get"), &m, RP, ORIGIN).is_err());
        // A registration, not a sign-in.
        assert!(verify_assertion(cmd, &p.sign(cmd, RP, ORIGIN, 0x05, "webauthn.create"), &m, RP, ORIGIN).is_err());
        // A passkey that isn't pinned, or someone else's key under a pinned id.
        let stranger = Passkey::new();
        assert!(verify_assertion(cmd, &stranger.ok(cmd), &[], RP, ORIGIN).is_err());
        let mut impostor = stranger.manager();
        impostor.cred = p.cred.clone();
        assert!(verify_assertion(cmd, &p.ok(cmd), &[impostor], RP, ORIGIN).is_err());
        // A flipped bit in the signature.
        let mut bad = p.ok(cmd);
        let mut sig = b64url_decode(&bad.signature).unwrap();
        sig[10] ^= 1;
        bad.signature = b64url(&sig);
        assert!(verify_assertion(cmd, &bad, &m, RP, ORIGIN).is_err());
    }

    #[test]
    fn a_key_is_read_from_a_p256_spki_only() {
        let p = Passkey::new();
        assert_eq!(point_from_spki(&b64url_decode(&p.spki()).unwrap()).unwrap().to_vec(), p.pair.public_key().as_ref().to_vec());
        assert!(point_from_spki(&[0; 91]).is_err());
        assert!(point_from_spki(&b64url_decode(&p.spki()).unwrap()[..90]).is_err());
    }

    #[test]
    fn the_vouch_is_keyed_by_the_links_code() {
        let pin = [7u8; 16];
        let msg = vouch_message("host", "cred", "spki");
        let good = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &pin), msg.as_bytes());
        assert!(hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, &pin), msg.as_bytes(), good.as_ref()).is_ok());
        // Another code, another key, or another computer: no.
        assert!(hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, &[8u8; 16]), msg.as_bytes(), good.as_ref()).is_err());
        assert!(hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, &pin), vouch_message("host", "cred", "other").as_bytes(), good.as_ref()).is_err());
        assert!(hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, &pin), vouch_message("other", "cred", "spki").as_bytes(), good.as_ref()).is_err());
    }

    #[test]
    fn the_website_name_comes_from_the_base() {
        // (rp_id and origin read the live base; their pieces are plain string handling.)
        assert_eq!("https://glassdesk.dev".split("://").nth(1).unwrap().split('/').next().unwrap().split(':').next().unwrap(), "glassdesk.dev");
        assert_eq!("http://localhost:8787".split("://").nth(1).unwrap().split(':').next().unwrap(), "localhost");
    }

    /// A scratch fleet folder (the config dir is process-wide, so one test owns it).
    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("glass-fleet-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: set before any other thread in this test reads it; only this test sets it.
        unsafe { std::env::set_var("GLASS_CONFIG_DIR", &dir) };
        dir
    }

    #[cfg(unix)]
    fn script(dir: &std::path::Path, name: &str, body: &str, mode: u32) {
        let scripts = dir.join("fleet").join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let path = scripts.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn scripts_run_only_from_a_private_folder_as_yours_and_are_stopped_on_time() {
        let dir = scratch();
        script(&dir, "hi.sh", "#!/bin/sh\necho hi there\n", 0o755);
        let scripts = dir.join("fleet").join("scripts");
        std::fs::set_permissions(&scripts, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(run_script("hi.sh").unwrap().contains("hi there"));

        // The folder others can write: a script there could be swapped.
        std::fs::set_permissions(&scripts, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(run_script("hi.sh").unwrap_err().to_string().contains("written by others"));
        std::fs::set_permissions(&scripts, std::fs::Permissions::from_mode(0o755)).unwrap();

        // The script others can write, one that isn't executable, a symlink, a failing one.
        script(&dir, "loose.sh", "#!/bin/sh\necho x\n", 0o777);
        std::fs::set_permissions(scripts.join("loose.sh"), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(run_script("loose.sh").unwrap_err().to_string().contains("written by others"));
        script(&dir, "plain.txt", "echo x\n", 0o644);
        assert!(run_script("plain.txt").unwrap_err().to_string().contains("executable"));
        std::os::unix::fs::symlink("/bin/echo", scripts.join("link.sh")).unwrap();
        assert!(run_script("link.sh").unwrap_err().to_string().contains("plain file"));
        script(&dir, "bad.sh", "#!/bin/sh\necho nope >&2\nexit 3\n", 0o755);
        let e = run_script("bad.sh").unwrap_err().to_string();
        assert!(e.contains("exited with 3") && e.contains("nope"));

        // A script that runs on is stopped, along with what it started.
        script(&dir, "slow.sh", "#!/bin/sh\nsleep 30 &\necho $! > child.pid\nsleep 30\n", 0o755);
        let started = std::time::Instant::now();
        assert!(run_script_for("slow.sh", Duration::from_millis(500)).unwrap_err().to_string().contains("was stopped"));
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(300));
        let pid: i32 = std::fs::read_to_string(scripts.join("child.pid")).unwrap().trim().parse().unwrap();
        // SAFETY: signal 0 only checks that the process exists.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "what the script started is gone too");

        // The fleet folder and what's in it are private.
        script(&dir, "p.sh", "#!/bin/sh\ntrue\n", 0o755);
        run_script("p.sh").unwrap();
        audit("test", "", "abc", true, "x");
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir.join("fleet")), 0o700);
        assert_eq!(mode(dir.join("fleet").join("actions.log")), 0o600);
        assert!(std::fs::read_dir(dir.join("fleet")).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".out")), "no output left behind");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn script_names_are_plain() {
        for bad in ["", "../x", ".hidden", "a/b", "a b", &"x".repeat(65)] {
            assert!(run_script(bad).is_err(), "{bad:?}");
        }
    }
}
