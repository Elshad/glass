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

/// netsh's arguments for a rule letting `exe` accept connections on private networks
/// (home, work, and Tailscale, which marks its network private), not public ones.
pub fn add_rule_args(exe: &Path) -> String {
    format!(
        "advfirewall firewall add rule name=\"Glass\" dir=in action=allow program=\"{}\" enable=yes profile=private",
        crate::browse::shell_path(&exe.to_string_lossy())
    )
}

/// Adds the rule without asking: works only when already running as administrator.
pub fn add_rule_quietly(exe: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new("netsh");
    cmd.raw_arg(add_rule_args(exe)).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    super::no_window(&mut cmd);
    cmd.status().is_ok_and(|s| s.success())
}

/// Adds the rule as administrator: Windows shows its approval prompt once.
pub fn add_rule_elevated(exe: &Path) -> bool {
    let script = format!(
        "$ErrorActionPreference = 'Stop'; $p = Start-Process -FilePath netsh -ArgumentList {} -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $p.ExitCode",
        super::ps_string(&add_rule_args(exe))
    );
    super::powershell(&script).status().is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(program: &str, allow: bool, profiles: i32) -> Rule {
        Rule { program: program.into(), enabled: true, inbound: true, allow, profiles }
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
        assert_eq!(
            add_rule_args(Path::new(r"\\?\C:\Program Files\Glass\glass.exe")),
            "advfirewall firewall add rule name=\"Glass\" dir=in action=allow program=\"C:\\Program Files\\Glass\\glass.exe\" enable=yes profile=private"
        );
    }
}
