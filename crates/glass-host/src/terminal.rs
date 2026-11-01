//! A shell on the host for the owner, in the viewer (xterm.js), over the control stream.
//! The shell runs in a pseudo-terminal as this user, in the home folder; it ends when the
//! viewer closes it or the session ends.

use std::fs::File;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::Child;
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::control::ServerMsg;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum ClientTerm {
    Open { cols: u16, rows: u16 },
    /// Keystrokes, base64.
    Input { data: String },
    Resize { cols: u16, rows: u16 },
    Close,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum ServerTerm {
    /// Output, base64.
    Output { data: String },
    Exit { code: Option<i32> },
}

/// Windows needs a ConPTY instead of a pty; not yet.
pub const SUPPORTED: bool = cfg!(unix);

/// Output is sent in pieces of at most this many bytes.
const CHUNK: usize = 16 << 10;

/// A shell in a pseudo-terminal.
struct Shell {
    master: File,
    child: Arc<Mutex<Child>>,
}

#[cfg(not(unix))]
impl Shell {
    fn spawn(program: &str, args: &[&str], cols: u16, rows: u16) -> Result<(Self, File)> {
        let _ = (program, args, cols, rows);
        anyhow::bail!("the terminal isn't available on Windows yet")
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let _ = (cols, rows);
        Ok(())
    }
}

#[cfg(unix)]
impl Shell {
    fn spawn(program: &str, args: &[&str], cols: u16, rows: u16) -> Result<(Self, File)> {
        let (master, slave) = openpty(cols, rows)?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(crate::home())
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // A new session with the pty as its controlling terminal, so job control and
        // Ctrl+C work like in any terminal.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().with_context(|| format!("start {program}"))?;
        let master = File::from(master);
        let reader = master.try_clone()?;
        Ok((
            Self {
                master,
                child: Arc::new(Mutex::new(child)),
            },
            reader,
        ))
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let ws = winsize(cols, rows);
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap();
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(unix)]
fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

#[cfg(unix)]
fn openpty(cols: u16, rows: u16) -> Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (0, 0);
    let mut ws = winsize(cols, rows);
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if r < 0 {
        return Err(std::io::Error::last_os_error()).context("openpty");
    }
    unsafe {
        // The shell gets the slave side as stdio only.
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC);
        Ok((OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)))
    }
}

/// The user's login shell.
fn login_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| std::path::Path::new(s).exists())
        .or_else(|| {
            ["/bin/zsh", "/bin/bash", "/bin/sh"]
                .into_iter()
                .find(|s| std::path::Path::new(s).exists())
                .map(String::from)
        })
        .unwrap_or_else(|| "/bin/sh".into())
}

/// Reads the pty until the shell exits, then reports its exit code.
fn pump(mut reader: File, child: Arc<Mutex<Child>>, out: UnboundedSender<ServerMsg>) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let data = B64.encode(&buf[..n]);
                if out.send(ServerMsg::Term(ServerTerm::Output { data })).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // EIO once the shell (the last holder of the slave side) has exited.
            Err(_) => break,
        }
    }
    let code = child.lock().unwrap().wait().ok().and_then(|s| s.code());
    let _ = out.send(ServerMsg::Term(ServerTerm::Exit { code }));
}

/// One session's shell, if it opened one. Dropping it ends the shell.
#[derive(Default)]
pub struct Session {
    shell: Option<Shell>,
}

impl Session {
    /// Handles one message; output goes to `out`.
    pub fn handle(&mut self, msg: ClientTerm, out: UnboundedSender<ServerMsg>) -> Result<()> {
        match msg {
            ClientTerm::Open { cols, rows } => {
                // One shell per session: opening again starts a fresh one.
                self.shell = None;
                let shell = login_shell();
                let (s, reader) = Shell::spawn(&shell, &["-l"], cols, rows)?;
                let child = s.child.clone();
                std::thread::spawn(move || pump(reader, child, out));
                tracing::info!("terminal: started {shell}");
                self.shell = Some(s);
            }
            ClientTerm::Input { data } => {
                let shell = self.shell.as_mut().context("no terminal open")?;
                let bytes = B64.decode(data.as_bytes()).context("bad terminal input")?;
                shell.master.write_all(&bytes)?;
            }
            ClientTerm::Resize { cols, rows } => {
                if let Some(s) = &self.shell {
                    s.resize(cols, rows)?;
                }
            }
            ClientTerm::Close => self.shell = None,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn collect(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<ServerMsg>,
    ) -> (String, Option<i32>) {
        let mut text = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(ServerMsg::Term(ServerTerm::Output { data })) => {
                    text.extend(B64.decode(data).unwrap())
                }
                Ok(ServerMsg::Term(ServerTerm::Exit { code })) => {
                    return (String::from_utf8_lossy(&text).into_owned(), code);
                }
                Ok(_) => {}
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        panic!("no exit; got {:?}", String::from_utf8_lossy(&text));
    }

    #[test]
    #[cfg(unix)]
    fn runs_a_program_in_a_pty() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (shell, reader) = Shell::spawn(
            "/bin/sh",
            &["-c", "stty size; printf glass-ok; exit 3"],
            100,
            30,
        )
        .unwrap();
        let child = shell.child.clone();
        std::thread::spawn(move || pump(reader, child, tx));
        let (text, code) = collect(&mut rx);
        assert!(text.contains("30 100"), "{text:?}");
        assert!(text.contains("glass-ok"), "{text:?}");
        assert_eq!(code, Some(3));
        drop(shell);
    }

    #[test]
    #[cfg(unix)]
    fn input_reaches_the_shell() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut shell, reader) = Shell::spawn("/bin/sh", &[], 80, 24).unwrap();
        let child = shell.child.clone();
        std::thread::spawn(move || pump(reader, child, tx));
        shell.resize(120, 40).unwrap();
        shell
            .master
            .write_all(b"stty size; echo done-$((40+2)); exit\n")
            .unwrap();
        let (text, _) = collect(&mut rx);
        assert!(text.contains("40 120"), "{text:?}");
        assert!(text.contains("done-42"), "{text:?}");
    }

    #[test]
    fn messages_parse() {
        let m: ClientTerm = serde_json::from_str(r#"{"op":"open","cols":80,"rows":24}"#).unwrap();
        assert!(matches!(m, ClientTerm::Open { cols: 80, rows: 24 }));
        let s =
            serde_json::to_string(&ServerMsg::Term(ServerTerm::Exit { code: Some(0) })).unwrap();
        assert_eq!(s, r#"{"t":"term","op":"exit","code":0}"#);
    }
}
