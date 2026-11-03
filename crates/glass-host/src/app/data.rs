//! What the app shows, read the way the CLI reads it: the service's status file and
//! control socket, and the shared config files. Gathered off the main thread; the window
//! only redraws when something actually changed.

use std::time::Duration;

use serde_json::Value;

use crate::{access, acl, agent, connlog, ipc, remote, settings, status, tailscale};

#[derive(Clone, Default, PartialEq)]
pub struct Snapshot {
    /// The running service's status, if it's running.
    pub status: Option<status::Status>,
    /// Installed as a background service (a login item).
    pub installed: bool,
    pub viewers: Vec<Viewer>,
    pub pending: Vec<Pending>,
    pub passes: Vec<Pass>,
    pub settings: settings::Settings,
    /// Which owner-only secrets are set: OIDC client secret, LDAP password, SCIM token.
    pub secrets: [bool; 3],
    pub remote: bool,
    /// Its address from anywhere, while remote access is on.
    pub anywhere_url: Option<String>,
    /// Privacy screen on right now.
    pub privacy: bool,
    pub agent: Agent,
    /// The conversation with viewers, oldest first.
    pub chat: Vec<ChatLine>,
    pub log: Vec<connlog::Entry>,
    pub rules: Vec<acl::Rule>,
    /// The rules come from oidc-allow, not `glass acl`.
    pub rules_implicit: bool,
    pub token: String,
    /// Devices that get notifications (Web Push).
    pub notify: Vec<NotifyDevice>,
    /// The device viewers' microphones play into (Glass Microphone or BlackHole), if installed.
    pub mic: Option<String>,
    /// What an administrator manages (shown, but not editable).
    pub policy: crate::policy::Policy,
}

#[derive(Clone, PartialEq)]
pub struct NotifyDevice {
    pub id: String,
    pub device: String,
    /// "Messages, questions" (what it hears about).
    pub what: String,
}

#[derive(Clone, PartialEq)]
pub struct Viewer {
    pub id: u64,
    pub describe: String,
}

#[derive(Clone, PartialEq)]
pub struct ChatLine {
    pub at: u64,
    pub from: String,
    pub mine: bool,
    pub text: String,
}

#[derive(Clone, PartialEq)]
pub struct Pending {
    pub id: u64,
    pub who: String,
}

#[derive(Clone, PartialEq)]
pub struct Pass {
    pub id: String,
    pub device: String,
    pub scope: String,
    pub expires: u64,
    pub passkey: bool,
}

#[derive(Clone, Default, PartialEq)]
pub struct Agent {
    pub paused: bool,
    pub stopped: bool,
    pub asks: Vec<Ask>,
    /// Agents from other devices that may use this computer.
    pub grants: Vec<GrantRow>,
    /// Agents from other devices may ask at all (`glass agents on|off`).
    pub enabled: bool,
    /// Published to the internet with Tailscale Funnel.
    pub public: bool,
    /// Apps asking for unattended access: (client id, name).
    pub installs: Vec<(String, String)>,
    /// The latest audit log lines, newest first, as text.
    pub recent: Vec<String>,
}

#[derive(Clone, PartialEq)]
pub struct Ask {
    pub id: u64,
    pub help: bool,
    /// An agent asking to be allowed in: pick the number its sign-in page shows.
    pub consent: bool,
    pub text: String,
    pub choices: Vec<String>,
}

#[derive(Clone, PartialEq)]
pub struct GrantRow {
    pub id: String,
    pub name: String,
    pub domain: String,
    pub summary: String,
    pub last_used: u64,
}

impl Snapshot {
    pub fn running(&self) -> bool {
        self.status.is_some()
    }

    pub fn url(&self) -> Option<String> {
        self.status.as_ref().and_then(|s| s.url.clone())
    }

    /// Screen Recording and Accessibility, as far as the running service knows.
    pub fn permissions(&self) -> (bool, bool) {
        self.status.as_ref().map_or((false, false), |s| (s.screen_recording || s.capturing, s.accessibility))
    }
}

/// Everything quick to read. Blocking; call it off the main thread.
pub fn gather() -> Snapshot {
    let status = status::read().filter(|s| s.is_fresh());
    let mut snap = Snapshot {
        installed: agent::is_installed(),
        settings: settings::load(),
        secrets: [
            settings::oidc_secret().is_some(),
            settings::ldap_bind_password().is_some(),
            settings::scim_token().is_some(),
        ],
        remote: remote::enabled(),
        anywhere_url: remote::enabled().then(|| remote::HostKey::load_or_create().ok().map(|k| remote::open_url(&k))).flatten(),
        log: connlog::recent(200).into_iter().rev().collect(),
        token: std::fs::read_to_string(crate::default_token_file()).unwrap_or_default().trim().to_string(),
        mic: crate::mic::device_name(),
        policy: crate::policy::load(),
        notify: crate::push::list()
            .into_iter()
            .map(|s| NotifyDevice {
                what: {
                    let what = [(s.kinds.chat, "messages"), (s.owner && s.kinds.asks, "questions"), (s.owner && s.kinds.activity, "activity")]
                        .into_iter()
                        .filter_map(|(on, k)| on.then_some(k))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let what = if what.is_empty() { "nothing".to_string() } else { what };
                    format!("{}{}", what[..1].to_uppercase(), &what[1..]) + if s.owner { "" } else { " · guest" }
                },
                id: s.id,
                device: s.device,
            })
            .collect(),
        ..Default::default()
    };
    let rules = acl::load();
    snap.rules_implicit = rules.is_empty();
    snap.rules = if rules.is_empty() { acl::effective() } else { rules };
    let now = access::now();
    snap.passes = access::list(&access::file_for(&crate::default_token_file()))
        .into_iter()
        .filter(|p| p.expires > now)
        .map(|p| Pass {
            id: p.id,
            device: if p.device.is_empty() { "A browser".into() } else { p.device },
            scope: p.scope.describe(),
            expires: p.expires,
            passkey: p.passkey,
        })
        .collect();
    if status.is_some() {
        if let Ok(v) = ipc::call(&ipc::Request::Viewers) {
            snap.viewers = list(&v["viewers"])
                .map(|v| Viewer { id: v["id"].as_u64().unwrap_or(0), describe: text(&v["describe"]) })
                .collect();
        }
        if let Ok(v) = ipc::call(&ipc::Request::Pending) {
            snap.pending = list(&v["pending"])
                .map(|p| Pending { id: p["id"].as_u64().unwrap_or(0), who: text(&p["who"]) })
                .collect();
        }
        if let Ok(v) = ipc::call(&ipc::Request::ChatLog) {
            snap.chat = list(&v["messages"])
                .map(|m| ChatLine {
                    at: m["at"].as_u64().unwrap_or(0),
                    from: text(&m["from"]),
                    mine: m["mine"] == true,
                    text: text(&m["text"]),
                })
                .collect();
        }
        // Older services don't know this request.
        if let Ok(v) = ipc::call(&ipc::Request::Agent { op: None }) {
            snap.agent = Agent {
                paused: v["status"]["paused"] == true,
                stopped: v["status"]["stopped"] == true,
                asks: list(&v["asks"])
                    .map(|a| Ask {
                        id: a["id"].as_u64().unwrap_or(0),
                        help: a["kind"] == "help",
                        consent: a["kind"] == "consent",
                        text: text(&a["text"]),
                        choices: list(&a["choices"]).map(text).collect(),
                    })
                    .collect(),
                ..Default::default()
            };
            snap.privacy = v["privacy"] == true;
        }
    }
    let now = crate::grants::now();
    let store = crate::grants::load();
    snap.agent.enabled = store.enabled;
    snap.agent.public = store.public;
    snap.agent.installs = store.clients.iter().filter(|c| c.install_requested && !c.installed).map(|c| (c.id.clone(), c.name.clone())).collect();
    snap.agent.recent = crate::grants::audit_lines(6)
        .iter()
        .rev()
        .map(|l| {
            format!(
                "{} · {} {}{}",
                l["name"].as_str().unwrap_or("Agent"),
                l["action"].as_str().unwrap_or(""),
                l["detail"].as_str().unwrap_or(""),
                if l["ok"] == false { " (refused)" } else { "" }
            )
        })
        .collect();
    snap.agent.grants = store
        .grants
        .into_iter()
        .filter(|g| g.live(now))
        .map(|g| GrantRow { summary: g.summary(), id: g.id, name: g.client_name, domain: g.domain, last_used: g.last_used })
        .collect();
    snap.status = status;
    snap
}

fn list(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// Where this computer stands with Tailscale.
#[derive(Clone, PartialEq)]
pub enum Tailnet {
    Checking,
    Missing,
    NotRunning,
    LoggedOut,
    NoHttps(String),
    Ready(String),
}

impl Tailnet {
    pub fn name(&self) -> Option<&str> {
        match self {
            Tailnet::Ready(n) => Some(n),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct Machine {
    pub name: String,
    pub dns: String,
    pub os: String,
    pub online: bool,
    pub is_self: bool,
    /// Wake-on-LAN addresses seen before.
    pub wakeable: bool,
}

/// The slow part: Tailscale's status and the owner's machines.
pub async fn tailnet() -> (Tailnet, Vec<Machine>) {
    let probe = match tailscale::probe().await {
        tailscale::Probe::Missing => Tailnet::Missing,
        tailscale::Probe::NotRunning => Tailnet::NotRunning,
        tailscale::Probe::LoggedOut => Tailnet::LoggedOut,
        tailscale::Probe::NoHttps { name } => Tailnet::NoHttps(name),
        tailscale::Probe::Ready { name } => Tailnet::Ready(name),
    };
    let machines = if matches!(probe, Tailnet::Ready(_) | Tailnet::NoHttps(_)) {
        let known = crate::wol::known();
        tailscale::machines()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|m| Machine {
                wakeable: known.get(&m.dns).is_some_and(|k| !k.is_empty()),
                name: m.name,
                dns: m.dns,
                os: m.os,
                online: m.online,
                is_self: m.is_self,
            })
            .collect()
    } else {
        Vec::new()
    };
    (probe, machines)
}

/// "in 3 h", "in 6 days".
pub fn until(unix: u64) -> String {
    let left = unix.saturating_sub(access::now());
    format!("{} left", access::describe(Duration::from_secs(left)))
}

/// "Today 14:02", "Mon 9:15", "12 Oct".
pub fn when(unix: u64) -> String {
    let now = access::now();
    let age = now.saturating_sub(unix);
    let ago = Duration::from_secs(age);
    if age < 60 {
        "Just now".into()
    } else if age < 3600 {
        format!("{} min ago", age / 60)
    } else if age < 86_400 {
        format!("{} h ago", age / 3600)
    } else {
        format!("{} ago", access::describe(ago))
    }
}
