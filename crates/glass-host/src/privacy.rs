//! Privacy mode: while someone is connected, this computer's own screen goes black and
//! its keyboard and mouse are ignored, so nobody in the room sees or interferes. Capture
//! is unaffected: viewers still see the desktop.
//!
//! On when `glass config privacy on` (whenever anyone is viewing), when an owner turns it
//! on from the viewer, or with `glass privacy on`. Always off once nobody is connected.

use std::sync::Mutex;

struct State {
    /// Set by the CLI or a viewer; `None` follows the setting.
    forced: Option<bool>,
    viewers: usize,
    engaged: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    forced: None,
    viewers: 0,
    engaged: false,
});

pub fn supported() -> bool {
    // Windows: it depends on the build (10 2004 or later), not just the target.
    #[cfg(windows)]
    let here = crate::platform::privacy::supported();
    #[cfg(not(windows))]
    let here = crate::platform::privacy::SUPPORTED;
    crate::testmode::enabled() || here
}

/// Turns it on or off for as long as anyone stays connected.
pub fn set_forced(on: bool) -> anyhow::Result<()> {
    anyhow::ensure!(supported(), "privacy mode isn't available on this computer yet");
    STATE.lock().unwrap().forced = Some(on);
    apply()
}

/// Called when the number of signed-in viewers changes.
pub fn viewers_changed(n: usize) {
    let mut s = STATE.lock().unwrap();
    s.viewers = n;
    if n == 0 {
        s.forced = None;
    }
    drop(s);
    if let Err(e) = apply() {
        tracing::warn!("privacy mode: {e:#}");
    }
}

pub fn active() -> bool {
    STATE.lock().unwrap().engaged
}

fn apply() -> anyhow::Result<()> {
    let mut s = STATE.lock().unwrap();
    let want = s.viewers > 0 && s.forced.unwrap_or_else(|| crate::settings::load().privacy);
    if want == s.engaged {
        return Ok(());
    }
    if crate::testmode::enabled() {
        crate::testmode::record_privacy(want);
    } else {
        crate::platform::privacy::set(want)?;
    }
    s.engaged = want;
    tracing::info!("privacy mode {}", if want { "on" } else { "off" });
    drop(s);
    if let Some(shared) = crate::server::shared() {
        let _ = shared.events.send(crate::control::ServerMsg::Privacy { on: want });
    }
    Ok(())
}
