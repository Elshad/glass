//! Dynamic resolution: each display is encoded at the size its viewers actually show it,
//! never more (bits wasted, fewer frames) and never less (a stretched, blurry picture).
//! Viewers report the pixels each display gets on their screen (after zoom); a viewer in
//! "This Mac" mode, or one that reports nothing, asks for full resolution. Displays are
//! shared by all viewers, so the largest request wins.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::media::DisplayInfo;

/// What one viewer asked for, per stream: `None` = full resolution, `Some([w, h])` = the
/// device pixels that stream's area covers.
pub type Request = Vec<Option<[u32; 2]>>;

static REQUESTS: Mutex<Option<HashMap<u64, Request>>> = Mutex::new(None);
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Sizes change only after requests have been stable this long (a resize or pinch sends
/// many in a row, and every change restarts the encoder).
const SETTLE: Duration = Duration::from_millis(800);
/// Never encode narrower than this (unless the display itself is).
const MIN_WIDTH: u32 = 640;
/// Encoded widths, as fractions of native: a few steps, so small changes don't restart.
const STEPS: [f64; 7] = [1.0, 0.8, 2.0 / 3.0, 0.5, 0.4, 1.0 / 3.0, 0.25];

pub fn set(session: u64, request: Option<Request>) {
    {
        let mut all = REQUESTS.lock().unwrap();
        let all = all.get_or_insert_with(Default::default);
        match request {
            Some(r) => all.insert(session, r),
            None => all.remove(&session),
        };
    }
    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    tokio::spawn(async move {
        tokio::time::sleep(SETTLE).await;
        if GENERATION.load(std::sync::atomic::Ordering::Relaxed) != generation {
            return; // a newer request will apply
        }
        if let Some(shared) = crate::server::shared() {
            let streams = shared.video.streams();
            let widths = widths(&streams, &snapshot());
            tracing::info!("resolution: encoding widths {widths:?} (None = native)");
            // Restarting capture blocks on ScreenCaptureKit.
            let _ =
                tokio::task::spawn_blocking(move || shared.video.set_target_widths(&widths)).await;
        }
    });
}

/// A new viewer's first size, at once: its first keyframe waits for this, and it would
/// otherwise cross a slow link at the wrong size just before the right one.
pub async fn set_now(session: u64, request: Request) {
    REQUESTS.lock().unwrap().get_or_insert_with(Default::default).insert(session, request);
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some(shared) = crate::server::shared() {
        let widths = widths(&shared.video.streams(), &snapshot());
        tracing::info!("resolution: encoding widths {widths:?} (None = native)");
        let _ = tokio::task::spawn_blocking(move || shared.video.set_target_widths(&widths)).await;
    }
}

fn snapshot() -> Vec<Request> {
    REQUESTS
        .lock()
        .unwrap()
        .as_ref()
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default()
}

/// The width to encode each stream at (`None` = native).
pub fn widths(streams: &[DisplayInfo], requests: &[Request]) -> Vec<Option<u32>> {
    streams
        .iter()
        .enumerate()
        .map(|(i, d)| {
            if requests.is_empty() {
                return None;
            }
            let mut needed = 0u32;
            for r in requests {
                match r.get(i).copied().flatten() {
                    // A viewer wants it full size (or hasn't said): native.
                    None => return None,
                    Some([w, h]) => {
                        // The picture is letterboxed into the area: its width is limited by
                        // whichever side runs out first.
                        let aspect = d.width as f64 / d.height.max(1) as f64;
                        let fit = (w as f64).min(h as f64 * aspect);
                        needed = needed.max(fit.ceil() as u32);
                    }
                }
            }
            snap(d.width as u32, needed)
        })
        .collect()
}

/// The smallest step at least `needed` wide (`None` when that's native).
fn snap(native: u32, needed: u32) -> Option<u32> {
    // Test displays are tiny (640 px); let tests see them shrink.
    let min = if crate::testmode::enabled() {
        160
    } else {
        MIN_WIDTH
    };
    let floor = min.min(native);
    let mut best = native;
    for f in STEPS {
        let w = ((native as f64 * f) as u32).max(floor) & !1;
        if w >= needed && w < best {
            best = w;
        }
    }
    (best < native).then_some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(w: u32, h: u32) -> DisplayInfo {
        DisplayInfo {
            id: 1,
            width: w as usize,
            height: h as usize,
            x: 0.0,
            y: 0.0,
            points_width: w as f64,
            points_height: h as f64,
            main: true,
            builtin: false,
        }
    }

    #[test]
    fn sizes_follow_the_largest_viewer() {
        let mac = [display(2560, 1440)];
        // A phone showing it ~1170 px wide: the 1280 step (0.5) covers it.
        assert_eq!(widths(&mac, &[vec![Some([1170, 2532])]]), vec![Some(1280)]);
        // An iPad at 2388 px: 0.8 step is 2048, too small, so native.
        assert_eq!(widths(&mac, &[vec![Some([2388, 1668])]]), vec![None]);
        // Two viewers: the larger need wins.
        let both = [vec![Some([1170, 2532])], vec![Some([1700, 956])]];
        assert_eq!(widths(&mac, &both), vec![Some(1706)]);
        // Anyone in "This Mac" mode (or silent) gets native for everyone.
        assert_eq!(
            widths(&mac, &[vec![Some([800, 450])], vec![None]]),
            vec![None]
        );
        assert_eq!(widths(&mac, &[]), vec![None]);
    }

    #[test]
    fn tall_areas_are_limited_by_height() {
        let mac = [display(2560, 1440)];
        // 2000 px wide but only 400 tall: the picture is 711 px wide.
        assert_eq!(widths(&mac, &[vec![Some([2000, 400])]]), vec![Some(852)]);
    }

    #[test]
    fn never_below_the_minimum() {
        assert_eq!(snap(2560, 100), Some(640));
        assert_eq!(snap(640, 100), None, "already at the minimum");
    }
}
