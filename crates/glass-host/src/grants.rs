//! Agents from anywhere: who may use this computer, and how much. A grant is one agent's
//! standing permission, approved once by the owner; its tokens are short-lived and checked
//! here on every request, so revoking takes effect at once. The host is the only
//! authority: nothing elsewhere (glassdesk.dev, the relay, the agent's platform) can make
//! or widen a grant.
//!
//! Kept in `agents.json` (mode 600) under an exclusive lock, so `glass agents` (another
//! process) and the host never undo each other's changes. Only hashes of tokens and
//! secrets are stored. Every action an agent takes goes to `agent-audit.jsonl`, each line
//! chained to the one before by its hash.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Access tokens last this long; refreshing makes new ones without asking anyone.
pub const ACCESS_SECS: u64 = 600;
/// A grant nobody has used this long ends; the agent asks again.
pub const IDLE_SECS: u64 = 30 * 24 * 3600;
/// And every grant ends after this long, used or not.
pub const MAX_SECS: u64 = 180 * 24 * 3600;
/// Actions an agent may take in an hour before it has to ask the owner.
pub const HOURLY_BUDGET: u32 = 1500;
/// A just-rotated refresh token used again this soon is a retry, refused but not theft.
const REUSE_GRACE_SECS: u64 = 30;
/// Rotated-out refresh tokens remembered, to catch one being used twice (a stolen copy).
const RETIRED_KEPT: usize = 32;

pub const VIEW: &str = "computer:view";
pub const USE: &str = "computer:use";
pub const SCOPES: [&str; 2] = [VIEW, USE];

/// Apps an agent never drives, whatever its grant: passwords and keychains, security and
/// privacy settings, the system's authentication prompts, terminals (a shell can undo any
/// limit), and Glass itself (its windows approve agents).
pub const PROTECTED_APPS: &[&str] = &[
    "com.apple.keychainaccess",
    "com.apple.Passwords",
    "com.apple.systempreferences",
    "com.apple.Settings",
    "com.apple.SecurityAgent",
    "com.apple.loginwindow",
    "com.apple.LocalAuthentication.UIAgent",
    "com.apple.Terminal",
    "com.googlecode.iterm2",
    "dev.warp.Warp-Stable",
    "com.mitchellh.ghostty",
    "net.kovidgoyal.kitty",
    "io.alacritty",
    "co.zeit.hyper",
    "com.1password.1password",
    "com.agilebits.onepassword7",
    "com.bitwarden.desktop",
    "com.lastpass.LastPass",
    "com.dashlane.dashlanephonefinal",
    "com.keepassxc.keepassxc",
    "com.apple.ScreenSharing",
    "dev.glassdesk.app",
    // Windows, by executable (see platform::convert::app_id).
    "cmd",
    "powershell",
    "powershell_ise",
    "pwsh",
    "windowsterminal",
    "openconsole",
    "conhost",
    "wsl",
    "wslhost",
    "bash",
    "regedit",
    "mmc",
    "taskmgr",
    "systemsettings",
    "sechealthui",
    "consent",
    "credentialuibroker",
    "logonui",
    "lockapp",
    "mstsc",
    "1password",
    "bitwarden",
    "keepass",
    "keepassxc",
    "glass",
    "glassw",
    "glass-host",
];

/// System UI that takes input without being an app's window: an agent limited to some
/// apps can't use these (Spotlight would open anything).
pub const SYSTEM_UI: &[&str] = &[
    "com.apple.Spotlight",
    "com.apple.controlcenter",
    "com.apple.notificationcenterui",
    "com.apple.dock",
    "com.apple.systemuiserver",
    "com.apple.WindowManager",
    // Windows: the taskbar, desktop and Start, search and notifications.
    "explorer",
    "startmenuexperiencehost",
    "searchhost",
    "searchapp",
    "shellexperiencehost",
    "shellhost",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    /// Off: agents from other devices are refused (`glass agents off`).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Published to the internet with Tailscale Funnel (`glass agents public on`).
    #[serde(default)]
    pub public: bool,
    #[serde(default)]
    pub clients: Vec<Client>,
    #[serde(default)]
    pub grants: Vec<Grant>,
}

fn yes() -> bool {
    true
}

/// A registered client (dynamic registration). Clients identified by a metadata
/// document URL aren't stored: the document is fetched when needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Client {
    pub id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
    /// `none`, `client_secret_basic`, `client_secret_post` or `private_key_jwt`.
    pub auth_method: String,
    #[serde(default)]
    pub grant_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_uri: Option<String>,
    /// For RFC 7592 updates.
    pub registration_hash: String,
    pub created: u64,
    /// Allowed to use client credentials (the `app` subject) on this computer, after the
    /// owner ran `glass agents install`.
    #[serde(default)]
    pub installed: bool,
    /// Asked for client credentials before being installed: listed for the owner.
    #[serde(default)]
    pub install_requested: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub id: String,
    pub client_id: String,
    /// The client's name, as it gave it (decoration only).
    pub client_name: String,
    /// What the owner was shown as the agent's identity: its verified domain.
    pub domain: String,
    /// `user` (approved by the owner) or `app` (installed by the owner).
    pub subject: String,
    pub scopes: Vec<String>,
    /// Bundle ids it may drive; any app (but the protected ones) when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<Vec<String>>,
    pub created: u64,
    pub last_used: u64,
    /// Who approved it: "this computer", or the device that answered.
    pub approved_by: String,
    #[serde(default)]
    pub revoked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<String>,
    #[serde(default)]
    pub retired: Vec<String>,
    /// When the refresh token last rotated: the one before may still arrive for a moment
    /// (a retry, a response lost on the way), which isn't theft.
    #[serde(default)]
    pub rotated: u64,
    #[serde(default)]
    pub access: Vec<Access>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Access {
    pub hash: String,
    pub expires: u64,
}

impl Grant {
    pub fn live(&self, now: u64) -> bool {
        !self.revoked
            && now < self.created + MAX_SECS
            && now < self.last_used.max(self.created) + IDLE_SECS
    }

    pub fn has(&self, scope: &str) -> bool {
        // Using the computer includes seeing it.
        self.scopes.iter().any(|s| s == scope || (scope == VIEW && s == USE))
    }

    /// What it may do, for a person to read.
    pub fn summary(&self) -> String {
        let what = if self.has(USE) { "see and use" } else { "see" };
        match &self.apps {
            Some(apps) => format!("{what} this computer, only in {}", apps.join(", ")),
            None => format!("{what} this computer"),
        }
    }
}

// ---- Storage ----

pub fn path() -> PathBuf {
    crate::config_dir().join("agents.json")
}

fn audit_path() -> PathBuf {
    crate::config_dir().join("agent-audit.jsonl")
}

/// The store, with `enabled` as a policy says if one does (update() keeps the owner's).
pub fn load() -> Store {
    let mut store = read().unwrap_or_else(|e| {
        tracing::warn!("agents: can't read {}: {e:#}", path().display());
        // Unreadable: nothing is allowed (an empty store), and update() won't overwrite it.
        Store { enabled: false, ..Default::default() }
    });
    if let Some(on) = crate::policy::load().flag("agents") {
        store.enabled = on;
    }
    store
}

fn read() -> Result<Store> {
    match std::fs::read(path()) {
        Ok(b) => Ok(serde_json::from_slice(&b)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store { enabled: true, ..Default::default() }),
        Err(e) => Err(e.into()),
    }
}

/// Reads, changes and writes the store under an exclusive lock (shared with `glass agents`).
pub fn update<R>(f: impl FnOnce(&mut Store) -> R) -> Result<R> {
    let dir = crate::config_dir();
    std::fs::create_dir_all(&dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("agents.lock"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a file we hold open; released when it closes.
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    }
    // A store that can't be read is kept as it is for the owner to look at, never replaced.
    let mut store = read().with_context(|| format!("{} is damaged; move it aside to start over", path().display()))?;
    let before = serde_json::to_vec(&store)?;
    let out = f(&mut store);
    if serde_json::to_vec(&store)? == before {
        return Ok(out);
    }
    let tmp = path().with_extension("json.tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        file.write_all(&serde_json::to_vec_pretty(&store)?)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path())?;
    drop(lock);
    Ok(out)
}

// ---- Secrets ----

pub fn random(prefix: &str) -> String {
    let mut b = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b).expect("random");
    format!("{prefix}{}", crate::remote::b64url(&b))
}

pub fn hash(secret: &str) -> String {
    crate::remote::b64url(ring::digest::digest(&ring::digest::SHA256, secret.as_bytes()).as_ref())
}

/// PKCE S256: the verifier hashes to the challenge.
pub fn pkce_ok(verifier: &str, challenge: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier.bytes().all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c))
        && crate::control::token_matches(&hash(verifier), challenge)
}

pub fn now() -> u64 {
    crate::access::now()
}

// ---- Tokens ----

pub struct Issued {
    pub access: String,
    pub refresh: Option<String>,
}

/// New tokens for a grant: an access token, and (for `user` grants) the next refresh token,
/// retiring the one before.
fn issue(g: &mut Grant, now: u64) -> Issued {
    let access = random("gla_");
    g.access.retain(|a| a.expires > now);
    g.access.push(Access { hash: hash(&access), expires: now + ACCESS_SECS });
    // The newest are the ones in use.
    let extra = g.access.len().saturating_sub(64);
    g.access.drain(..extra);
    g.last_used = now;
    let refresh = (g.subject == "user").then(|| {
        let r = random("glr_");
        if let Some(old) = g.refresh.replace(hash(&r)) {
            g.retired.push(old);
            g.rotated = now;
            let extra = g.retired.len().saturating_sub(RETIRED_KEPT);
            g.retired.drain(..extra);
        }
        r
    });
    Issued { access, refresh }
}

/// A new grant, approved by the owner; its first tokens.
pub fn create(mut g: Grant) -> Result<(Grant, Issued)> {
    let now = now();
    let issued = issue(&mut g, now);
    let out = g.clone();
    let replaced = update(|s| {
        s.grants.retain(|x| x.live(now));
        // Approved again (or for more): this grant replaces the agent's earlier one.
        let mut replaced = Vec::new();
        for x in s.grants.iter_mut().filter(|x| x.client_id == g.client_id && x.subject == g.subject) {
            x.revoked = true;
            x.refresh = None;
            x.access.clear();
            replaced.push(x.clone());
        }
        s.grants.retain(|x| !x.revoked);
        s.grants.push(g);
        replaced
    })?;
    for x in &replaced {
        audit(x, "revoked", "replaced by a new approval", None, true);
    }
    audit(&out, "granted", &out.summary(), None, true);
    Ok((out, issued))
}

pub enum Refreshed {
    Ok(Grant, Issued),
    /// Unknown, expired or revoked.
    Invalid,
    /// A refresh token used twice: someone else has a copy. The grant is revoked.
    Reused(Grant),
}

pub fn refresh(token: &str, client_id: &str) -> Result<Refreshed> {
    let h = hash(token);
    let now = now();
    let out = update(|s| {
        let Some(g) = s.grants.iter_mut().find(|g| g.client_id == client_id
            && (g.refresh.as_deref() == Some(h.as_str()) || g.retired.contains(&h)))
        else {
            return Refreshed::Invalid;
        };
        if g.retired.contains(&h) {
            if g.retired.last() == Some(&h) && now.saturating_sub(g.rotated) < REUSE_GRACE_SECS {
                return Refreshed::Invalid;
            }
            g.revoked = true;
            g.refresh = None;
            g.access.clear();
            return Refreshed::Reused(g.clone());
        }
        if !g.live(now) {
            return Refreshed::Invalid;
        }
        let issued = issue(g, now);
        Refreshed::Ok(g.clone(), issued)
    })?;
    if let Refreshed::Reused(g) = &out {
        audit(g, "revoked", "a refresh token was used twice", None, false);
    }
    Ok(out)
}

/// Client credentials for an installed client: an access token (no refresh).
pub fn app_token(client: &Client, domain: &str) -> Result<(Grant, Issued)> {
    let now = now();
    let existing = update(|s| {
        let g = s.grants.iter_mut().find(|g| g.client_id == client.id && g.subject == "app" && g.live(now))?;
        let issued = issue(g, now);
        Some((g.clone(), issued))
    })?;
    if let Some(found) = existing {
        return Ok(found);
    }
    create(Grant {
        id: random("grant_")[..20].to_string(),
        client_id: client.id.clone(),
        client_name: client.name.clone(),
        domain: domain.to_string(),
        subject: "app".into(),
        scopes: SCOPES.iter().map(|s| s.to_string()).collect(),
        apps: None,
        created: now,
        last_used: now,
        approved_by: "installed on this computer".into(),
        revoked: false,
        refresh: None,
        retired: Vec::new(),
        rotated: 0,
        access: Vec::new(),
    })
}

/// Agents from other devices on or off. Off also stops publishing them with Funnel, since
/// nothing there would answer; grants are kept for when they're back on.
pub fn set_enabled(on: bool) -> Result<()> {
    crate::policy::load().check("agents", if on { "on" } else { "off" })?;
    let published = update(|s| {
        s.enabled = on;
        s.public
    })?;
    if !on && published {
        crate::oauth::publish(false)?;
    }
    Ok(())
}

/// The live grant an access token belongs to.
pub fn by_access(token: &str) -> Option<Grant> {
    let store = load();
    if !store.enabled {
        return None;
    }
    let h = hash(token);
    let now = now();
    store
        .grants
        .into_iter()
        .find(|g| g.access.iter().any(|a| a.expires > now && crate::control::token_matches(&a.hash, &h)))
        .filter(|g| g.live(now))
}

/// RFC 7009: a refresh token ends its grant; an access token ends just itself.
pub fn revoke_token(token: &str, client_id: &str) -> Result<()> {
    let h = hash(token);
    let ended = update(|s| {
        // Any of the grant's tokens ends all of it: Vercel Connect revokes just the access
        // token and forgets the connection, which would leave a live grant no one holds.
        for g in s.grants.iter_mut().filter(|g| g.client_id == client_id && !g.revoked) {
            if g.refresh.as_deref() == Some(h.as_str()) || g.retired.contains(&h) || g.access.iter().any(|a| a.hash == h) {
                g.revoked = true;
                g.refresh = None;
                g.access.clear();
                return Some(g.clone());
            }
        }
        None
    })?;
    // An app's token revoked by its platform: it's uninstalled too.
    if let Some(g) = ended.as_ref().filter(|g| g.subject == "app") {
        let g = g.clone();
        update(|s| uninstall_apps(s, std::slice::from_ref(&g)))?;
    }
    if let Some(g) = ended {
        audit(&g, "revoked", "by the agent's platform", None, true);
    }
    Ok(())
}

/// Ends grants by id (or all of them, for `None`); how many ended.
pub fn revoke(id: Option<&str>) -> Result<usize> {
    revoke_because(id, "by the owner")
}

/// Ends grants by id (or all), saying why in the audit log.
pub fn revoke_because(id: Option<&str>, why: &str) -> Result<usize> {
    let ended = update(|s| {
        let mut ended = Vec::new();
        for g in s.grants.iter_mut().filter(|g| !g.revoked && id.is_none_or(|id| g.id == id)) {
            g.revoked = true;
            g.refresh = None;
            g.access.clear();
            ended.push(g.clone());
        }
        uninstall_apps(s, &ended);
        ended
    })?;
    for g in &ended {
        audit(g, "revoked", why, None, true);
    }
    Ok(ended.len())
}

/// An app's grant ended: it's no longer installed, so it can't simply get a new one.
fn uninstall_apps(s: &mut Store, ended: &[Grant]) {
    for g in ended.iter().filter(|g| g.subject == "app") {
        if let Some(c) = s.clients.iter_mut().find(|c| c.id == g.client_id) {
            c.installed = false;
        }
    }
}

// ---- Audit ----

/// One line per action, chained: each line's `hash` covers the line before's. The chain's
/// end is read from the file under a lock each time, since the host, the CLI and the Mac
/// app all write here.
pub fn audit(g: &Grant, action: &str, detail: &str, app: Option<&str>, ok: bool) {
    let result = (|| -> Result<()> {
        use std::io::{Seek, SeekFrom};
        let mut f = std::fs::OpenOptions::new().create(true).read(true).append(true).open(audit_path())?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: flock on a file we hold open; released when it closes.
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
        }
        let mut len = f.seek(SeekFrom::End(0))?;
        if len > AUDIT_ROTATE {
            // Rotated: the old lines move to .1 and the chain carries on from them.
            let prev = last_hash(&mut f, len)?;
            std::fs::rename(audit_path(), audit_path().with_extension("1.jsonl"))?;
            f = std::fs::OpenOptions::new().create(true).read(true).append(true).open(audit_path())?;
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                // SAFETY: as above.
                unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
            }
            let mut entry = json!({ "at": now(), "action": "rotated", "prev": prev });
            entry["hash"] = json!(hash(&entry.to_string()));
            writeln!(f, "{entry}")?;
            len = f.seek(SeekFrom::End(0))?;
        }
        let prev = last_hash(&mut f, len)?;
        let mut entry = json!({
            "at": now(), "grant": g.id, "agent": g.domain, "name": g.client_name,
            "action": action, "detail": detail, "app": app, "ok": ok, "prev": prev,
        });
        entry["hash"] = json!(hash(&entry.to_string()));
        writeln!(f, "{entry}")?;
        Ok(())
    })();
    if let Err(e) = result {
        tracing::warn!("agents: audit log: {e:#}");
    }
}

/// Checks the audit log's chain; the number of good lines, or where it breaks.
/// The audit log rolls over to `agent-audit.1.jsonl` at this size.
const AUDIT_ROTATE: u64 = 8 * 1024 * 1024;

/// The hash on the last line of an open audit file of length `len`.
fn last_hash(f: &mut std::fs::File, len: u64) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    f.seek(SeekFrom::Start(len.saturating_sub(16 * 1024)))?;
    let mut tail = String::new();
    f.read_to_string(&mut tail)?;
    Ok(tail
        .lines()
        .last()
        .and_then(|l| serde_json::from_str::<Value>(l).ok())
        .and_then(|v| v["hash"].as_str().map(str::to_string))
        .unwrap_or_default())
}

pub fn verify_audit() -> Result<usize> {
    let old = std::fs::read_to_string(audit_path().with_extension("1.jsonl")).unwrap_or_default();
    let text = old + &std::fs::read_to_string(audit_path()).unwrap_or_default();
    // The oldest kept line may follow lines rotated away long ago.
    let mut prev = text
        .lines()
        .next()
        .and_then(|l| serde_json::from_str::<Value>(l).ok())
        .and_then(|v| v["prev"].as_str().map(str::to_string))
        .unwrap_or_default();
    for (i, line) in text.lines().enumerate() {
        let mut v: Value = serde_json::from_str(line).with_context(|| format!("line {}", i + 1))?;
        let claimed = v["hash"].as_str().unwrap_or_default().to_string();
        anyhow::ensure!(v["prev"].as_str() == Some(prev.as_str()), "line {} doesn't follow the one before", i + 1);
        v.as_object_mut().map(|o| o.remove("hash"));
        anyhow::ensure!(hash(&v.to_string()) == claimed, "line {} was changed", i + 1);
        prev = claimed;
    }
    Ok(text.lines().count())
}

pub fn audit_lines(n: usize) -> Vec<Value> {
    use std::io::{Read, Seek, SeekFrom};
    // Just the end of the file.
    let mut text = String::new();
    if let Ok(mut f) = std::fs::File::open(audit_path()) {
        let len = f.seek(SeekFrom::End(0)).unwrap_or(0);
        let from = len.saturating_sub(n as u64 * 1024);
        let _ = f.seek(SeekFrom::Start(from));
        let _ = f.read_to_string(&mut text);
        if from > 0 {
            text = text.split_once('\n').map(|(_, rest)| rest.to_string()).unwrap_or_default();
        }
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.contains("\"action\":\"rotated\"")).collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

// ---- glass agents ----

fn ago(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

pub fn cli(action: &str, id: Option<&str>, json_out: bool) -> Result<()> {
    let now = now();
    match action {
        "revoke" => {
            let id = id.context("which grant? `glass agents` lists them (or `all`)")?;
            let n = revoke((id != "all").then_some(id))?;
            anyhow::ensure!(n > 0, "no live grant {id}");
            println!("Revoked {n} grant{}.", if n == 1 { "" } else { "s" });
        }
        "install" => {
            let id = id.context("which client? `glass agents` lists those asking")?;
            let name = update(|s| {
                let c = s.clients.iter_mut().find(|c| c.id == id && c.auth_method != "none")?;
                c.installed = true;
                c.install_requested = false;
                Some(c.name.clone())
            })?
            .with_context(|| format!("no client {id} that can authenticate"))?;
            println!(
                "{name} ({id}) can now use this computer unattended: see and use it, no app limits. Its name is its own claim; Glass hasn't verified who runs it. Undo: glass agents revoke <grant>"
            );
        }
        "on" | "off" => {
            set_enabled(action == "on")?;
            if action == "off" {
                println!("Agents from other devices are refused until `glass agents on`, and nothing is published with Funnel. Grants are kept.");
            } else {
                println!("Agents can ask to use this computer again.");
            }
        }
        "log" => {
            for l in audit_lines(50) {
                if json_out {
                    println!("{l}");
                    continue;
                }
                let at = l["at"].as_u64().unwrap_or(0);
                println!(
                    "{:>4} ago  {:<20} {:<16} {}{}{}",
                    ago(now.saturating_sub(at)),
                    l["name"].as_str().unwrap_or(""),
                    l["action"].as_str().unwrap_or(""),
                    l["detail"].as_str().unwrap_or(""),
                    l["app"].as_str().map(|a| format!("  [{a}]")).unwrap_or_default(),
                    if l["ok"] == false { "  ✗" } else { "" },
                );
            }
        }
        "verify" => println!("The audit log is intact ({} entries).", verify_audit()?),
        "public" => match id {
            Some("on") => {
                let url = crate::oauth::publish(true)?.unwrap_or_default();
                println!("Agents anywhere can now ask to use this computer at:\n  {url}\nEach one still needs your approval. Stop with: glass agents public off");
            }
            Some("off") => {
                crate::oauth::publish(false)?;
                println!("Agents can only reach this computer from your tailnet now.");
            }
            _ => anyhow::bail!("glass agents public on|off"),
        },
        _ => {
            let store = load();
            let live: Vec<&Grant> = store.grants.iter().filter(|g| g.live(now)).collect();
            let asking: Vec<&Client> = store.clients.iter().filter(|c| c.install_requested && !c.installed).collect();
            if json_out {
                println!("{}", serde_json::to_string_pretty(&json!({
                    "enabled": store.enabled,
                    "grants": live.iter().map(|g| json!({ "id": g.id, "agent": g.client_name, "domain": g.domain,
                        "subject": g.subject, "scopes": g.scopes, "apps": g.apps, "created": g.created,
                        "last_used": g.last_used, "approved_by": g.approved_by })).collect::<Vec<_>>(),
                    "install_requests": asking.iter().map(|c| json!({ "client_id": c.id, "name": c.name })).collect::<Vec<_>>(),
                }))?);
                return Ok(());
            }
            if !store.enabled {
                println!("Agents from other devices are off (glass agents on).");
            }
            if let Some(url) = crate::status::read().and_then(|s| s.url) {
                println!("Agents on your tailnet connect at {}/mcp", url.trim_end_matches('/'));
                if store.public
                    && let Some(host) = url.split("://").nth(1).and_then(|h| h.split([':', '/']).next())
                {
                    println!("Agents anywhere connect at https://{host}:{}/mcp", crate::oauth::FUNNEL_PORT);
                } else {
                    println!("To let agents outside your tailnet ask (Vercel, claude.ai): glass agents public on");
                }
            }
            if live.is_empty() {
                println!("No agents can use this computer.");
            }
            for g in live {
                println!(
                    "{}  {} ({}): {}; approved by {}, last used {} ago",
                    g.id,
                    g.client_name,
                    g.domain,
                    g.summary(),
                    g.approved_by,
                    ago(now.saturating_sub(g.last_used)),
                );
            }
            for c in asking {
                println!(
                    "{} asks for unattended access (registered {} ago; name not verified): glass agents install {}",
                    c.name,
                    ago(now.saturating_sub(c.created)),
                    c.id
                );
            }
        }
    }
    Ok(())
}

// ---- What an agent may do right now ----

/// Who's acting: the owner's own tools (`glass mcp` with this computer's token), or an
/// agent under a grant.
#[derive(Debug, Clone)]
pub enum Actor {
    Owner,
    Agent(Grant),
}

tokio::task_local! {
    pub static ACTOR: Actor;
}

pub fn current() -> Actor {
    ACTOR.try_with(Actor::clone).unwrap_or(Actor::Owner)
}

/// Where input is about to go: a point (global, in points; pixels on Windows) for the
/// pointer, wherever the keyboard is, or an app's element pressed through Accessibility
/// (by process id).
#[derive(Debug, Clone, Copy)]
pub enum Target {
    Point(f64, f64),
    Keys,
    App(i32),
}

/// Before input reaches the computer: an agent under a grant drives only allowed apps (as
/// Accessibility sees what's actually under the pointer or has the keyboard), and never
/// the protected ones or a password field.
pub fn check_input(target: Target) -> Result<(), String> {
    let Actor::Agent(g) = current() else { return Ok(()) };
    // Revoked (or stopped) while this action waited: nothing more goes through.
    let now = now();
    let store = load();
    if !store.enabled || !store.grants.iter().any(|x| x.id == g.id && x.live(now)) {
        return Err("This agent's access was revoked.".into());
    }
    if matches!(target, Target::Keys) && secure_input() {
        return Err("A password field has the keyboard (secure input is on). Ask the person to type it with request_help.".into());
    }
    let app = target_app(target);
    if let Some(app) = &app
        && PROTECTED_APPS.contains(&app.as_str())
    {
        return Err(format!("{app} is protected: agents can't use it. Ask the person with request_help."));
    }
    if let Some(apps) = &g.apps {
        match &app {
            Some(app) if apps.iter().any(|a| a == app) => {}
            Some(app) if SYSTEM_UI.contains(&app.as_str()) => {
                return Err(format!("This agent may only use {}; that input would go to {app}, which could open anything.", apps.join(", ")));
            }
            Some(app) => return Err(format!("This agent may only use {}; that input would go to {app}.", apps.join(", "))),
            None => return Err("Can't tell which app the input would reach, and this agent is limited to some apps.".into()),
        }
    }
    Ok(())
}

/// The app input to `target` would reach (macOS bundle id, or Windows executable name).
pub fn target_app(target: Target) -> Option<String> {
    if crate::testmode::enabled() {
        return std::env::var("GLASS_TEST_FRONTMOST").ok();
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        match target {
            Target::Point(x, y) => crate::platform::front::app_at(x, y),
            Target::Keys => crate::platform::front::key_app(),
            Target::App(pid) => crate::platform::front::bundle_of(pid),
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = target;
        None
    }
}

/// The app in front (macOS bundle id, or Windows executable name), for the audit log.
pub fn frontmost_app() -> Option<String> {
    if crate::testmode::enabled() {
        return std::env::var("GLASS_TEST_FRONTMOST").ok();
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        crate::platform::front::frontmost_app()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    None
}

/// Glass can tell which app input reaches (and keep agents out of protected ones) here.
pub fn app_guard_available() -> bool {
    cfg!(any(target_os = "macos", windows)) || crate::testmode::enabled()
}

/// Someone used this computer's own keyboard or mouse in the last few seconds.
pub fn local_input_recent() -> bool {
    if crate::testmode::enabled() {
        return std::env::var_os("GLASS_TEST_LOCAL_INPUT").is_some();
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        crate::platform::front::local_input_age().is_some_and(|age| age < 5.0)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    false
}

/// A password field has the keyboard (macOS secure input; on Windows, as the focused
/// control says), or, on Linux, the session is locked (there's no telling which field has
/// the keyboard, but a lock screen's is one).
pub fn secure_input() -> bool {
    if crate::testmode::enabled() {
        return std::env::var_os("GLASS_TEST_SECURE_INPUT").is_some();
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        crate::platform::front::secure_input()
    }
    #[cfg(target_os = "linux")]
    {
        crate::platform::session::locked()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    false
}

static SPENT: std::sync::Mutex<Option<std::collections::HashMap<String, (u64, u32)>>> = std::sync::Mutex::new(None);

pub enum Spend {
    Ok,
    /// Just went over: tell the owner (once).
    JustOver,
    Over,
}

/// Counts an action against the grant's hour.
pub fn spend(g: &Grant) -> Spend {
    let hour = now() / 3600;
    let mut spent = SPENT.lock().unwrap();
    let entry = spent.get_or_insert_with(Default::default).entry(g.id.clone()).or_insert((hour, 0));
    if entry.0 != hour {
        *entry = (hour, 0);
    }
    entry.1 = entry.1.saturating_add(1);
    match entry.1 {
        n if n <= HOURLY_BUDGET => Spend::Ok,
        n if n == HOURLY_BUDGET + 1 => Spend::JustOver,
        _ => Spend::Over,
    }
}

/// The owner resumed agents: every agent gets a fresh hour.
pub fn reset_budgets() {
    SPENT.lock().unwrap().take();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant() -> Grant {
        Grant {
            id: "g1".into(),
            client_id: "c1".into(),
            client_name: "Test".into(),
            domain: "agent.example".into(),
            subject: "user".into(),
            scopes: vec![VIEW.into()],
            apps: None,
            created: 1000,
            last_used: 1000,
            approved_by: "test".into(),
            revoked: false,
            refresh: None,
            retired: Vec::new(),
            rotated: 0,
            access: Vec::new(),
        }
    }

    #[test]
    fn pkce_s256() {
        // base64url(sha256(verifier)), as computed independently.
        assert!(pkce_ok("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWEOEjXk", "TP3DZQl9rJnri9eRuZaFS6Q7czCrth7sEZC00LreApo"));
        assert!(!pkce_ok("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWEOEjXX", "TP3DZQl9rJnri9eRuZaFS6Q7czCrth7sEZC00LreApo"));
        assert!(!pkce_ok("short", &hash("short")));
    }

    #[test]
    fn refresh_tokens_rotate_and_retire() {
        let mut g = grant();
        let first = issue(&mut g, 2000).refresh.unwrap();
        let second = issue(&mut g, 2001).refresh.unwrap();
        assert_eq!(g.refresh.as_deref(), Some(hash(&second).as_str()));
        assert!(g.retired.contains(&hash(&first)));
        assert_eq!(g.access.len(), 2);
    }

    #[test]
    fn grants_end_when_idle_or_old() {
        let g = grant();
        assert!(g.live(1000 + IDLE_SECS - 1));
        assert!(!g.live(1000 + IDLE_SECS));
        let mut busy = grant();
        busy.last_used = 1000 + MAX_SECS - 10;
        assert!(!busy.live(1000 + MAX_SECS));
        let mut revoked = grant();
        revoked.revoked = true;
        assert!(!revoked.live(1001));
    }

    #[test]
    fn app_tokens_have_no_refresh() {
        let mut g = grant();
        g.subject = "app".into();
        assert!(issue(&mut g, 2000).refresh.is_none());
    }
}
