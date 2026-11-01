//! Runs the host in the background: `glassw.exe` (src/bin/glassw.rs) starts at logon from
//! the HKCU Run key, needing no administrator, and keeps the host running in the user's
//! session (a Windows service would run in session 0, which can't see the desktop).

use std::path::PathBuf;

use anyhow::{Context, Result};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
use windows::Win32::System::Threading::{OpenEventW, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, EVENT_MODIFY_STATE};
use windows::core::{HSTRING, w};

pub const LOG_HINT: &str = " Check %LOCALAPPDATA%\\Glass\\Logs\\agent.log.";

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Glass";

/// Marks the agent as installed (it holds the launcher's path).
fn marker() -> PathBuf {
    super::local_app_data().join("Glass").join("agent")
}

fn launcher() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let path = exe.with_file_name("glassw.exe");
    anyhow::ensure!(path.exists(), "{} is missing; reinstall Glass", path.display());
    Ok(path)
}

pub fn is_installed() -> bool {
    marker().exists()
}

/// Whether glassw is running in this session.
fn running() -> bool {
    // SAFETY: the handle, if any, is closed right away.
    unsafe {
        match OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, w!("Local\\GlassAgent")) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

/// Signals one of glassw's events; false if it isn't running.
fn signal(name: &str) -> bool {
    // SAFETY: the handle is closed after signalling.
    unsafe {
        let Ok(h) = OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(name)) else { return false };
        let ok = SetEvent(h).is_ok();
        let _ = CloseHandle(h);
        ok
    }
}

/// Starts glassw in the signed-in desktop session. From a session without a desktop
/// (an SSH shell runs in session 0) it would start where nothing can be captured, so it's
/// handed to a one-off interactive scheduled task that runs in the user's desktop session.
fn spawn_launcher() -> Result<()> {
    let path = launcher()?;
    if !in_service_session() {
        std::process::Command::new(&path).spawn().with_context(|| format!("start {}", path.display()))?;
        return Ok(());
    }
    const TASK: &str = "Glass launcher";
    let schtasks = |args: &[&str]| -> Result<()> {
        let mut cmd = std::process::Command::new("schtasks");
        cmd.args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        super::no_window(&mut cmd);
        let status = cmd.status().context("schtasks")?;
        anyhow::ensure!(status.success(), "schtasks {} failed", args[0]);
        Ok(())
    };
    let run = format!("\"{}\"", path.display());
    schtasks(&["/Create", "/TN", TASK, "/TR", &run, "/SC", "ONCE", "/ST", "00:00", "/IT", "/F"])
        .context("start Glass in the desktop session (is anyone signed in at the computer?)")?;
    let started = schtasks(&["/Run", "/TN", TASK]);
    let _ = schtasks(&["/Delete", "/TN", TASK, "/F"]);
    started
}

/// Whether this process runs in session 0 (services, and OpenSSH's shells), which has no
/// desktop to show or capture.
fn in_service_session() -> bool {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let mut session = 0u32;
    // SAFETY: plain query into a local.
    unsafe { ProcessIdToSessionId(std::process::id(), &mut session) }.is_ok() && session == 0
}

/// In the host, asked over the pipe from another session: `restart` (the host exits and
/// glassw starts it again) or `stop` (glassw ends the host and itself).
pub fn from_host(op: &str) -> Result<()> {
    match op {
        "restart" => crate::power::restart_glass(),
        "stop" => {
            anyhow::ensure!(signal("Local\\GlassAgentStop"), "Glass isn't running under its background launcher");
            Ok(())
        }
        _ => anyhow::bail!("unknown launcher request {op}"),
    }
}

/// Asks the running host to pass `op` to its launcher (it shares the launcher's session).
fn ask_host(op: &str) -> bool {
    crate::ipc::call(&crate::ipc::Request::Launcher { op: op.into() }).is_ok()
}

/// Registers the launcher (at logon if `start_at_login`) and (re)starts the host.
pub fn install(start_at_login: bool) -> Result<()> {
    let path = launcher()?;
    if start_at_login {
        let command: Vec<u16> = format!("\"{}\"", path.display()).encode_utf16().chain([0]).collect();
        // SAFETY: a NUL-terminated UTF-16 string and its size in bytes.
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_KEY),
                &HSTRING::from(RUN_VALUE),
                REG_SZ.0,
                Some(command.as_ptr().cast()),
                (command.len() * 2) as u32,
            )
            .ok()
            .context("add Glass to the programs that start at sign-in")?;
        }
    } else {
        remove_run_key();
    }
    std::fs::create_dir_all(marker().parent().unwrap())?;
    std::fs::write(marker(), path.display().to_string())?;
    restart()
}

fn remove_run_key() {
    // SAFETY: plain registry call; a missing value is fine.
    let _ = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(RUN_KEY), &HSTRING::from(RUN_VALUE)) };
}

pub fn restart() -> Result<()> {
    if running() && signal("Local\\GlassAgentRestart") {
        return Ok(());
    }
    // From another session the signal can't reach; the host can pass it on.
    if ask_host("restart") {
        return Ok(());
    }
    spawn_launcher()
}

pub fn uninstall() -> Result<()> {
    if !signal("Local\\GlassAgentStop") {
        ask_host("stop");
    }
    remove_run_key();
    let _ = std::fs::remove_file(marker());
    Ok(())
}
