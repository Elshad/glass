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

fn spawn_launcher() -> Result<()> {
    let path = launcher()?;
    std::process::Command::new(&path).spawn().with_context(|| format!("start {}", path.display()))?;
    Ok(())
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
    spawn_launcher()
}

pub fn uninstall() -> Result<()> {
    signal("Local\\GlassAgentStop");
    remove_run_key();
    let _ = std::fs::remove_file(marker());
    Ok(())
}
