//! Magic links: one-time, short-lived codes that let a device in without the access
//! token. Opening one trades it for a guest pass, a per-device credential that works
//! until it expires or is revoked. Only SHA-256 fingerprints are stored, in a file next
//! to the token, so the CLI and the running host share it and changes apply at once.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// How long an unopened link stays valid.
pub const LINK_TTL: Duration = Duration::from_secs(15 * 60);
/// Bounds for how long the access a link grants lasts.
pub const MIN_GRANT: Duration = Duration::from_secs(5 * 60);
pub const MAX_GRANT: Duration = Duration::from_secs(30 * 24 * 3600);

/// Serializes read-modify-write within this process (the host's sessions).
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    links: Vec<Link>,
    #[serde(default)]
    passes: Vec<Pass>,
}

/// What a device let in by a link may do. Old access files (no scope) mean full guest
/// access, as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// Watch only: no mouse, keyboard or gamepad.
    #[serde(default)]
    pub view_only: bool,
    /// Send and get files.
    #[serde(default = "yes")]
    pub files: bool,
    /// Copy and paste both ways.
    #[serde(default = "yes")]
    pub clipboard: bool,
    /// The person at the computer must allow each connection.
    #[serde(default)]
    pub ask: bool,
    /// The device must register a passkey when it opens the link, and use it every time.
    #[serde(default)]
    pub passkey: bool,
}

fn yes() -> bool {
    true
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            view_only: false,
            files: true,
            clipboard: true,
            ask: false,
            passkey: false,
        }
    }
}

impl Scope {
    /// "view only, no files" or "full control".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.view_only {
            parts.push("view only");
        }
        if !self.files {
            parts.push("no files");
        }
        if !self.clipboard {
            parts.push("no clipboard");
        }
        if self.ask {
            parts.push("asks first");
        }
        if self.passkey {
            parts.push("passkey");
        }
        if parts.is_empty() {
            "full control".into()
        } else {
            parts.join(", ")
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Link {
    hash: String,
    expires: u64,
    grant_secs: u64,
    #[serde(default)]
    scope: Scope,
    /// Set when the link is first opened. Until the link expires, the same device can open it
    /// again in another browser (see [`redeem_link`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    used: Option<Used>,
}

/// A link's first use: who opened it, so only the same device can reopen it.
#[derive(Serialize, Deserialize, Clone)]
struct Used {
    /// The network it was opened from (see [`network_key`]).
    network: String,
    /// The operating system ("iPhone", "Windows"…): the part of the device name after "on".
    os: String,
    /// When the first pass ends. Later browsers share it: reopening never extends access.
    pass_expires: u64,
    /// Browsers let in so far.
    devices: u8,
}

/// Browsers one link can let in (the first, and others of the same device within its time).
const LINK_DEVICES: u8 = 3;

/// The network an address belongs to: the address itself, or for IPv6 its /64 (a device's
/// privacy addresses change inside it).
fn network_key(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
    }
}

/// "Chrome on iPhone" → "iPhone".
fn os_of(device: &str) -> String {
    device.rsplit_once(" on ").map_or(device, |(_, os)| os).trim().to_lowercase()
}

#[derive(Serialize, Deserialize)]
struct Pass {
    hash: String,
    expires: u64,
    #[serde(default)]
    scope: Scope,
    /// When the link was opened (unix seconds).
    #[serde(default)]
    created: u64,
    /// What opened it, e.g. "Safari on iPad".
    #[serde(default)]
    device: String,
    /// A passkey bound to this pass (SPKI DER, base64url), once registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    passkey: Option<Passkey>,
    /// The directory account (email) it was minted for by single sign-on or LDAP, so
    /// deprovisioning that account (SCIM) revokes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<String>,
}

/// A WebAuthn credential (ES256) bound to a guest pass.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Passkey {
    /// Credential id, base64url.
    pub id: String,
    /// Uncompressed P-256 point (65 bytes), base64url.
    pub public_key: String,
    /// The signature counter last seen (0 when the authenticator doesn't count).
    #[serde(default)]
    pub counter: u32,
}

/// A guest pass, as listed by `glass link --list` (never the secret itself).
#[derive(Debug, Clone, Serialize)]
pub struct PassInfo {
    /// Short id for `glass link --revoke <id>`: the start of the pass's fingerprint.
    pub id: String,
    pub expires: u64,
    pub created: u64,
    pub device: String,
    pub scope: Scope,
    pub passkey: bool,
}

/// A link to hand out: the secret code and when it stops working if unopened.
pub struct NewLink {
    pub code: String,
    pub expires: u64,
    pub grant: Duration,
    pub scope: Scope,
}

/// A guest pass for the device that opened a link.
pub struct Granted {
    pub pass: String,
    pub expires: u64,
    pub scope: Scope,
}

/// A valid pass: when it ends, what it allows, and its passkey if one is bound.
#[derive(Debug, Clone)]
pub struct Valid {
    pub id: String,
    pub expires: u64,
    pub scope: Scope,
    pub passkey: Option<Passkey>,
    /// Let in by a link (not minted for a directory account by single sign-on or LDAP).
    pub from_link: bool,
}

/// The access file that sits next to the token file.
pub fn file_for(token_file: &Path) -> PathBuf {
    token_file.with_file_name("access.json")
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A guest link (`glass link`), unless a policy turned them off.
pub fn create_link(path: &Path, grant: Duration, scope: Scope) -> Result<NewLink> {
    crate::policy::load().require("links")?;
    create_link_for(path, LINK_TTL, grant, scope)
}

/// A link that stays usable for `ttl` (quick support waits up to an hour for the helper).
pub fn create_link_for(path: &Path, ttl: Duration, grant: Duration, scope: Scope) -> Result<NewLink> {
    let grant = grant.clamp(MIN_GRANT, MAX_GRANT);
    let code = random_code()?;
    let expires = now() + ttl.as_secs();
    update(path, |store| {
        store.links.push(Link {
            hash: fingerprint(&code),
            expires,
            grant_secs: grant.as_secs(),
            scope,
            used: None,
        });
    })?;
    Ok(NewLink {
        code,
        expires,
        grant,
        scope,
    })
}

/// A guest pass issued directly (single sign-on), without a link.
/// A pass for a directory account (SSO, LDAP): revoked with [`revoke_account`].
pub fn grant_pass(path: &Path, grant: Duration, scope: Scope, device: &str) -> Result<Granted> {
    grant_pass_for(path, grant, scope, device, Some(device))
}

pub fn grant_pass_for(
    path: &Path,
    grant: Duration,
    scope: Scope,
    device: &str,
    account: Option<&str>,
) -> Result<Granted> {
    let grant = grant.clamp(MIN_GRANT, MAX_GRANT);
    let pass = random_code()?;
    let expires = now() + grant.as_secs();
    update(path, |store| {
        store.passes.push(Pass {
            hash: fingerprint(&pass),
            expires,
            scope,
            created: now(),
            device: device.to_string(),
            passkey: None,
            account: account.map(|a| a.to_ascii_lowercase()),
        });
    })?;
    Ok(Granted {
        pass,
        expires,
        scope,
    })
}

/// Removes every pass minted for `account`; returns how many.
pub fn revoke_account(path: &Path, account: &str) -> Result<usize> {
    let account = account.to_ascii_lowercase();
    let mut n = 0;
    update(path, |store| {
        let before = store.passes.len();
        store
            .passes
            .retain(|p| p.account.as_deref() != Some(account.as_str()));
        n = before - store.passes.len();
    })?;
    Ok(n)
}

/// Opens a link, if it's valid, and returns a new guest pass.
///
/// The first browser gets a pass for the link's grant time. Until the link expires, the same
/// device (the same network and operating system) can open it again in another browser, up to
/// [`LINK_DEVICES`] in all, and gets a pass that ends when the first one does. Anyone else,
/// and a link that needs a passkey (which belongs to one device), finds it used.
pub fn redeem_link(path: &Path, code: &str, device: &str, ip: std::net::IpAddr) -> Result<Option<Granted>> {
    let hash = fingerprint(code);
    let pass = random_code()?;
    let mut granted = None;
    update(path, |store| {
        let Some(i) = store
            .links
            .iter()
            .position(|l| crate::control::token_matches(&l.hash, &hash))
        else {
            return;
        };
        let now = now();
        let link = &mut store.links[i];
        let expires = match &mut link.used {
            None => {
                let expires = now + link.grant_secs;
                link.used = Some(Used {
                    network: network_key(ip),
                    os: os_of(device),
                    pass_expires: expires,
                    devices: 1,
                });
                expires
            }
            Some(used) => {
                let again = !link.scope.passkey
                    && used.devices < LINK_DEVICES
                    && used.pass_expires > now
                    && used.network == network_key(ip)
                    && used.os == os_of(device);
                if !again {
                    return;
                }
                used.devices += 1;
                used.pass_expires
            }
        };
        let scope = link.scope;
        store.passes.push(Pass {
            hash: fingerprint(&pass),
            expires,
            scope,
            created: now,
            device: device.to_string(),
            passkey: None,
            account: None,
        });
        granted = Some(Granted {
            pass: pass.clone(),
            expires,
            scope,
        });
    })?;
    Ok(granted)
}

/// When the pass stops working, if it's valid now.
pub fn pass_expiry(path: &Path, pass: &str) -> Option<u64> {
    check_pass(path, pass).map(|v| v.expires)
}

/// The pass's details, if it's valid now.
pub fn check_pass(path: &Path, pass: &str) -> Option<Valid> {
    let hash = fingerprint(pass);
    let now = now();
    load(path)
        .passes
        .iter()
        .find(|p| p.expires > now && crate::control::token_matches(&p.hash, &hash))
        .map(|p| Valid {
            id: short_id(&p.hash),
            expires: p.expires,
            scope: p.scope,
            passkey: p.passkey.clone(),
            from_link: p.account.is_none(),
        })
}

/// Binds a passkey to a pass (only once: a pass that has one keeps it).
pub fn bind_passkey(path: &Path, pass: &str, key: Passkey) -> Result<bool> {
    let hash = fingerprint(pass);
    let mut bound = false;
    update(path, |store| {
        if let Some(p) = store
            .passes
            .iter_mut()
            .find(|p| crate::control::token_matches(&p.hash, &hash))
            && p.passkey.is_none()
        {
            p.passkey = Some(key);
            bound = true;
        }
    })?;
    Ok(bound)
}

/// Records a passkey's new signature counter; false if it went backwards (a cloned key).
pub fn advance_passkey_counter(path: &Path, pass: &str, counter: u32) -> Result<bool> {
    let hash = fingerprint(pass);
    let mut ok = false;
    update(path, |store| {
        if let Some(k) = store
            .passes
            .iter_mut()
            .find(|p| crate::control::token_matches(&p.hash, &hash))
            .and_then(|p| p.passkey.as_mut())
        {
            // Authenticators that don't count always report 0.
            ok = counter == 0 && k.counter == 0 || counter > k.counter;
            if ok {
                k.counter = counter;
            }
        }
    })?;
    Ok(ok)
}

/// Every valid guest pass.
pub fn list(path: &Path) -> Vec<PassInfo> {
    let now = now();
    load(path)
        .passes
        .iter()
        .filter(|p| p.expires > now)
        .map(|p| PassInfo {
            id: short_id(&p.hash),
            expires: p.expires,
            created: p.created,
            device: p.device.clone(),
            scope: p.scope,
            passkey: p.passkey.is_some(),
        })
        .collect()
}

/// Removes the pass whose id starts with `id`; false if none (or several) match.
pub fn revoke(path: &Path, id: &str) -> Result<bool> {
    let id = id.trim().to_ascii_lowercase();
    if id.len() < 4 {
        return Ok(false);
    }
    let mut removed = false;
    update(path, |store| {
        let matches: Vec<usize> = (0..store.passes.len())
            .filter(|&i| short_id(&store.passes[i].hash).starts_with(&id))
            .collect();
        if let [i] = matches[..] {
            store.passes.remove(i);
            removed = true;
        }
    })?;
    Ok(removed)
}

fn short_id(hash: &str) -> String {
    hash.chars().take(8).collect()
}

/// Removes every link and pass; returns how many devices lose access.
pub fn revoke_all(path: &Path) -> Result<usize> {
    let mut passes = 0;
    update(path, |store| {
        passes = store.passes.len();
        *store = Store::default();
    })?;
    Ok(passes)
}

fn load(path: &Path) -> Store {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Loads, drops whatever has expired, applies `f`, and saves atomically with mode 0600.
fn update(path: &Path, f: impl FnOnce(&mut Store)) -> Result<()> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = load(path);
    let now = now();
    store.links.retain(|l| l.expires > now);
    store.passes.retain(|p| p.expires > now);
    f(&mut store);
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut file = opts.open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(&store)?)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn random_code() -> Result<String> {
    let mut bytes = [0u8; 16];
    crate::control::getrandom(&mut bytes)?;
    Ok(hex(&bytes))
}

fn fingerprint(secret: &str) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, secret.as_bytes()).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses "30m", "24h", "7d", "2w" (or plain seconds).
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("expected a duration like 30m, 24h or 7d"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        "w" => n * 7 * 86400,
        _ => anyhow::bail!("expected a duration like 30m, 24h or 7d"),
    };
    Ok(Duration::from_secs(secs))
}

/// "24 hours", "7 days", "30 minutes".
pub fn describe(d: Duration) -> String {
    let s = d.as_secs();
    let (n, unit) = if s >= 86400 && s.is_multiple_of(86400) {
        (s / 86400, "day")
    } else if s >= 3600 {
        ((s + 1800) / 3600, "hour")
    } else {
        (s.div_ceil(60), "minute")
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "glass-access-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("access.json")
    }

    #[test]
    fn a_link_works_once_and_becomes_a_pass() {
        let path = temp();
        let link = create_link(&path, Duration::from_secs(86400), Scope::default()).unwrap();
        let granted = redeem_link(&path, &link.code, "Safari on iPad", ip())
            .unwrap()
            .expect("valid link");
        assert!(
            redeem_link(&path, &link.code, "x", ip()).unwrap().is_none(),
            "single use"
        );
        assert_eq!(pass_expiry(&path, &granted.pass), Some(granted.expires));
        assert!(pass_expiry(&path, "not-a-pass").is_none());
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains(&granted.pass),
            "only fingerprints are stored"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(revoke_all(&path).unwrap(), 1);
        assert!(pass_expiry(&path, &granted.pass).is_none());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn scopes_follow_the_link_and_passes_revoke_one_at_a_time() {
        let path = temp().with_file_name("scoped.json");
        let scope = Scope {
            view_only: true,
            files: false,
            ..Scope::default()
        };
        let a = create_link(&path, Duration::from_secs(3600), scope).unwrap();
        let b = create_link(&path, Duration::from_secs(3600), Scope::default()).unwrap();
        let ga = redeem_link(&path, &a.code, "Chrome on Windows", ip()).unwrap().unwrap();
        let gb = redeem_link(&path, &b.code, "Safari on iPad", ip()).unwrap().unwrap();
        assert_eq!(ga.scope, scope);
        assert_eq!(check_pass(&path, &ga.pass).unwrap().scope, scope);
        let listed = list(&path);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].device, "Chrome on Windows");
        assert_eq!(scope.describe(), "view only, no files");
        assert_eq!(Scope::default().describe(), "full control");
        assert!(!revoke(&path, "zz").unwrap(), "too short to be safe");
        assert!(revoke(&path, &listed[0].id).unwrap());
        assert!(check_pass(&path, &ga.pass).is_none());
        assert!(check_pass(&path, &gb.pass).is_some());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn deprovisioning_an_account_revokes_its_passes() {
        let path = temp().with_file_name("accounts.json");
        let a = grant_pass(&path, Duration::from_secs(3600), Scope::default(), "Ana@Example.com").unwrap();
        let b = grant_pass(&path, Duration::from_secs(3600), Scope::default(), "bob@example.com").unwrap();
        assert_eq!(revoke_account(&path, "ana@example.com").unwrap(), 1);
        assert!(check_pass(&path, &a.pass).is_none());
        assert!(check_pass(&path, &b.pass).is_some());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn old_access_files_mean_full_access() {
        let s: Scope = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Scope::default());
    }

    #[test]
    fn passkeys_bind_once_and_counters_only_go_up() {
        let path = temp().with_file_name("passkey.json");
        let link = create_link(&path, Duration::from_secs(3600), Scope::default()).unwrap();
        let g = redeem_link(&path, &link.code, "x", ip()).unwrap().unwrap();
        let key = Passkey {
            id: "abc".into(),
            public_key: "def".into(),
            counter: 0,
        };
        assert!(bind_passkey(&path, &g.pass, key.clone()).unwrap());
        assert!(!bind_passkey(&path, &g.pass, key.clone()).unwrap(), "only once");
        assert_eq!(check_pass(&path, &g.pass).unwrap().passkey, Some(key));
        assert!(advance_passkey_counter(&path, &g.pass, 5).unwrap());
        assert!(!advance_passkey_counter(&path, &g.pass, 5).unwrap(), "replayed");
        assert!(!advance_passkey_counter(&path, &g.pass, 0).unwrap(), "went back");
        assert!(advance_passkey_counter(&path, &g.pass, 6).unwrap());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(
            parse_duration("7d").unwrap(),
            Duration::from_secs(7 * 86400)
        );
        assert!(parse_duration("soon").is_err());
        assert_eq!(describe(Duration::from_secs(86400)), "1 day");
        assert_eq!(describe(Duration::from_secs(7200)), "2 hours");
        assert_eq!(describe(Duration::from_secs(900)), "15 minutes");
        assert_eq!(describe(Duration::from_secs(86399)), "24 hours");
    }

    fn ip() -> std::net::IpAddr {
        "203.0.113.7".parse().unwrap()
    }

    #[test]
    fn the_same_device_can_open_a_link_in_another_browser() {
        let path = temp();
        let link = create_link(&path, Duration::from_secs(3600), Scope::default()).unwrap();
        let first = redeem_link(&path, &link.code, "Safari on iPhone", ip()).unwrap().unwrap();
        // Another browser on the same phone and network, while the link is still good.
        let second = redeem_link(&path, &link.code, "Chrome on iPhone", ip()).unwrap().unwrap();
        assert_ne!(first.pass, second.pass);
        // It ends when the first pass does: reopening never extends access.
        assert_eq!(first.expires, second.expires);
        assert!(check_pass(&path, &first.pass).is_some() && check_pass(&path, &second.pass).is_some());
        assert_eq!(list(&path).len(), 2);
    }

    #[test]
    fn another_network_or_device_finds_the_link_used() {
        let path = temp();
        let link = create_link(&path, Duration::from_secs(3600), Scope::default()).unwrap();
        redeem_link(&path, &link.code, "Safari on iPhone", ip()).unwrap().unwrap();
        let elsewhere: std::net::IpAddr = "198.51.100.9".parse().unwrap();
        assert!(redeem_link(&path, &link.code, "Chrome on iPhone", elsewhere).unwrap().is_none());
        assert!(redeem_link(&path, &link.code, "Chrome on Windows", ip()).unwrap().is_none());
        assert_eq!(list(&path).len(), 1);
    }

    #[test]
    fn a_link_lets_in_a_few_browsers_at_most() {
        let path = temp();
        let link = create_link(&path, Duration::from_secs(3600), Scope::default()).unwrap();
        for _ in 0..LINK_DEVICES {
            assert!(redeem_link(&path, &link.code, "Safari on iPhone", ip()).unwrap().is_some());
        }
        assert!(redeem_link(&path, &link.code, "Safari on iPhone", ip()).unwrap().is_none());
    }

    #[test]
    fn a_passkey_link_belongs_to_one_device() {
        let path = temp();
        let scope = Scope { passkey: true, ..Default::default() };
        let link = create_link(&path, Duration::from_secs(3600), scope).unwrap();
        redeem_link(&path, &link.code, "Safari on iPhone", ip()).unwrap().unwrap();
        assert!(redeem_link(&path, &link.code, "Chrome on iPhone", ip()).unwrap().is_none());
    }

    #[test]
    fn an_ipv6_device_keeps_its_link_across_privacy_addresses() {
        let path = temp();
        let link = create_link(&path, Duration::from_secs(3600), Scope::default()).unwrap();
        let a: std::net::IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: std::net::IpAddr = "2001:db8:1:2:bbbb::2".parse().unwrap();
        let other: std::net::IpAddr = "2001:db8:1:3::1".parse().unwrap();
        redeem_link(&path, &link.code, "Safari on iPhone", a).unwrap().unwrap();
        assert!(redeem_link(&path, &link.code, "Chrome on iPhone", other).unwrap().is_none());
        assert!(redeem_link(&path, &link.code, "Chrome on iPhone", b).unwrap().is_some());
    }
}
