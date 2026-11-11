//! Staying awake with the lid closed, so a laptop closed on a desk can still be unlocked
//! and used from a viewer, only while it's on power (closed in a bag on battery, it sleeps).
//!
//! macOS: closing the lid sleeps a Mac unless a real display is attached (a virtual one
//! doesn't count), and only `pmset disablesleep`, as root, stops that. Turning `lid-awake`
//! on installs a sudoers rule that allows exactly that toggle, once, with the admin
//! password; the host turns sleep off on power and back on on battery.
//!
//! Linux: logind suspends on the lid whatever the host's sleep inhibitor says
//! (LidSwitchIgnoreInhibited), so on power the host also holds a `handle-lid-switch`
//! inhibitor. Desktops that handle the lid themselves (GNOME, KDE) may still suspend
//! unless their own lid setting says otherwise.
//!
//! Windows: the power plan says what closing the lid does, separately on power and on
//! battery. While `lid-awake` is on and the laptop is plugged in, the host sets the active
//! plan's on-power lid action to "Do nothing", and puts back what it was when the setting
//! goes off or the laptop goes on battery. Plans are usually the user's to change; where
//! they aren't (a policy), turning the setting on and off each asks for administrator once.

#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "macos")]
const RULE: &str = "/etc/sudoers.d/glass-lid";
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const PMSET: &str = "/usr/bin/pmset";

/// The sudoers rule for `user`: the two exact commands, nothing else.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn rule(user: &str) -> String {
    format!("# Written by Glass (glass config lid-awake on)\n{user} ALL=(root) NOPASSWD: {PMSET} -a disablesleep 0, {PMSET} -a disablesleep 1\n")
}

/// A user name safe to put in a sudoers rule.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plain_name(user: &str) -> bool {
    !user.is_empty() && user.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Lets Glass turn sleep off and on without a password: installs the rule, asking for the
/// admin password (in this terminal, or the system's dialog) unless it's there already.
pub fn allow() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        if allowed() {
            return Ok(());
        }
        let user = std::env::var("USER").unwrap_or_default();
        anyhow::ensure!(plain_name(&user), "can't tell which user to allow");
        let tmp = std::env::temp_dir().join(format!("glass-lid-{}", std::process::id()));
        std::fs::write(&tmp, rule(&user))?;
        let checked = Command::new("/usr/sbin/visudo").arg("-cf").arg(&tmp).output()?.status.success();
        if !checked {
            let _ = std::fs::remove_file(&tmp);
            anyhow::bail!("the sudoers rule didn't pass visudo");
        }
        use std::io::IsTerminal;
        let ask = if std::io::stdin().is_terminal() { crate::mic::Ask::Terminal } else { crate::mic::Ask::Dialog };
        let installed = crate::mic::as_admin(
            &format!("/usr/bin/install -m 0440 -o root -g wheel '{}' {RULE}", tmp.display()),
            ask,
            "let it stay awake with the lid closed",
        );
        let _ = std::fs::remove_file(&tmp);
        installed?;
        anyhow::ensure!(allowed(), "the rule was installed, but sudo still asks for a password");
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        // polkit decides who may hold it; a session's own user usually may.
        let ok = std::process::Command::new("systemd-inhibit")
            .args(["--what=handle-lid-switch", "--who=Glass", "--why=Checking lid-awake", "true"])
            .output()
            .is_ok_and(|o| o.status.success());
        anyhow::ensure!(ok, "this system doesn't let Glass hold the lid switch (polkit: org.freedesktop.login1.inhibit-handle-lid-switch)");
        Ok(())
    }
    #[cfg(windows)]
    {
        win::allow()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    anyhow::bail!("lid-awake isn't available here")
}

/// `lid-awake` went off: puts back what Glass changed now, rather than when the host next
/// looks (it may not be running), asking for administrator if that's what it took.
pub fn release() -> anyhow::Result<()> {
    #[cfg(windows)]
    win::release()?;
    Ok(())
}

/// What the lid did before Glass set it to "Do nothing": the plan and its on-power value.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Saved {
    scheme: String,
    original: u32,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Saved {
    fn parse(s: &str) -> Option<Saved> {
        let mut words = s.split_whitespace();
        let scheme = words.next()?.to_string();
        let original = words.next()?.parse().ok()?;
        Some(Saved { scheme, original })
    }

    fn line(&self) -> String {
        format!("{} {}\n", self.scheme, self.original)
    }
}

/// One change to a plan's on-power lid action.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum Op {
    /// Set it to "Do nothing" (0), remembering `original`.
    Hold { scheme: String, original: u32 },
    /// Put back what was there before.
    Restore { scheme: String, value: u32 },
    /// It's no longer Glass's change (someone set it since, or the plan is gone).
    Forget,
}

/// What to change, given whether the lid should stay awake, the active plan, what Glass
/// saved, and each plan's on-power lid action.
#[cfg_attr(not(windows), allow(dead_code))]
fn plan(want: bool, active: &str, saved: Option<&Saved>, lid: impl Fn(&str) -> Option<u32>) -> Vec<Op> {
    let mut ops = Vec::new();
    if let Some(s) = saved
        && !(want && s.scheme == active)
    {
        ops.push(match lid(&s.scheme) {
            Some(0) => Op::Restore { scheme: s.scheme.clone(), value: s.original },
            _ => Op::Forget,
        });
    }
    // Already "Do nothing" (Glass's doing, or the user's own): nothing to change.
    if want && let Some(v) = lid(active).filter(|&v| v != 0) {
        ops.push(Op::Hold { scheme: active.to_string(), original: v });
    }
    ops
}

/// On power, from GetSystemPowerStatus' ACLineStatus (0 offline, 1 online, 255 unknown: a
/// desktop that can't tell is plugged in).
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_on_power(ac_line_status: u8) -> bool {
    ac_line_status != 0
}

#[cfg(windows)]
mod win {
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SUCCESS, HLOCAL, LocalFree, WIN32_ERROR};
    use windows::Win32::System::Power::{
        GetSystemPowerStatus, PowerGetActiveScheme, PowerReadACValueIndex, PowerSetActiveScheme, PowerWriteACValueIndex,
        SYSTEM_POWER_STATUS,
    };
    use windows::core::GUID;

    use super::{Op, Saved, plan, windows_on_power};

    /// GUID_SYSTEM_BUTTON_SUBGROUP (powercfg's SUB_BUTTONS).
    const SUB_BUTTONS: GUID = GUID::from_u128(0x4f971e89_eebd_4455_a8de_9e59040e7347);
    /// GUID_LIDCLOSE_ACTION (LIDACTION): 0 do nothing, 1 sleep, 2 hibernate, 3 shut down.
    const LIDACTION: GUID = GUID::from_u128(0x5ca83367_6e45_459f_a27b_476b1d01c936);

    fn marker() -> std::path::PathBuf {
        crate::config_dir().join("lid-awake-applied")
    }

    fn saved() -> Option<Saved> {
        Saved::parse(&std::fs::read_to_string(marker()).ok()?)
    }

    fn active() -> Option<String> {
        let mut p: *mut GUID = std::ptr::null_mut();
        // SAFETY: p is a valid out pointer; the GUID it gets is copied, then freed with
        // LocalFree as the docs say.
        unsafe {
            if PowerGetActiveScheme(None, &mut p) != ERROR_SUCCESS || p.is_null() {
                return None;
            }
            let g = *p;
            let _ = LocalFree(Some(HLOCAL(p.cast())));
            Some(format!("{g:?}"))
        }
    }

    /// The plan's on-power lid action.
    fn lid(scheme: &str) -> Option<u32> {
        let g = GUID::try_from(scheme).ok()?;
        let mut v = 0u32;
        // SAFETY: the GUIDs live for the call; v is a valid out pointer.
        let r = unsafe { PowerReadACValueIndex(None, Some(&g), Some(&SUB_BUTTONS), Some(&LIDACTION), &mut v) };
        (r == ERROR_SUCCESS).then_some(v)
    }

    /// Sets it, and re-applies the plan if it's the active one (changes to it don't take
    /// effect until then).
    fn set_lid(scheme: &str, value: u32) -> Result<(), WIN32_ERROR> {
        let g = GUID::try_from(scheme).map_err(|_| windows::Win32::Foundation::ERROR_INVALID_PARAMETER)?;
        // SAFETY: the GUIDs live for the call.
        let r = unsafe { PowerWriteACValueIndex(None, &g, Some(&SUB_BUTTONS), Some(&LIDACTION), value) };
        if r != ERROR_SUCCESS {
            return Err(r);
        }
        if active().as_deref() == Some(scheme) {
            // SAFETY: g lives for the call.
            let r = unsafe { PowerSetActiveScheme(None, Some(&g)) };
            if r != ERROR_SUCCESS {
                return Err(r);
            }
        }
        Ok(())
    }

    /// The same change as administrator, with powercfg: Windows asks once.
    fn set_lid_elevated(scheme: &str, value: u32) -> bool {
        let cmd = format!("/c powercfg /setacvalueindex {scheme} SUB_BUTTONS LIDACTION {value} && powercfg /setactive SCHEME_CURRENT");
        let script = format!(
            "$ErrorActionPreference = 'Stop'; $p = Start-Process -FilePath cmd.exe -ArgumentList {} -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $p.ExitCode",
            crate::platform::ps_string(&cmd)
        );
        crate::platform::powershell(&script).status().is_ok_and(|s| s.success()) && lid(scheme) == Some(value)
    }

    fn on_power() -> bool {
        let mut s = SYSTEM_POWER_STATUS::default();
        // SAFETY: s is a valid out pointer.
        unsafe { GetSystemPowerStatus(&mut s) }.is_err() || windows_on_power(s.ACLineStatus)
    }

    /// Checks that Glass can change the lid action (writing back the value it has); if
    /// only an administrator can, sets it to "Do nothing" now, with Windows' prompt.
    pub fn allow() -> anyhow::Result<()> {
        let scheme = active().ok_or_else(|| anyhow::anyhow!("can't read the active power plan"))?;
        let current = lid(&scheme).ok_or_else(|| anyhow::anyhow!("this power plan has no lid setting (not a laptop?)"))?;
        match set_lid(&scheme, current) {
            Ok(()) => Ok(()),
            Err(e) if e == ERROR_ACCESS_DENIED => {
                if current != 0 {
                    std::fs::write(marker(), Saved { scheme: scheme.clone(), original: current }.line())?;
                }
                anyhow::ensure!(set_lid_elevated(&scheme, 0), "Windows didn't allow changing what closing the lid does");
                Ok(())
            }
            Err(e) => anyhow::bail!("can't change what closing the lid does ({e:?})"),
        }
    }

    pub fn release() -> anyhow::Result<()> {
        let Some(s) = saved() else { return Ok(()) };
        if lid(&s.scheme) == Some(0) {
            match set_lid(&s.scheme, s.original) {
                Ok(()) => {}
                Err(e) if e == ERROR_ACCESS_DENIED => {
                    anyhow::ensure!(set_lid_elevated(&s.scheme, s.original), "Windows didn't allow putting the lid setting back");
                }
                Err(e) => anyhow::bail!("can't put the lid setting back ({e:?})"),
            }
        }
        let _ = std::fs::remove_file(marker());
        Ok(())
    }

    pub fn watch() {
        std::thread::spawn(|| {
            let mut warned = false;
            loop {
                let want = crate::settings::load().lid_awake && on_power();
                if let Some(active) = active() {
                    for op in plan(want, &active, saved().as_ref(), lid) {
                        match op {
                            Op::Hold { scheme, original } => {
                                // Saved first, so a crash after the change can still undo it.
                                let saved = std::fs::write(marker(), Saved { scheme: scheme.clone(), original }.line());
                                if saved.is_ok() && set_lid(&scheme, 0).is_ok() {
                                    warned = false;
                                    tracing::info!("lid-awake: on power, so closing the lid does nothing");
                                } else {
                                    let _ = std::fs::remove_file(marker());
                                    if !warned {
                                        warned = true;
                                        tracing::warn!("lid-awake: can't change the lid setting; run `glass config lid-awake on` again");
                                    }
                                }
                            }
                            Op::Restore { scheme, value } => match set_lid(&scheme, value) {
                                Ok(()) => {
                                    let _ = std::fs::remove_file(marker());
                                    tracing::info!("lid-awake: closing the lid does what it did before");
                                }
                                // Set as administrator: put back when the setting goes off.
                                Err(_) if !warned => {
                                    warned = true;
                                    tracing::warn!("lid-awake: can't put the lid setting back without administrator");
                                }
                                Err(_) => {}
                            },
                            Op::Forget => {
                                let _ = std::fs::remove_file(marker());
                            }
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        });
    }
}

/// Sudo lets this user toggle sleep without a password.
#[cfg(target_os = "macos")]
fn allowed() -> bool {
    Command::new("/usr/bin/sudo")
        .args(["-n", "-l", PMSET, "-a", "disablesleep", "1"])
        .output()
        .is_ok_and(|o| o.status.success())
}

#[cfg(target_os = "macos")]
fn set_sleep_disabled(on: bool) -> bool {
    Command::new("/usr/bin/sudo")
        .args(["-n", PMSET, "-a", "disablesleep", if on { "1" } else { "0" }])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Sleep is off, from `pmset -g` (it lists `SleepDisabled` only while it's on).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_sleep_disabled(pmset_g: &str) -> bool {
    pmset_g.lines().any(|l| {
        let mut words = l.split_whitespace();
        words.next() == Some("SleepDisabled") && words.next() == Some("1")
    })
}

/// On power, from each supply in /sys/class/power_supply as (type, online): a mains or USB
/// supply is plugged in, or there are none at all (a desktop).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_on_power(supplies: &[(String, String)]) -> bool {
    let mut mains = supplies.iter().filter(|(kind, _)| matches!(kind.as_str(), "Mains" | "USB")).peekable();
    mains.peek().is_none() || mains.any(|(_, online)| online == "1")
}

#[cfg(target_os = "linux")]
fn linux_supplies() -> Vec<(String, String)> {
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap_or_default().trim().to_string();
    std::fs::read_dir("/sys/class/power_supply")
        .map(|d| d.flatten().map(|e| (read(e.path().join("type")), read(e.path().join("online")))).collect())
        .unwrap_or_default()
}

/// The Mac is on power, from `pmset -g batt` (a Mac without a battery always is).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_on_power(pmset_batt: &str) -> bool {
    !pmset_batt.contains("'Battery Power'")
}

#[cfg(target_os = "macos")]
fn pmset(args: &[&str]) -> String {
    Command::new(PMSET).args(args).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}

/// Glass turned sleep off (so it's Glass's to turn back on, not someone else's setting).
#[cfg(target_os = "macos")]
fn marker() -> std::path::PathBuf {
    crate::config_dir().join("lid-awake-applied")
}

/// Keeps a closed lid from sleeping while `lid-awake` is on and the laptop is on power, and
/// lets it again otherwise. Checks every few seconds, for as long as the host runs.
pub fn watch() {
    #[cfg(windows)]
    win::watch();
    #[cfg(target_os = "linux")]
    std::thread::spawn(|| {
        // The inhibitor lasts as long as this child; `tail --pid` ends it with the host.
        let mut held: Option<std::process::Child> = None;
        let mut warned = false;
        loop {
            let want = crate::settings::load().lid_awake && linux_on_power(&linux_supplies());
            if let Some(child) = &mut held
                && !matches!(child.try_wait(), Ok(None))
            {
                held = None;
                if !warned {
                    warned = true;
                    tracing::warn!("lid-awake: couldn't hold the lid switch (polkit?)");
                }
            }
            if want && held.is_none() && !warned {
                held = std::process::Command::new("systemd-inhibit")
                    .args(["--what=handle-lid-switch", "--who=Glass", "--why=Stay awake with the lid closed (glass config lid-awake)", "--mode=block"])
                    .args(["tail", &format!("--pid={}", std::process::id()), "-f", "/dev/null"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .inspect(|_| tracing::info!("lid-awake: on power, so a closed lid won't suspend"))
                    .ok();
            } else if !want && let Some(mut child) = held.take() {
                let _ = child.kill();
                let _ = child.wait();
                tracing::info!("lid-awake: the lid may suspend again");
            }
            if !want {
                warned = false;
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    });
    #[cfg(target_os = "macos")]
    std::thread::spawn(|| {
        let mut warned = false;
        loop {
            let want = crate::settings::load().lid_awake && parse_on_power(&pmset(&["-g", "batt"]));
            let off = parse_sleep_disabled(&pmset(&["-g"]));
            let ours = marker().exists();
            if want && !off {
                if set_sleep_disabled(true) {
                    let _ = std::fs::write(marker(), b"");
                    warned = false;
                    tracing::info!("lid-awake: on power, so a closed lid won't sleep");
                } else if !warned {
                    warned = true;
                    tracing::warn!("lid-awake: can't turn sleep off; run `glass config lid-awake on` again");
                }
            } else if !want && off && ours {
                if set_sleep_disabled(false) {
                    let _ = std::fs::remove_file(marker());
                    tracing::info!("lid-awake: sleep allowed again");
                }
            } else if !want && ours && !off {
                let _ = std::fs::remove_file(marker());
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rule_allows_only_the_two_toggles() {
        assert_eq!(
            rule("nathan").lines().nth(1),
            Some("nathan ALL=(root) NOPASSWD: /usr/bin/pmset -a disablesleep 0, /usr/bin/pmset -a disablesleep 1")
        );
        assert!(plain_name("first.last-2"));
        assert!(!plain_name("nathan ALL=(ALL) ALL"));
        assert!(!plain_name(""));
    }

    #[test]
    fn reads_pmset() {
        assert!(parse_sleep_disabled(" sleep 1\n SleepDisabled\t\t1\n"));
        assert!(!parse_sleep_disabled(" SleepDisabled\t\t0\n"));
        assert!(!parse_sleep_disabled(" sleep 1\n"));
        assert!(parse_on_power("Now drawing from 'AC Power'\n -InternalBattery-0 80%"));
        assert!(!parse_on_power("Now drawing from 'Battery Power'\n -InternalBattery-0 80%"));
    }

    #[test]
    fn linux_power_is_a_plugged_in_supply_or_none_at_all() {
        let s = |kind: &str, online: &str| (kind.to_string(), online.to_string());
        assert!(linux_on_power(&[s("Battery", ""), s("Mains", "1")]));
        assert!(!linux_on_power(&[s("Battery", ""), s("Mains", "0"), s("USB", "0")]));
        assert!(linux_on_power(&[s("USB", "1"), s("Mains", "0")]));
        assert!(linux_on_power(&[]));
    }

    #[test]
    fn windows_holds_the_lid_and_puts_back_only_its_own_change() {
        const A: &str = "381B4222-F694-41F0-9685-FF5BB260DF2E";
        const B: &str = "8C5E7FDA-E8BF-4A96-9A85-A6E23A8C635C";
        let sleeps = |_: &str| Some(1);
        let nothing = |_: &str| Some(0);
        let ours = Saved { scheme: A.into(), original: 1 };
        // On power: hold, remembering what it was; already "Do nothing": leave it.
        assert_eq!(plan(true, A, None, sleeps), [Op::Hold { scheme: A.into(), original: 1 }]);
        assert_eq!(plan(true, A, None, nothing), []);
        assert_eq!(plan(true, A, Some(&ours), nothing), []);
        // Battery or off: put it back, unless someone has set it since.
        assert_eq!(plan(false, A, Some(&ours), nothing), [Op::Restore { scheme: A.into(), value: 1 }]);
        assert_eq!(plan(false, A, Some(&ours), |_| Some(2)), [Op::Forget]);
        assert_eq!(plan(false, A, None, sleeps), []);
        // Someone set it back while held: hold again, and that's what to put back later.
        assert_eq!(plan(true, A, Some(&ours), |_| Some(3)), [Op::Hold { scheme: A.into(), original: 3 }]);
        // Another plan became active: undo the old one, hold the new one.
        assert_eq!(
            plan(true, B, Some(&ours), |s| Some(if s == A { 0 } else { 1 })),
            [Op::Restore { scheme: A.into(), value: 1 }, Op::Hold { scheme: B.into(), original: 1 }]
        );
        // A plan that's gone (or has no lid setting) has nothing to put back.
        assert_eq!(plan(false, A, Some(&ours), |_| None), [Op::Forget]);
    }

    #[test]
    fn windows_saved_lid_and_power() {
        let s = Saved { scheme: "381B4222-F694-41F0-9685-FF5BB260DF2E".into(), original: 1 };
        assert_eq!(Saved::parse(&s.line()), Some(s));
        assert_eq!(Saved::parse(""), None);
        assert_eq!(Saved::parse("381B4222-F694-41F0-9685-FF5BB260DF2E x"), None);
        assert!(windows_on_power(1));
        assert!(windows_on_power(255));
        assert!(!windows_on_power(0));
    }
}
