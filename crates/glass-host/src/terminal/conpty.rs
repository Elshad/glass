//! The terminal on Windows: the shell runs in a pseudoconsole (ConPTY), which draws its
//! console as VT sequences into one pipe and reads keystrokes as VT from another.
//!
//! The output pipe only ends when the pseudoconsole is closed, not when the shell exits,
//! so a waiter thread closes it once the shell is gone; `pump` keeps reading meanwhile,
//! which closing needs (it can wait for conhost to finish writing).

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use super::Shell;

/// How long the shell's last output gets to come through after it exits before the
/// console closes: older conhosts drop whatever they haven't drawn yet.
const LAST_FRAME: Duration = Duration::from_millis(100);

/// An open pseudoconsole; dropping it closes it, which ends the output pipe.
pub(super) struct Console(HPCON);

impl Drop for Console {
    fn drop(&mut self) {
        // SAFETY: made by CreatePseudoConsole, and closed only here.
        unsafe { ClosePseudoConsole(self.0) }
    }
}

/// The shell's process. std's `Child` can't be made from a handle, so this has the three
/// methods `pump` and `Drop for Shell` use.
pub(super) struct Child(Arc<OwnedHandle>);

impl Child {
    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        // SAFETY: a process handle we own.
        if unsafe { WaitForSingleObject(handle(&self.0), 0) } != WAIT_OBJECT_0 {
            return Ok(None);
        }
        self.status().map(Some)
    }

    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        // SAFETY: as above.
        if unsafe { WaitForSingleObject(handle(&self.0), INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        self.status()
    }

    pub(super) fn kill(&mut self) -> io::Result<()> {
        // SAFETY: as above.
        unsafe { TerminateProcess(handle(&self.0), 1) }.map_err(io::Error::from)
    }

    fn status(&self) -> io::Result<ExitStatus> {
        let mut code = 0;
        // SAFETY: as above, with a u32 to fill in.
        unsafe { GetExitCodeProcess(handle(&self.0), &mut code) }?;
        Ok(ExitStatus::from_raw(code))
    }
}

impl Shell {
    pub(super) fn spawn(
        program: &str,
        args: &[&str],
        cols: u16,
        rows: u16,
    ) -> Result<(Self, File)> {
        let (in_read, in_write) = pipe()?;
        let (out_read, out_write) = pipe()?;
        // SAFETY: two open pipe ends; the console duplicates them, so ours close right after.
        let console = Console(
            unsafe {
                CreatePseudoConsole(size(cols, rows), handle(&in_read), handle(&out_write), 0)
            }
            .context("CreatePseudoConsole")?,
        );
        drop((in_read, out_write));
        let process = match start(program, args, &console) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                // Closing the console can wait for its output to be read; with no reader
                // yet, the pipe has to go first.
                drop(out_read);
                return Err(e);
            }
        };
        let console = Arc::new(Mutex::new(Some(console)));
        let (waited, closing) = (process.clone(), console.clone());
        std::thread::spawn(move || {
            // SAFETY: a process handle we own.
            unsafe { WaitForSingleObject(handle(&waited), INFINITE) };
            std::thread::sleep(LAST_FRAME);
            // Taken out of the slot first, so a resize never uses a closed console.
            let console = closing.lock().unwrap().take();
            drop(console);
        });
        Ok((
            Self {
                master: File::from(in_write),
                child: Arc::new(Mutex::new(Child(process))),
                console,
            },
            File::from(out_read),
        ))
    }

    pub(super) fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        if let Some(console) = &*self.console.lock().unwrap() {
            // SAFETY: the console is open while it's in the slot.
            unsafe { ResizePseudoConsole(console.0, size(cols, rows)) }
                .context("ResizePseudoConsole")?;
        }
        Ok(())
    }
}

/// PowerShell 7 if it's on PATH, else Windows PowerShell, which every Windows has.
pub(super) fn login_shell() -> String {
    // The Store's pwsh.exe is an app alias, which a plain metadata check can't follow.
    let found = |p: &Path| std::fs::symlink_metadata(p).is_ok();
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("pwsh.exe"))
                .find(|p| found(p))
        })
        .or_else(|| {
            let root = PathBuf::from(std::env::var_os("SystemRoot")?);
            Some(root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe")).filter(|p| found(p))
        })
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "powershell.exe".into())
}

fn handle(h: &OwnedHandle) -> HANDLE {
    HANDLE(h.as_raw_handle())
}

fn size(cols: u16, rows: u16) -> COORD {
    let clamp = |n: u16| n.clamp(1, i16::MAX as u16) as i16;
    COORD {
        X: clamp(cols),
        Y: clamp(rows),
    }
}

/// An anonymous pipe, (read end, write end); neither end is inherited.
fn pipe() -> Result<(OwnedHandle, OwnedHandle)> {
    let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
    // SAFETY: two out-pointers; on success both handles are ours to own.
    unsafe {
        CreatePipe(&mut read, &mut write, None, 0).context("CreatePipe")?;
        Ok((
            OwnedHandle::from_raw_handle(read.0),
            OwnedHandle::from_raw_handle(write.0),
        ))
    }
}

/// Starts the program attached to the console, in the home folder.
fn start(program: &str, args: &[&str], console: &Console) -> Result<OwnedHandle> {
    let mut attrs = Attributes::new(console)?;
    let mut si = STARTUPINFOEXW::default();
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    // Without this a shell can pick up the host's own stdio (a log file, say) when it's
    // redirected, instead of the console's.
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    si.lpAttributeList = attrs.list();
    let mut cmdline = wide(command_line(program, args));
    let env = environment();
    let home = crate::home();
    let dir = (!home.as_os_str().is_empty()).then(|| wide(home.as_os_str()));
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: every pointer is to a live, NUL-terminated buffer (the environment
    // double-terminated) that outlives the call; the attribute list is set up.
    unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            Some(env.as_ptr().cast()),
            dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
            &si.StartupInfo,
            &mut pi,
        )
    }
    .with_context(|| format!("start {program}"))?;
    // SAFETY: both handles are new and ours; the thread's isn't needed.
    unsafe {
        let _ = CloseHandle(pi.hThread);
        Ok(OwnedHandle::from_raw_handle(pi.hProcess.0))
    }
}

/// A one-entry attribute list that attaches the new process to the console; deleted
/// when dropped.
struct Attributes(Vec<u64>);

impl Attributes {
    fn new(console: &Console) -> Result<Self> {
        let mut size = 0;
        // SAFETY: a size query: it fails by design, filling in the size.
        let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) };
        let mut buf = vec![0u64; size.div_ceil(8)];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr().cast());
        // SAFETY: a buffer of the size asked for, pointer-aligned.
        unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &mut size) }
            .context("InitializeProcThreadAttributeList")?;
        // Moving the Vec leaves its buffer where it is.
        let mut attrs = Self(buf);
        // SAFETY: the value is the HPCON itself, not a pointer to it, as ConPTY wants; the
        // console outlives the list.
        unsafe {
            UpdateProcThreadAttribute(
                attrs.list(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                Some(console.0.0 as *const _),
                std::mem::size_of::<HPCON>(),
                None,
                None,
            )
        }
        .context("UpdateProcThreadAttribute")?;
        Ok(attrs)
    }

    fn list(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.0.as_mut_ptr().cast())
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: initialized in `new`, and deleted only here.
        unsafe { DeleteProcThreadAttributeList(self.list()) }
    }
}

/// The host's environment, plus what tells programs the terminal is xterm.js.
fn environment() -> Vec<u16> {
    const OURS: [(&str, &str); 2] = [("TERM", "xterm-256color"), ("COLORTERM", "truecolor")];
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| !OURS.iter().any(|(ours, _)| k.eq_ignore_ascii_case(ours)))
        .chain(OURS.map(|(k, v)| (k.into(), v.into())))
        .collect();
    // Windows wants the block sorted by name, ignoring case.
    vars.sort_by_key(|(k, _)| k.to_ascii_uppercase());
    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(k.encode_wide());
        block.push(u16::from(b'='));
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// The program and its arguments as one command line, quoted the way the C runtime
/// (and so most programs) splits it back up.
fn command_line(program: &str, args: &[&str]) -> String {
    let mut line = String::new();
    for (i, arg) in std::iter::once(&program).chain(args).enumerate() {
        if i > 0 {
            line.push(' ');
        }
        quote(arg, &mut line);
    }
    line
}

fn quote(arg: &str, line: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        line.push_str(arg);
        return;
    }
    line.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            // Backslashes before a quote are doubled, and the quote escaped.
            '"' => {
                line.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                line.push('"');
                backslashes = 0;
            }
            _ => {
                line.extend(std::iter::repeat_n('\\', backslashes));
                line.push(c);
                backslashes = 0;
            }
        }
    }
    // And before the closing quote.
    line.extend(std::iter::repeat_n('\\', backslashes * 2));
    line.push('"');
}

/// NUL-terminated UTF-16.
fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_like_the_c_runtime() {
        assert_eq!(
            command_line("cmd.exe", &["/c", "echo hi"]),
            r#"cmd.exe /c "echo hi""#
        );
        assert_eq!(command_line(r"C:\a b\x.exe", &[""]), r#""C:\a b\x.exe" """#);
        assert_eq!(
            command_line("x", &[r#"a\"b"#, r"c d\"]),
            r#"x "a\\\"b" "c d\\""#
        );
    }
}
