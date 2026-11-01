//! The optional secure-desktop service (`glass service install`). It runs as LocalSystem
//! and does one thing: keep a SYSTEM helper (see `windows::secure`) running in the active
//! console session, re-spawning it whenever that session changes or the helper exits. The
//! helper is what can see and drive the lock screen, UAC prompts and elevated windows; the
//! per-user host stays exactly as it was and remains the only thing that serves and
//! authenticates viewers.
//!
//! The service account is LocalSystem so it can duplicate its own token into the console
//! session and call SendSAS; nothing else about Glass moves to SYSTEM. Install also sets
//! the SoftwareSASGeneration policy so SendSAS (Ctrl+Alt+Del) is allowed.

use std::sync::atomic::{AtomicIsize, Ordering};

use anyhow::{Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::core::{HSTRING, PWSTR};

/// The flag the SCM starts us with (see main.rs).
pub const SERVICE_ARG: &str = "--glass-service";
const SERVICE_NAME: &str = "GlassSecureDesktop";
const DISPLAY_NAME: &str = "Glass Secure Desktop";

/// `glass service <install|uninstall|status>`.
pub fn cli(action: &str) -> Result<()> {
    match action {
        "install" => install(),
        "uninstall" => uninstall(),
        "status" => {
            match status()? {
                Some(true) => println!("The Glass secure-desktop service is installed and running."),
                Some(false) => println!("The Glass secure-desktop service is installed but not running."),
                None => println!(
                    "The Glass secure-desktop service isn't installed. Install it (lets viewers use the\n\
                     lock screen, answer UAC, control elevated windows and send Ctrl+Alt+Del) with:\n  glass service install"
                ),
            }
            Ok(())
        }
        other => anyhow::bail!("unknown action {other}"),
    }
}

/// Whether the service exists, and if so whether it's running. `None` = not installed.
pub fn status() -> Result<Option<bool>> {
    use windows::Win32::System::Services::{
        ControlService, OpenSCManagerW, OpenServiceW, SC_MANAGER_ALL_ACCESS, SERVICE_ALL_ACCESS, SERVICE_CONTROL_INTERROGATE, SERVICE_RUNNING, SERVICE_STATUS,
    };
    // SAFETY: handles are closed on every path out.
    unsafe {
        let scm = match OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS) {
            Ok(h) => h,
            Err(_) => return Ok(None),
        };
        let svc = OpenServiceW(scm, &HSTRING::from(SERVICE_NAME), SERVICE_ALL_ACCESS);
        let out = match svc {
            Ok(svc) => {
                let mut st = SERVICE_STATUS::default();
                let running = ControlService(svc, SERVICE_CONTROL_INTERROGATE, &mut st).is_ok()
                    && st.dwCurrentState == SERVICE_RUNNING;
                let _ = CloseServiceHandle(svc.0);
                Some(running)
            }
            Err(_) => None,
        };
        let _ = CloseServiceHandle(scm.0);
        Ok(out)
    }
}

/// Closes an SC_HANDLE (windows-rs doesn't wrap CloseServiceHandle on SC_HANDLE directly).
unsafe fn CloseServiceHandle(h: *mut core::ffi::c_void) -> bool {
    // SAFETY: closing a service-control handle we opened.
    unsafe { windows::Win32::System::Services::CloseServiceHandle(windows::Win32::System::Services::SC_HANDLE(h)).is_ok() }
}

fn install() -> Result<()> {
    if !is_elevated() {
        // One UAC prompt, like the firewall step (see windows::firewall).
        return elevate("install");
    }
    let exe = std::env::current_exe()?;
    let bin = format!("\"{}\" {SERVICE_ARG}", exe.display());
    use windows::Win32::System::Services::{
        CreateServiceW, OpenSCManagerW, SC_MANAGER_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_ALL_ACCESS,
        SERVICE_ERROR_NORMAL, SERVICE_WIN32_OWN_PROCESS, StartServiceW,
    };
    set_sas_policy(true)?;
    // SAFETY: all strings are NUL-terminated HSTRINGs; handles are closed below.
    unsafe {
        let scm = OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS).context("open the service manager (need administrator)")?;
        let svc = CreateServiceW(
            scm,
            &HSTRING::from(SERVICE_NAME),
            &HSTRING::from(DISPLAY_NAME),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            &HSTRING::from(bin),
            None,
            None,
            None,
            None, // lpServiceStartName = null → LocalSystem
            None,
        );
        let svc = match svc {
            Ok(svc) => svc,
            Err(e) if e.code() == windows::Win32::Foundation::ERROR_SERVICE_EXISTS.to_hresult() => {
                let _ = CloseServiceHandle(scm.0);
                println!("The Glass secure-desktop service is already installed.");
                return Ok(());
            }
            Err(e) => {
                let _ = CloseServiceHandle(scm.0);
                return Err(e).context("create the service");
            }
        };
        let started = StartServiceW(svc, None).is_ok();
        let _ = CloseServiceHandle(svc.0);
        let _ = CloseServiceHandle(scm.0);
        if started {
            println!("Installed and started the Glass secure-desktop service.");
        } else {
            println!("Installed the Glass secure-desktop service; it starts at the next sign-in or reboot.");
        }
    }
    Ok(())
}

fn uninstall() -> Result<()> {
    if !is_elevated() {
        return elevate("uninstall");
    }
    use windows::Win32::System::Services::{
        ControlService, DeleteService, OpenSCManagerW, OpenServiceW, SC_MANAGER_ALL_ACCESS, SERVICE_ALL_ACCESS,
        SERVICE_CONTROL_STOP, SERVICE_STATUS,
    };
    // SAFETY: handles closed on every path; a missing service is reported, not fatal.
    unsafe {
        let scm = OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS).context("open the service manager (need administrator)")?;
        let svc = match OpenServiceW(scm, &HSTRING::from(SERVICE_NAME), SERVICE_ALL_ACCESS) {
            Ok(svc) => svc,
            Err(_) => {
                let _ = CloseServiceHandle(scm.0);
                println!("The Glass secure-desktop service isn't installed.");
                return Ok(());
            }
        };
        let mut st = SERVICE_STATUS::default();
        let _ = ControlService(svc, SERVICE_CONTROL_STOP, &mut st);
        let deleted = DeleteService(svc).is_ok();
        let _ = CloseServiceHandle(svc.0);
        let _ = CloseServiceHandle(scm.0);
        anyhow::ensure!(deleted, "couldn't remove the service");
    }
    let _ = set_sas_policy(false);
    println!("Removed the Glass secure-desktop service.");
    Ok(())
}

/// Turns the SoftwareSASGeneration policy on (so SendSAS works) or off.
fn set_sas_policy(on: bool) -> Result<()> {
    use windows::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, REG_DWORD, RegDeleteKeyValueW, RegSetKeyValueW,
    };
    const KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
    const VALUE: &str = "SoftwareSASGeneration";
    // SAFETY: plain registry writes under HKLM (we're elevated).
    unsafe {
        if on {
            let one: u32 = 1;
            RegSetKeyValueW(
                HKEY_LOCAL_MACHINE,
                &HSTRING::from(KEY),
                &HSTRING::from(VALUE),
                REG_DWORD.0,
                Some((&raw const one).cast()),
                4,
            )
            .ok()
            .context("set the SoftwareSASGeneration policy")?;
        } else {
            let _ = RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, &HSTRING::from(KEY), &HSTRING::from(VALUE));
        }
    }
    Ok(())
}

/// Re-runs `glass service <action>` as administrator (one UAC prompt), like the firewall step.
fn elevate(action: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let script = format!(
        "$ErrorActionPreference='Stop'; $p = Start-Process -FilePath {} -ArgumentList 'service','{action}' -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $p.ExitCode",
        super::ps_string(&exe.display().to_string())
    );
    let ok = super::powershell(&script).status().is_ok_and(|s| s.success());
    anyhow::ensure!(ok, "administrator approval was declined, or the elevated step failed");
    Ok(())
}

/// Whether this process is elevated (runs with a full administrator token).
fn is_elevated() -> bool {
    use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: the token is closed right after the query.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

// ---- Running as the service ----

/// Set by the control handler, read by the keep-alive loop. HANDLEs stored as isize so the
/// `extern "system"` handler can reach them.
static STOP_EVENT: AtomicIsize = AtomicIsize::new(0);
static WAKE_EVENT: AtomicIsize = AtomicIsize::new(0);
static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);

/// The SCM entry point (`glass --glass-service`).
pub fn run_service() -> Result<()> {
    use windows::Win32::System::Services::{SERVICE_TABLE_ENTRYW, StartServiceCtrlDispatcherW};
    let mut name: Vec<u16> = SERVICE_NAME.encode_utf16().chain([0]).collect();
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: a null-terminated table whose names live for the call (the dispatcher blocks
    // here until the service stops).
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }.context("StartServiceCtrlDispatcher")?;
    Ok(())
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    use windows::Win32::System::Services::RegisterServiceCtrlHandlerExW;
    // SAFETY: registering our handler; the returned status handle is stored for updates.
    let handle = unsafe { RegisterServiceCtrlHandlerExW(&HSTRING::from(SERVICE_NAME), Some(handler), None) };
    let Ok(handle) = handle else { return };
    STATUS_HANDLE.store(handle.0 as isize, Ordering::SeqCst);

    // SAFETY: two auto-reset events, kept for the service's life.
    let (stop, wake) = unsafe {
        use windows::Win32::System::Threading::CreateEventW;
        (
            CreateEventW(None, false, false, None).unwrap_or_default(),
            CreateEventW(None, false, false, None).unwrap_or_default(),
        )
    };
    STOP_EVENT.store(stop.0 as isize, Ordering::SeqCst);
    WAKE_EVENT.store(wake.0 as isize, Ordering::SeqCst);

    set_status(SERVICE_STATE_RUNNING, 0);
    keep_helper_alive(stop, wake);
    set_status(SERVICE_STATE_STOPPED, 0);
}

const SERVICE_STATE_RUNNING: u32 = 4;
const SERVICE_STATE_STOPPED: u32 = 1;
const SERVICE_STATE_STOP_PENDING: u32 = 3;

/// Tells the SCM what state we're in, accepting stop, shutdown and session-change.
fn set_status(state: u32, checkpoint: u32) {
    use windows::Win32::System::Services::{
        SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_STATUS,
        SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus,
    };
    let handle = STATUS_HANDLE.load(Ordering::SeqCst);
    if handle == 0 {
        return;
    }
    let accepts = if state == SERVICE_STATE_RUNNING {
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE
    } else {
        0
    };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: SERVICE_STATUS_CURRENT_STATE(state),
        dwControlsAccepted: accepts,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: checkpoint,
        dwWaitHint: if state == SERVICE_STATE_STOP_PENDING { 3000 } else { 0 },
    };
    // SAFETY: a valid status handle and a fully-initialized status.
    unsafe { let _ = SetServiceStatus(SERVICE_STATUS_HANDLE(handle as *mut _), &status); }
}

unsafe extern "system" fn handler(control: u32, _event: u32, _data: *mut core::ffi::c_void, _ctx: *mut core::ffi::c_void) -> u32 {
    use windows::Win32::System::Services::{SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP};
    use windows::Win32::System::Threading::SetEvent;
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            set_status(SERVICE_STATE_STOP_PENDING, 1);
            let stop = STOP_EVENT.load(Ordering::SeqCst);
            // SAFETY: signalling the stop event if it exists.
            if stop != 0 { unsafe { let _ = SetEvent(HANDLE(stop as *mut _)); } }
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            let wake = WAKE_EVENT.load(Ordering::SeqCst);
            // SAFETY: nudging the loop to re-check the active session.
            if wake != 0 { unsafe { let _ = SetEvent(HANDLE(wake as *mut _)); } }
        }
        _ => {}
    }
    0
}

/// Keeps the SYSTEM helper running in the active console session until the service stops.
fn keep_helper_alive(stop: HANDLE, wake: HANDLE) {
    use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    use windows::Win32::System::Threading::WaitForMultipleObjects;
    let mut child: Option<HANDLE> = None;
    let mut child_session = 0xFFFF_FFFFu32;
    loop {
        // SAFETY: plain query.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        // Reap a helper that exited.
        if let Some(h) = child
            && !super::process_alive_handle(h)
        {
            // SAFETY: closing the exited child's handle.
            unsafe { let _ = CloseHandle(h); }
            child = None;
        }
        // A real interactive session (not the "no one connected" 0xFFFFFFFF, nor session 0).
        let usable = session != 0xFFFF_FFFF && session != 0;
        if usable && (child.is_none() || session != child_session) {
            if let Some(h) = child.take() {
                // Session changed: the old helper is in the wrong session; drop it.
                terminate(h);
            }
            match spawn_helper_in_session(session) {
                Ok(h) => {
                    child = Some(h);
                    child_session = session;
                }
                Err(()) => {}
            }
        }
        // Wake on stop, on the helper exiting, or every second to re-check the session.
        let mut handles = vec![stop, wake];
        if let Some(h) = child {
            handles.push(h);
        }
        // SAFETY: all handles are live for the wait.
        let woke = unsafe { WaitForMultipleObjects(&handles, false, 1000) };
        if woke == WAIT_OBJECT_0 {
            break; // stop (handles[0])
        }
    }
    if let Some(h) = child {
        terminate(h);
    }
}

fn terminate(child: HANDLE) {
    use windows::Win32::System::Threading::TerminateProcess;
    // SAFETY: the child is ours; terminate and close it.
    unsafe {
        let _ = TerminateProcess(child, 0);
        let _ = CloseHandle(child);
    }
}

/// Launches the SYSTEM helper in `session` on the default desktop, by duplicating this
/// service's own (LocalSystem) token and moving the copy into that session. Returns the
/// new process's handle.
fn spawn_helper_in_session(session: u32) -> Result<HANDLE, ()> {
    use windows::Win32::Security::{
        DuplicateTokenEx, SECURITY_IMPERSONATION_LEVEL, SetTokenInformation, TOKEN_ALL_ACCESS, TOKEN_TYPE,
        TokenPrimary, TokenSessionId,
    };
    use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken,
        PROCESS_INFORMATION, STARTUPINFOW,
    };
    let Ok(exe) = std::env::current_exe() else { return Err(()) };
    // SAFETY: every handle opened here is closed before returning; buffers are sized for
    // the APIs. On any failure we clean up and report Err so the loop retries later.
    unsafe {
        let mut token = HANDLE::default();
        let access = windows::Win32::Security::TOKEN_ACCESS_MASK(
            0x0002 | 0x0008 | 0x0001 | 0x0080 | 0x0100, // DUPLICATE|QUERY|ASSIGN_PRIMARY|ADJUST_DEFAULT|ADJUST_SESSIONID
        );
        if OpenProcessToken(GetCurrentProcess(), access, &mut token).is_err() {
            return Err(());
        }
        let mut dup = HANDLE::default();
        let dup_ok = DuplicateTokenEx(
            token,
            TOKEN_ALL_ACCESS,
            None,
            SECURITY_IMPERSONATION_LEVEL(2), // SecurityImpersonation
            TOKEN_TYPE(TokenPrimary.0),
            &mut dup,
        )
        .is_ok();
        let _ = CloseHandle(token);
        if !dup_ok {
            return Err(());
        }
        let sid = session;
        let set = SetTokenInformation(dup, TokenSessionId, (&raw const sid).cast(), 4).is_ok();
        if !set {
            let _ = CloseHandle(dup);
            return Err(());
        }
        let mut env = std::ptr::null_mut();
        let have_env = CreateEnvironmentBlock(&mut env, Some(dup), false).is_ok();

        let mut cmd: Vec<u16> = format!("\"{}\" {}", exe.display(), super::secure::HELPER_ARG)
            .encode_utf16()
            .chain([0])
            .collect();
        let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
        let mut si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        let flags = CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT;
        let ok = CreateProcessAsUserW(
            Some(dup),
            None,
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            flags,
            if have_env { Some(env) } else { None },
            None,
            &mut si,
            &mut pi,
        )
        .is_ok();
        if have_env {
            let _ = DestroyEnvironmentBlock(env);
        }
        let _ = CloseHandle(dup);
        if !ok {
            return Err(());
        }
        if !pi.hThread.is_invalid() {
            let _ = CloseHandle(pi.hThread);
        }
        Ok(pi.hProcess)
    }
}
