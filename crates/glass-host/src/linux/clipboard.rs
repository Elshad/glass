//! The clipboard via wl-clipboard's `wl-copy` / `wl-paste` on Wayland and `xclip` on X11:
//! text, and images.
//! TODO: rich text (HTML). Both tools offer one type per call, so setting HTML would drop
//! the plain text other apps need; it needs a clipboard owner offering several types.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::broadcast;

use crate::clip::{Clip, MAX_IMAGE};
use crate::control::ServerMsg;

const POLL: Duration = Duration::from_millis(500);

/// What we last put on (or saw on) the clipboard, so our own writes aren't echoed back:
/// the text, or for an image its size.
static LAST: Mutex<Option<String>> = Mutex::new(None);

/// X11 names UTF-8 text by its own target, not by a MIME type.
fn x11_target(mime: &str) -> &str {
    if mime.starts_with("text/plain") { "UTF8_STRING" } else { mime }
}

fn copy(bytes: &[u8], mime: &str) {
    // Both tools fork a process that keeps serving the selection after we return.
    let mut command = if super::is_x11() {
        let mut c = Command::new("xclip");
        c.args(["-selection", "clipboard", "-in", "-target", x11_target(mime)]);
        c
    } else {
        let mut c = Command::new("wl-copy");
        c.args(["--type", mime]);
        c
    };
    let child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match child {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(bytes);
            }
            let _ = child.wait();
        }
        Err(e) if super::is_x11() => tracing::warn!("xclip: {e} (install xclip)"),
        Err(e) => tracing::warn!("wl-copy: {e} (install wl-clipboard)"),
    }
}

pub fn set_text(text: &str) {
    *LAST.lock().unwrap() = Some(text.to_owned());
    copy(text.as_bytes(), "text/plain;charset=utf-8");
}

/// An image if there is one (as given), else the text.
pub fn set(c: &Clip) {
    match &c.image {
        Some(image) => {
            let mime = if crate::clip::is_png(image) { "image/png" } else { "image/jpeg" };
            copy(image, mime);
            *LAST.lock().unwrap() = current().map(|(k, _)| k);
        }
        None => set_text(&c.text),
    }
}

fn image_marker(len: usize) -> String {
    format!("\u{0}image:{len}")
}

fn paste(mime: &str) -> Option<Vec<u8>> {
    let mut command = if super::is_x11() {
        let mut c = Command::new("xclip");
        c.args(["-selection", "clipboard", "-out", "-target", x11_target(mime)]);
        c
    } else {
        let mut c = Command::new("wl-paste");
        c.args(["--no-newline", "--type", mime]);
        c
    };
    let out = command.stderr(Stdio::null()).output().ok()?;
    out.status.success().then_some(out.stdout)
}

fn types() -> Vec<String> {
    paste_types().unwrap_or_default()
}

fn paste_types() -> Option<Vec<String>> {
    let mut command = if super::is_x11() {
        let mut c = Command::new("xclip");
        c.args(["-selection", "clipboard", "-out", "-target", "TARGETS"]);
        c
    } else {
        let mut c = Command::new("wl-paste");
        c.arg("--list-types");
        c
    };
    let out = command.stderr(Stdio::null()).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).lines().map(str::to_owned).collect())
}

pub fn image_png() -> Option<Vec<u8>> {
    types().iter().any(|t| t == "image/png").then(|| paste("image/png")).flatten().filter(|b| b.len() <= MAX_IMAGE)
}

/// What's on the clipboard now, for change detection: the text, or an image marker. An
/// image is only read when asked for (`image_png`), so polling stays cheap; one image
/// replaced by another with the same offered types isn't noticed until the text changes.
fn current() -> Option<(String, bool)> {
    let types = types();
    if types.iter().any(|t| t == "image/png") {
        return Some((image_marker(types.len()), true));
    }
    let text = paste("text/plain").map(|b| String::from_utf8_lossy(&b).into_owned())?;
    Some((text, false))
}

/// Broadcasts clipboard changes.
pub fn watch(events: broadcast::Sender<ServerMsg>) {
    std::thread::spawn(move || {
        *LAST.lock().unwrap() = current().map(|(k, _)| k);
        loop {
            std::thread::sleep(POLL);
            let Some((key, image)) = current() else { continue };
            let mut last = LAST.lock().unwrap();
            if last.as_deref() == Some(key.as_str()) {
                continue;
            }
            *last = Some(key.clone());
            drop(last);
            let text = if image { String::new() } else { key };
            let _ = events.send(ServerMsg::Clipboard { text, html: None, image });
        }
    });
}

/// The real system clipboard.
pub struct SystemClipboard;

impl crate::media::Clipboard for SystemClipboard {
    fn set_text(&self, text: &str) {
        set_text(text)
    }

    fn set(&self, c: &Clip) {
        set(c)
    }

    fn image_png(&self) -> Option<Vec<u8>> {
        image_png()
    }
}
