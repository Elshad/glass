//! A small status file the running host keeps current, so `glass setup` (a separate
//! process) can show what the background service can and can't do yet. macOS grants
//! Screen Recording and Accessibility per process, so only the host itself knows.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const REFRESH: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Status {
    pub version: String,
    pub pid: u32,
    pub screen_recording: bool,
    pub accessibility: bool,
    /// At least one display is being captured.
    pub capturing: bool,
    /// Where to open the client, if known.
    pub url: Option<String>,
    /// Remote access: "off", "connecting" (to glassdesk.dev) or "ready".
    #[serde(default)]
    pub remote: String,
    /// Unix seconds of the last write; a stale value means the host isn't running.
    pub updated: u64,
    /// Quick support can go through this host: it tags passes with their link and cleans
    /// up after a `glass support` that's gone. Older hosts don't, so support runs its own.
    #[serde(default)]
    pub support_links: bool,
}

pub fn path() -> PathBuf {
    crate::config_dir().join("status.json")
}

pub fn read() -> Option<Status> {
    serde_json::from_slice(&std::fs::read(path()).ok()?).ok()
}

impl Status {
    /// Why this computer can't be reached from anywhere right now (empty: it can).
    pub fn remote_problems(&self) -> Vec<&'static str> {
        let mut why = Vec::new();
        match self.remote.as_str() {
            "ready" => {}
            "connecting" => why.push("still connecting to glassdesk.dev"),
            _ => why.push("remote access is off (glass remote on)"),
        }
        if !self.capturing {
            why.push(if self.screen_recording { "not capturing the screen yet" } else { "Screen Recording isn't allowed" });
        }
        if !self.accessibility {
            why.push("remote mouse and keyboard aren't allowed (accessibility)");
        }
        why
    }

    /// True when written recently by a live process.
    pub fn is_fresh(&self) -> bool {
        unix_now().saturating_sub(self.updated) <= 5
    }
}

static CURRENT: Mutex<Option<Status>> = Mutex::new(None);

/// Starts keeping the status file current. `update` may be called from anywhere after.
pub fn start(url: Option<String>) {
    *CURRENT.lock().unwrap() = Some(Status {
        version: env!("CARGO_PKG_VERSION").into(),
        pid: std::process::id(),
        url,
        support_links: true,
        ..Default::default()
    });
    std::thread::spawn(|| {
        loop {
            let snapshot = CURRENT.lock().unwrap().as_mut().map(|s| {
                s.accessibility = crate::input::has_accessibility();
                s.remote = crate::remote::state().into();
                s.screen_recording = s.capturing || crate::permissions::has_screen_recording();
                s.updated = unix_now();
                s.clone()
            });
            if let Some(s) = snapshot {
                let _ = std::fs::create_dir_all(path().parent().unwrap());
                let tmp = path().with_extension("tmp");
                if std::fs::write(&tmp, serde_json::to_vec_pretty(&s).unwrap()).is_ok() {
                    let _ = std::fs::rename(&tmp, path());
                }
            }
            std::thread::sleep(REFRESH);
        }
    });
}

/// The host's latest status, if it's started (the menu bar reads this).
pub fn current() -> Option<Status> {
    CURRENT.lock().unwrap().clone()
}

pub fn set_capturing(capturing: bool) {
    if let Some(s) = CURRENT.lock().unwrap().as_mut() {
        s.capturing = capturing;
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up() -> Status {
        Status { remote: "ready".into(), capturing: true, screen_recording: true, accessibility: true, ..Default::default() }
    }

    #[test]
    fn ready_when_connected_and_capturing() {
        assert!(up().remote_problems().is_empty());
    }

    #[test]
    fn says_why_it_is_not_ready() {
        assert_eq!(Status { remote: "off".into(), ..up() }.remote_problems(), ["remote access is off (glass remote on)"]);
        assert_eq!(Status { remote: "connecting".into(), ..up() }.remote_problems(), ["still connecting to glassdesk.dev"]);
        assert_eq!(Status { capturing: false, ..up() }.remote_problems(), ["not capturing the screen yet"]);
        assert_eq!(Status { capturing: false, screen_recording: false, ..up() }.remote_problems(), ["Screen Recording isn't allowed"]);
        assert_eq!(Status { accessibility: false, ..up() }.remote_problems().len(), 1);
    }

    #[test]
    fn an_old_status_file_without_remote_still_reads() {
        let s: Status = serde_json::from_str(r#"{"version":"0.8.2","pid":1,"screen_recording":true,"accessibility":true,"capturing":true,"url":null,"updated":0}"#).unwrap();
        assert_eq!(s.remote, "");
        assert_eq!(s.remote_problems(), ["remote access is off (glass remote on)"]);
    }
}
