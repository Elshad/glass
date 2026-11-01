//! Fleet reports: with `glass fleet on`, this computer tells your glassdesk.dev account what
//! state it's in (model, system, memory, disk, encryption, firewall, uptime, battery, Glass
//! version) every 15 minutes, signed with its own key, so /computers can show it. Off until
//! you turn it on; `glass fleet off` stops and deletes what was sent. Only computers on an
//! account are stored. Nothing here can run anything: that's `actions` (also opt-in).

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use serde_json::json;

use crate::remote::HostKey;

const EVERY: Duration = Duration::from_secs(15 * 60);

pub(crate) fn dir() -> PathBuf {
    crate::config_dir().join("fleet")
}

/// The fleet folder, readable by this user only (it holds the action log, pinned keys and
/// script output).
pub(crate) fn ensure_dir() -> Result<PathBuf> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        builder.mode(0o700).create(dir())?;
        std::fs::set_permissions(dir(), std::fs::Permissions::from_mode(0o700))?;
    }
    // On Windows the folder inherits the profile's ACL, which only this user can read.
    #[cfg(not(unix))]
    builder.create(dir())?;
    Ok(dir())
}

/// Writes a file only this user can read, from the moment it exists.
pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)?.write_all(bytes)
}

pub fn enabled() -> bool {
    dir().join("enabled").exists()
}

pub fn set_enabled(on: bool) -> Result<()> {
    ensure_dir()?;
    let flag = dir().join("enabled");
    if on {
        write_private(&flag, b"")?;
    } else {
        let _ = std::fs::remove_file(flag);
    }
    Ok(())
}

/// What a report says. Everything is optional-ish: a field that can't be read is left out.
#[derive(Serialize, Default, Clone, Debug, PartialEq)]
pub struct Inventory {
    /// "macOS 15.1", or the Linux distribution's name.
    pub os: String,
    /// "MacBook Pro".
    pub model: String,
    /// "Apple M3 Max".
    pub chip: String,
    pub ram_gb: u32,
    pub disk_total_gb: u32,
    pub disk_free_gb: u32,
    /// Disk encryption: FileVault on a Mac. `None` when unknown.
    pub filevault: Option<bool>,
    /// The system firewall. `None` when unknown.
    pub firewall: Option<bool>,
    pub uptime_h: u32,
    /// Percent, when there's a battery.
    pub battery: Option<u8>,
    /// Glass's own version.
    pub version: String,
    /// A passkey is pinned, so this account can run actions here (`glass fleet manage`).
    pub managed: bool,
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// "FileVault is On." → Some(true).
fn parse_filevault(s: &str) -> Option<bool> {
    if s.contains("FileVault is On") {
        Some(true)
    } else if s.contains("FileVault is Off") {
        Some(false)
    } else {
        None
    }
}

/// `defaults read …alf globalstate`: 0 off, 1 or 2 on.
fn parse_firewall(s: &str) -> Option<bool> {
    match s.trim() {
        "0" => Some(false),
        "1" | "2" => Some(true),
        _ => None,
    }
}

/// "…InternalBattery-0 (id=…)	83%; discharging; 5:02 remaining…" → 83.
fn parse_battery(s: &str) -> Option<u8> {
    let i = s.find('%')?;
    let digits: String = s[..i].chars().rev().take_while(char::is_ascii_digit).collect::<Vec<_>>().into_iter().rev().collect();
    digits.parse::<u8>().ok().filter(|p| *p <= 100)
}

/// `df -k /`: (total GB, free GB).
fn parse_df(s: &str) -> (u32, u32) {
    let Some(line) = s.lines().nth(1) else { return (0, 0) };
    let f: Vec<&str> = line.split_whitespace().collect();
    let kb = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    ((kb(1) / 1_048_576) as u32, (kb(3) / 1_048_576) as u32)
}

/// `sysctl kern.boottime` → seconds since the epoch of the boot.
fn parse_boottime(s: &str) -> Option<u64> {
    let rest = s.split("sec =").nth(1)?;
    rest.trim_start().split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// A line like `  Model Name: MacBook Pro`.
fn profile_field(s: &str, name: &str) -> String {
    s.lines()
        .find_map(|l| l.trim().strip_prefix(name).and_then(|r| r.strip_prefix(':')))
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

/// PRETTY_NAME from /etc/os-release.
fn parse_os_release(s: &str) -> String {
    s.lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_else(|| "Linux".into())
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

/// Reads this computer's state. Blocking (it runs a few system tools).
pub fn collect() -> Inventory {
    if std::env::var_os("GLASS_FLEET_FAKE").is_some() {
        return Inventory {
            os: "macOS 15.1".into(),
            model: "MacBook Pro".into(),
            chip: "Apple M3 Max".into(),
            ram_gb: 64,
            disk_total_gb: 994,
            disk_free_gb: 412,
            filevault: Some(true),
            firewall: Some(false),
            uptime_h: 52,
            battery: Some(83),
            version: env!("CARGO_PKG_VERSION").into(),
            managed: crate::actions::managed(),
        };
    }
    let mut inv = Inventory { version: env!("CARGO_PKG_VERSION").into(), managed: crate::actions::managed(), ..Default::default() };
    let (total, free) = run("df", &["-k", "/"]).map(|s| parse_df(&s)).unwrap_or((0, 0));
    inv.disk_total_gb = total;
    inv.disk_free_gb = free;
    #[cfg(target_os = "macos")]
    {
        inv.os = format!("macOS {}", run("sw_vers", &["-productVersion"]).unwrap_or_default());
        let hw = run("system_profiler", &["SPHardwareDataType"]).unwrap_or_default();
        inv.model = profile_field(&hw, "Model Name");
        inv.chip = Some(profile_field(&hw, "Chip")).filter(|c| !c.is_empty()).unwrap_or_else(|| profile_field(&hw, "Processor Name"));
        inv.ram_gb = run("sysctl", &["-n", "hw.memsize"]).and_then(|v| v.parse::<u64>().ok()).map_or(0, |b| (b >> 30) as u32);
        inv.filevault = run("fdesetup", &["status"]).and_then(|s| parse_filevault(&s));
        inv.firewall = run("defaults", &["read", "/Library/Preferences/com.apple.alf", "globalstate"]).and_then(|s| parse_firewall(&s));
        inv.uptime_h = run("sysctl", &["-n", "kern.boottime"]).and_then(|s| parse_boottime(&s)).map_or(0, |boot| (now().saturating_sub(boot) / 3600) as u32);
        inv.battery = run("pmset", &["-g", "batt"]).and_then(|s| parse_battery(&s));
    }
    #[cfg(not(target_os = "macos"))]
    {
        inv.os = parse_os_release(&std::fs::read_to_string("/etc/os-release").unwrap_or_default());
        let dmi = |f: &str| std::fs::read_to_string(format!("/sys/devices/virtual/dmi/id/{f}")).map(|s| s.trim().to_string()).unwrap_or_default();
        inv.model = dmi("product_name");
        inv.chip = std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("model name").and_then(|r| r.split_once(':')).map(|(_, v)| v.trim().to_string())))
            .unwrap_or_default();
        inv.ram_gb = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("MemTotal:").map(|r| r.trim().trim_end_matches("kB").trim().to_string())))
            .and_then(|kb| kb.parse::<u64>().ok())
            .map_or(0, |kb| ((kb + 524_288) / 1_048_576) as u32);
        inv.uptime_h = std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()))
            .map_or(0, |secs| (secs / 3600.0) as u32);
        inv.battery = std::fs::read_dir("/sys/class/power_supply")
            .ok()
            .and_then(|d| d.flatten().find(|e| e.file_name().to_string_lossy().starts_with("BAT")))
            .and_then(|e| std::fs::read_to_string(e.path().join("capacity")).ok())
            .and_then(|c| c.trim().parse::<u8>().ok());
    }
    inv
}

/// Sends this computer's report (or, with `forget`, asks for it to be deleted).
pub fn send(forget: bool) -> Result<Inventory> {
    let inv = collect();
    let data = if forget { String::new() } else { serde_json::to_string(&inv)? };
    let key = HostKey::load_or_create()?;
    let post = |ts: u64| -> Result<()> {
        let sig = key.sign(format!("glass-fleet-v1\n{ts}\n{data}").as_bytes())?;
        crate::account::post("/api/fleet/report", json!({ "key": key.public(), "ts": ts, "data": data, "sig": sig })).map(drop)
    };
    // The website takes a report every few seconds at most: once more if it was too soon.
    if let Err(e) = post(now()) {
        anyhow::ensure!(format!("{e:#}").contains("few seconds"), "{e:#}");
        std::thread::sleep(Duration::from_millis(5500));
        post(now())?;
    }
    let ts = now();
    if !forget {
        if ensure_dir().is_ok() {
            let _ = write_private(&dir().join("last"), ts.to_string().as_bytes());
        }
    }
    Ok(inv)
}

/// Seconds since the last report went out, if one did.
pub fn last_sent() -> Option<u64> {
    std::fs::read_to_string(dir().join("last")).ok()?.trim().parse::<u64>().ok().map(|t| now().saturating_sub(t))
}

/// Reports every 15 minutes while it's on, and soon after it's turned on. A computer that
/// isn't on an account is told so (the report is refused) and simply tries again later.
pub fn watch() {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let due = last_sent().is_none_or(|ago| ago >= EVERY.as_secs());
            if enabled() && due {
                match tokio::task::spawn_blocking(|| send(false)).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => tracing::debug!("fleet report: {e:#}"),
                    Err(_) => {}
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_the_system_tools_say() {
        assert_eq!(parse_filevault("FileVault is On.\n"), Some(true));
        assert_eq!(parse_filevault("FileVault is Off."), Some(false));
        assert_eq!(parse_filevault("something else"), None);
        assert_eq!(parse_firewall("0\n"), Some(false));
        assert_eq!(parse_firewall("2"), Some(true));
        assert_eq!(parse_firewall("?"), None);
        assert_eq!(parse_battery("Now drawing from 'Battery Power'\n -InternalBattery-0 (id=1)\t83%; discharging; 5:02 remaining"), Some(83));
        assert_eq!(parse_battery("Now drawing from 'AC Power'"), None);
        assert_eq!(parse_df("Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3s1s1 1043554304 11000000 432000000 3% /"), (995, 411));
        assert_eq!(parse_boottime("{ sec = 1793291000, usec = 123 } Thu Oct 29 09:00:00 2026"), Some(1_793_291_000));
        assert_eq!(profile_field("      Model Name: MacBook Pro\n      Chip: Apple M3 Max\n", "Chip"), "Apple M3 Max");
        assert_eq!(parse_os_release("NAME=\"Arch\"\nPRETTY_NAME=\"Omarchy\"\n"), "Omarchy");
    }

    #[test]
    fn a_report_carries_only_the_listed_fields() {
        let json = serde_json::to_value(Inventory::default()).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["battery", "chip", "disk_free_gb", "disk_total_gb", "filevault", "firewall", "managed", "model", "os", "ram_gb", "uptime_h", "version"]);
    }
}
