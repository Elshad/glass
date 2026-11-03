//! Windows Defender Firewall: whether it lets Glass accept connections, and the rule that
//! does. Without one, Windows asks the first time the host listens, on the computer's own
//! screen, and turning that down leaves Glass blocked.

use std::path::Path;

use windows::Win32::NetworkManagement::WindowsFirewall::{
    INetFwPolicy2, INetFwRule, NET_FW_ACTION_ALLOW, NET_FW_PROFILE2_PRIVATE, NET_FW_RULE_DIR_IN, NetFwPolicy2,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IDispatch};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Ole::IEnumVARIANT;
use windows::Win32::System::Variant::{VARIANT, VT_DISPATCH, VariantClear};
use windows::core::{HSTRING, Interface};

pub use crate::firewall_bind::Remote;
use crate::firewall_bind::{self, TAILSCALE_V4, TAILSCALE_V6};

/// What the inbound rules say about a program on private networks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Inbound {
    Allowed,
    /// A block rule (one is left behind when someone turns down Windows' prompt). Block
    /// rules win over allow rules.
    Blocked,
    /// No rule: Windows will ask.
    Missing,
}

/// One rule, as much of it as matters here.
#[derive(Debug, Clone)]
pub struct Rule {
    pub program: String,
    pub enabled: bool,
    pub inbound: bool,
    pub allow: bool,
    pub profiles: i32,
    pub name: String,
    pub remote: String,
}

/// Decides from the rules for `exe`; `same` says whether a rule's program is it.
pub fn decide(rules: &[Rule], same: impl Fn(&str) -> bool) -> Inbound {
    let (block, allow) = rules
        .iter()
        .filter(|r| r.enabled && r.inbound && r.profiles & NET_FW_PROFILE2_PRIVATE.0 != 0 && same(&r.program))
        .fold((false, false), |(b, a), r| (b || !r.allow, a || r.allow));
    if block {
        Inbound::Blocked
    } else if allow {
        Inbound::Allowed
    } else {
        Inbound::Missing
    }
}

/// The program paths in rules are often lower-cased (Windows' own prompt does that) or
/// written with variables like %ProgramFiles%.
fn same_program(rule: &str, exe: &Path) -> bool {
    expand(rule).to_lowercase() == crate::browse::shell_path(&exe.to_string_lossy()).to_lowercase()
}

fn expand(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let src = HSTRING::from(s);
    let mut buf = vec![0u16; 1024];
    // SAFETY: buf is a writable slice; the call reports the length it needed.
    let n = unsafe { ExpandEnvironmentStringsW(&src, Some(&mut buf)) } as usize;
    if n == 0 || n > buf.len() {
        return s.to_string();
    }
    String::from_utf16_lossy(&buf[..n - 1])
}

/// Every firewall rule's essentials. `None` if the firewall service can't be asked.
fn rules() -> Option<Vec<Rule>> {
    super::system::with_com(|| {
        // SAFETY: COM is initialised on this thread. Every VARIANT the enumerator fills is
        // cleared after its dispatch pointer is copied (AddRef'd) out.
        unsafe {
            let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER).ok()?;
            let all = policy.Rules().ok()?;
            let list: IEnumVARIANT = all._NewEnum().ok()?.cast().ok()?;
            let mut out = Vec::new();
            loop {
                let mut v = [VARIANT::default()];
                let mut got = 0u32;
                if list.Next(&mut v, &mut got).is_err() || got == 0 {
                    break;
                }
                let inner = &v[0].Anonymous.Anonymous;
                let disp: Option<IDispatch> = if inner.vt == VT_DISPATCH { (*inner.Anonymous.pdispVal).clone() } else { None };
                let _ = VariantClear(&mut v[0]);
                let Some(rule) = disp.and_then(|d| d.cast::<INetFwRule>().ok()) else { continue };
                out.push(Rule {
                    program: rule.ApplicationName().map(|s| s.to_string()).unwrap_or_default(),
                    enabled: rule.Enabled().is_ok_and(|e| e.as_bool()),
                    inbound: rule.Direction().is_ok_and(|d| d == NET_FW_RULE_DIR_IN),
                    allow: rule.Action().is_ok_and(|a| a == NET_FW_ACTION_ALLOW),
                    profiles: rule.Profiles().unwrap_or(0),
                    name: rule.Name().map(|s| s.to_string()).unwrap_or_default(),
                    remote: rule.RemoteAddresses().map(|s| s.to_string()).unwrap_or_default(),
                });
            }
            Some(out)
        }
    })
}

/// Whether `exe` may accept connections on private networks; `None` if unknown.
pub fn inbound(exe: &Path) -> Option<Inbound> {
    Some(decide(&rules()?, |p| same_program(p, exe)))
}

/// The saved listen address. A missing file is the default (any remote address).
/// Anything unreadable is `Err`: the caller must not add a wide rule.
pub fn saved_remote() -> Result<Remote, ()> {
    let bytes = match std::fs::read(crate::settings::path()) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Remote::Any),
        Err(_) => return Err(()),
    };
    let settings: crate::settings::Settings = serde_json::from_slice(&bytes).map_err(|_| ())?;
    firewall_bind::remote_for_bind(&settings.bind)
}

fn program(exe: &Path) -> String {
    crate::browse::shell_path(&exe.to_string_lossy())
}

/// netsh arguments that delete the rule named Glass for `exe`.
pub fn delete_rule_args(exe: &Path) -> String {
    format!("advfirewall firewall delete rule name=\"Glass\" program=\"{}\"", program(exe))
}

/// netsh arguments for Glass's allow, or `None` when loopback needs no allow.
pub fn add_rule_args(exe: &Path, remote: Remote) -> Option<String> {
    let base = format!(
        "advfirewall firewall add rule name=\"Glass\" dir=in action=allow program=\"{}\" enable=yes profile=private",
        program(exe)
    );
    match remote {
        Remote::Loopback => None,
        Remote::Any => Some(base),
        Remote::Tailscale => Some(format!("{base} remoteip=\"{TAILSCALE_V4},{TAILSCALE_V6}\" edge=no")),
    }
}

fn netsh(args: &str) -> bool {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new("netsh");
    cmd.raw_arg(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    super::no_window(&mut cmd);
    cmd.status().is_ok_and(|s| s.success())
}

/// Deletes Glass's rule for `exe`, then adds the one `remote` asks for. Already admin.
/// A missing rule counts as deleted. Success is the firewall then matching `remote`.
fn replace_rule(exe: &Path, remote: Remote) -> bool {
    if !remove_rule_quietly(exe) {
        return false;
    }
    if let Some(add) = add_rule_args(exe, remote)
        && !netsh(&add)
    {
        return false;
    }
    in_sync(exe, remote) == Some(true)
}

/// Replaces Glass's rule from the saved bind. `false` leaves any existing rule as it is.
pub fn add_rule_quietly(exe: &Path) -> bool {
    match saved_remote() {
        Ok(remote) => replace_rule(exe, remote),
        Err(()) => false,
    }
}

/// Removes Glass's rule for `exe`. `true` when no rule of that name remains for it,
/// including when there was nothing to delete.
pub fn remove_rule_quietly(exe: &Path) -> bool {
    let _ = netsh(&delete_rule_args(exe));
    glass_named(exe) == Some(false)
}

/// Any rule named Glass for `exe`, enabled or not. `None` if the firewall can't be listed.
fn glass_named(exe: &Path) -> Option<bool> {
    Some(rules()?.iter().any(|r| r.name.trim().eq_ignore_ascii_case("Glass") && same_program(&r.program, exe)))
}

/// Hidden argument: relaunch this program elevated and run [`apply_from_arg`].
pub const APPLY_ARG: &str = "--apply-firewall-rule";

/// `Start-Process -Verb RunAs` of `exe` with [`APPLY_ARG`]. The path is one PowerShell
/// literal, so spaces, `$` and `%` stay in the path. The child runs [`replace_rule`].
fn elevate_launch(exe: &Path) -> String {
    format!(
        "$ErrorActionPreference='Stop'; $p = Start-Process -FilePath {} -ArgumentList {} -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $p.ExitCode",
        super::ps_string(&exe.display().to_string()),
        super::ps_string(APPLY_ARG),
    )
}

/// Replaces the rule as administrator. Windows asks once. Already elevated: do it here.
pub fn add_rule_elevated(exe: &Path) -> bool {
    if super::service::is_elevated() {
        return add_rule_quietly(exe);
    }
    super::powershell(&elevate_launch(exe)).status().is_ok_and(|s| s.success())
}

/// Entry for [`APPLY_ARG`]. True only when this program's own rule matches the saved bind.
pub fn apply_from_arg() -> bool {
    std::env::current_exe().ok().is_some_and(|exe| super::service::is_elevated() && add_rule_quietly(&exe))
}

/// Remote addresses of enabled private inbound allows named Glass for `exe`.
fn glass_remotes(exe: &Path) -> Option<Vec<String>> {
    let private = NET_FW_PROFILE2_PRIVATE.0;
    Some(rules()?.into_iter().filter(|r| r.enabled && r.inbound && r.allow && r.profiles & private != 0 && r.name.trim().eq_ignore_ascii_case("Glass") && same_program(&r.program, exe)).map(|r| r.remote).collect())
}

/// Whether Glass's own rule for `exe` already matches `remote`.
pub fn in_sync(exe: &Path, remote: Remote) -> Option<bool> {
    let found = glass_remotes(exe)?;
    Some(match remote {
        Remote::Loopback => found.is_empty(),
        Remote::Any | Remote::Tailscale => found.len() == 1 && firewall_bind::stored_matches(remote, found.first().map(String::as_str)),
    })
}

/// What to say after `bind` changes. Nothing when the rule already matches and we are not elevated.
pub fn bind_change_note() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let remote = match saved_remote() {
        Ok(remote) => remote,
        Err(()) => return Some("Couldn't read the saved listen address, so the firewall rule was left unchanged.".into()),
    };
    if super::service::is_elevated() {
        return Some(if replace_rule(&exe, remote) {
            match remote {
                Remote::Tailscale => format!("The firewall rule now allows only {TAILSCALE_V4} and {TAILSCALE_V6}."),
                Remote::Loopback => "The inbound firewall rule was removed; Glass only listens on this computer.".into(),
                Remote::Any => "The firewall rule allows connections on private networks.".into(),
            }
        } else {
            "Couldn't update the Windows Firewall rule.".into()
        });
    }
    if in_sync(&exe, remote) == Some(true) {
        return None;
    }
    Some(match remote {
        Remote::Tailscale => format!("The firewall rule was not changed. Run `glass setup` to allow only {TAILSCALE_V4} and {TAILSCALE_V6}."),
        Remote::Loopback => "The firewall rule was not changed. Run `glass setup` to remove the inbound allow.".into(),
        Remote::Any => "The firewall rule was not changed. Run `glass setup` to allow Glass on private networks.".into(),
    })
}

/// The setup line once the rule matches `remote`.
pub fn ready_sentence(remote: Remote) -> &'static str {
    match remote {
        Remote::Any => "Glass can accept connections on private networks",
        Remote::Tailscale => "Glass accepts connections from Tailscale only",
        Remote::Loopback => "Glass only listens on this computer, so it has no inbound firewall rule",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(program: &str, allow: bool, profiles: i32) -> Rule {
        Rule { program: program.into(), enabled: true, inbound: true, allow, profiles, name: String::new(), remote: String::new() }
    }

    #[test]
    fn block_rules_win_and_only_private_ones_count() {
        let same = |p: &str| p.eq_ignore_ascii_case(r"C:\Glass\glass.exe");
        assert_eq!(decide(&[], same), Inbound::Missing);
        assert_eq!(decide(&[rule(r"c:\glass\glass.exe", true, 2)], same), Inbound::Allowed);
        assert_eq!(decide(&[rule(r"c:\glass\glass.exe", true, 0x7fff_ffff)], same), Inbound::Allowed);
        assert_eq!(decide(&[rule(r"c:\glass\glass.exe", true, 4)], same), Inbound::Missing);
        assert_eq!(decide(&[rule(r"c:\other.exe", true, 2)], same), Inbound::Missing);
        assert_eq!(decide(&[rule(r"c:\glass\glass.exe", true, 2), rule(r"C:\Glass\glass.exe", false, 2)], same), Inbound::Blocked);
        let mut off = rule(r"c:\glass\glass.exe", false, 2);
        off.enabled = false;
        assert_eq!(decide(&[off, rule(r"c:\glass\glass.exe", true, 3)], same), Inbound::Allowed);
    }

    #[test]
    fn the_rule_names_the_program_plainly() {
        let exe = Path::new(r"\\?\C:\Program Files\Glass\glass.exe");
        assert_eq!(
            add_rule_args(exe, Remote::Any).as_deref(),
            Some("advfirewall firewall add rule name=\"Glass\" dir=in action=allow program=\"C:\\Program Files\\Glass\\glass.exe\" enable=yes profile=private")
        );
        let tailscale = add_rule_args(exe, Remote::Tailscale).unwrap();
        assert!(tailscale.contains("remoteip=\"100.64.0.0/10,fd7a:115c:a1e0::/48\""));
        assert!(tailscale.ends_with("edge=no"));
        assert!(add_rule_args(exe, Remote::Loopback).is_none());
    }

    #[test]
    fn elevate_relaunches_this_program_with_the_path_literal() {
        let spaces = elevate_launch(Path::new(r"C:\Program Files\Glass\glass.exe"));
        assert!(spaces.contains(r"-FilePath 'C:\Program Files\Glass\glass.exe'"), "{spaces}");
        assert!(spaces.contains("-ArgumentList '--apply-firewall-rule'"), "{spaces}");
        let odd = elevate_launch(Path::new(r"C:\Glass$Bin\%ProgramFiles%\glass.exe"));
        assert!(odd.contains(r"-FilePath 'C:\Glass$Bin\%ProgramFiles%\glass.exe'"), "{odd}");
        assert!(!odd.contains("cmd.exe") && !odd.contains("netsh"), "{odd}");
    }

    /// Creates the Tailscale rule for this executable and reads back exactly both ranges.
    /// Skipped unless Windows CI sets GLASS_FIREWALL_INTEGRATION=1.
    #[test]
    fn windows_tailscale_rule_is_exactly_both_ranges() {
        if std::env::var("GLASS_FIREWALL_INTEGRATION").ok().as_deref() != Some("1") {
            return;
        }
        assert!(super::super::service::is_elevated(), "Windows CI must run elevated");
        let exe = std::env::current_exe().unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = replace_rule(&self.0, Remote::Loopback);
            }
        }
        let _cleanup = Cleanup(exe.clone());
        assert!(replace_rule(&exe, Remote::Any), "create the wide rule");
        let wide = glass_remotes(&exe).expect("list the wide rule");
        assert_eq!(wide.len(), 1, "wide rules: {wide:?}");
        assert!(firewall_bind::stored_matches(Remote::Any, wide.first().map(String::as_str)), "wide remote was {}", wide[0]);
        assert!(replace_rule(&exe, Remote::Tailscale), "replace the wide rule");
        let found = glass_remotes(&exe).expect("list firewall rules");
        assert_eq!(found.len(), 1, "rules: {found:?}");
        assert_eq!(firewall_bind::normalized_remotes(&found[0]), vec![TAILSCALE_V4.to_string(), TAILSCALE_V6.to_string()], "remote addresses were {}", found[0]);
        assert!(replace_rule(&exe, Remote::Loopback), "remove the rule");
        assert!(glass_remotes(&exe).expect("list firewall rules").is_empty());
    }
}
