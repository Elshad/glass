//! Connection history: one JSON line per session (and per refused attempt) in
//! `connections.jsonl` next to the token, for `glass log` and the menu bar.

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// Unix seconds.
    pub start: u64,
    /// Seconds connected (0 for refused attempts).
    #[serde(default)]
    pub secs: u64,
    pub device: String,
    /// tailnet, token, guest, or refused.
    pub how: String,
    #[serde(default)]
    pub remote: bool,
    pub peer: String,
    /// The guest's scope, or why it was refused.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// Lines kept; older ones are trimmed when the file grows past twice this.
const KEEP: usize = 2000;

pub fn path() -> PathBuf {
    crate::config_dir().join("connections.jsonl")
}

pub fn append(entry: &Entry) {
    let p = path();
    let write = || -> std::io::Result<()> {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
        writeln!(f, "{}", serde_json::to_string(entry)?)?;
        Ok(())
    };
    if let Err(e) = write() {
        tracing::warn!("connection log: {e}");
    }
    trim(&p);
}

fn trim(p: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(p) else { return };
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > KEEP * 2 {
        let kept = lines[lines.len() - KEEP..].join("\n") + "\n";
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, kept).is_ok() {
            let _ = std::fs::rename(&tmp, p);
        }
    }
}

/// The most recent `n` entries, oldest first.
pub fn recent(n: usize) -> Vec<Entry> {
    let text = std::fs::read_to_string(path()).unwrap_or_default();
    let all: Vec<Entry> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    all[all.len().saturating_sub(n)..].to_vec()
}

/// "2026-10-22 14:03  12 min  Safari on iPad  guest (view only)  remote from 203.0.113.9"
pub fn describe(e: &Entry) -> String {
    let when = format_time(e.start);
    let dur = match e.secs {
        _ if e.how == "refused" => "—".to_string(),
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s => format!("{} h {} min", s / 3600, s % 3600 / 60),
    };
    let how = if e.detail.is_empty() {
        e.how.clone()
    } else {
        format!("{} ({})", e.how, e.detail)
    };
    let from = if e.remote {
        format!("remote from {}", e.peer)
    } else {
        e.peer.clone()
    };
    format!("{when}  {dur:>9}  {}  {how}  {from}", e.device)
}

/// Local date and time, without pulling in a time zone database: Windows itself does it.
#[cfg(windows)]
fn format_time(unix: u64) -> String {
    crate::platform::system::local_time(unix).unwrap_or_else(|| unix.to_string())
}

/// Local date and time, without pulling in a time zone database: `date` does it.
#[cfg(not(windows))]
fn format_time(unix: u64) -> String {
    let flag = if cfg!(target_os = "macos") { "-r" } else { "-d" };
    let arg = if cfg!(target_os = "macos") {
        unix.to_string()
    } else {
        format!("@{unix}")
    };
    std::process::Command::new("date")
        .args([flag, &arg, "+%Y-%m-%d %H:%M"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| unix.to_string())
}

/// Logs a session when dropped (at its end), with how long it lasted.
pub struct Session {
    pub entry: Entry,
    pub started: std::time::Instant,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.entry.secs = self.started.elapsed().as_secs();
        append(&self.entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_entries() {
        let e = Entry {
            start: 0,
            secs: 754,
            device: "Safari on iPad".into(),
            how: "guest".into(),
            remote: true,
            peer: "203.0.113.9".into(),
            detail: "view only".into(),
        };
        let line = describe(&e);
        assert!(line.contains("12 min"), "{line}");
        assert!(line.contains("guest (view only)"));
        assert!(line.ends_with("remote from 203.0.113.9"));
        let refused = Entry {
            how: "refused".into(),
            remote: false,
            ..e
        };
        assert!(describe(&refused).contains("—"));
    }
}
