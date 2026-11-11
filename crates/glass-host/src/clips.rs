//! Screen recordings agents ask for (`record`): for some seconds, or started and stopped
//! by id. Like voxl's `/record` (`GET /record?seconds=N`, `POST /record/start`, `POST
//! /record/stop?id=`; adapted from voxl, MIT, github.com/tembo/voxl).
//!
//! A recording is the H.264 the host already encodes for viewers, rewrapped as MP4 by
//! crate::recording: nothing is re-encoded. It's what's streamed first (the main display
//! unless a viewer chose another), with the pointer, and frames only when the screen
//! changes. Files go to the config directory's `agent-recordings` and are removed after a
//! day.
//!
//! Like screenshots, an agent under a grant doesn't see protected apps: the recording ends
//! and is deleted if one comes to the front.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};

use crate::computer::Refused;

/// The longest `record` with `seconds` (the call waits for it).
pub const MAX_SECONDS: f64 = 60.0;
/// A started recording ends by itself after this.
pub const MAX_LENGTH: Duration = Duration::from_secs(600);
/// A recording nobody stopped is forgotten this long after it ended (its file stays a day).
const UNCLAIMED: Duration = Duration::from_secs(3600);
/// Recordings at most this big come back inline (as base64); bigger ones as a path only.
pub const INLINE_MAX: u64 = 20 * 1024 * 1024;
/// Recordings running at once, for everyone, and for one agent.
const MAX_ACTIVE: usize = 4;
const MAX_ACTIVE_EACH: usize = 2;
/// Old recordings are removed when a new one starts.
const KEEP: Duration = Duration::from_secs(24 * 3600);
/// How often a running recording checks it may go on.
const CHECK_EVERY: Duration = Duration::from_millis(500);

struct Active {
    /// Who started it (`computer::asker`): only they stop it.
    asker: String,
    started: Instant,
    display: Option<u32>,
    recording: Option<crate::recording::Recording>,
    /// Why it ended early, with its files deleted.
    ended: Option<String>,
    /// How long it ran, once it ended by itself.
    ran: Option<Duration>,
    made: Option<crate::recording::Made>,
}

static ACTIVE: Mutex<Option<HashMap<u64, Active>>> = Mutex::new(None);

fn active<R>(f: impl FnOnce(&mut HashMap<u64, Active>) -> R) -> R {
    f(ACTIVE.lock().unwrap().get_or_insert_with(HashMap::new))
}

fn dir() -> PathBuf {
    crate::config_dir().join("agent-recordings")
}

/// How long `record` with `seconds` records: `secs`, at most MAX_SECONDS.
pub fn record_len(secs: f64) -> Result<Duration, Refused> {
    if secs.is_nan() || secs <= 0.0 {
        return Err(Refused("seconds must be more than 0".into()));
    }
    // Capped before converting: from_secs_f64 panics on infinite or huge values.
    Ok(Duration::from_secs_f64(secs.min(MAX_SECONDS)))
}

/// Removes recordings in `dir` older than `keep`.
fn prune(dir: &Path, keep: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > keep);
        if old && e.path().extension().is_some_and(|x| x == "mp4") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Starts a recording; its id.
pub async fn start() -> Result<Value, Refused> {
    let id = begin().await?;
    Ok(json!({ "id": id, "recording": true, "max_seconds": MAX_LENGTH.as_secs() }))
}

/// Records for `len`, then returns it.
pub async fn record(len: Duration) -> Result<Value, Refused> {
    let id = begin().await?;
    let until = Instant::now() + len;
    while Instant::now() < until && active(|a| a.get(&id).is_some_and(|x| x.recording.is_some())) {
        tokio::time::sleep(CHECK_EVERY.min(until.saturating_duration_since(Instant::now()))).await;
    }
    finish(id).await
}

/// Stops recording `id` (this agent's) and returns it.
pub async fn stop(id: u64) -> Result<Value, Refused> {
    let who = crate::computer::asker();
    if !active(|a| a.get(&id).is_some_and(|x| x.asker == who)) {
        return Err(Refused(format!("No recording {id} of yours is running; start one with record.")));
    }
    finish(id).await
}

async fn begin() -> Result<u64, Refused> {
    crate::computer::may_act(true).await?;
    crate::computer::screen_visible()?;
    let who = crate::computer::asker();
    let (all, mine) = active(|a| (a.values().filter(|x| x.recording.is_some()).count(), a.values().filter(|x| x.recording.is_some() && x.asker == who).count()));
    if mine >= MAX_ACTIVE_EACH {
        return Err(Refused(format!("{mine} of your recordings are running; stop one first.")));
    }
    if all >= MAX_ACTIVE {
        return Err(Refused("Too many recordings are running; try again in a while.".into()));
    }
    let name = match crate::grants::current() {
        crate::grants::Actor::Agent(g) => format!("agent {}", g.client_name),
        crate::grants::Actor::Owner => "agent".into(),
    };
    let rec = tokio::task::spawn_blocking(move || {
        let dir = dir();
        prune(&dir, KEEP);
        crate::recording::start_in(&dir, &name)
    })
    .await
    .map_err(|e| Refused(e.to_string()))?
    .map_err(|e| Refused(format!("can't record: {e:#}")))?;
    let display = crate::server::shared().and_then(|s| s.video.streams().first().map(|d| d.id));
    let id = new_id();
    active(|a| {
        a.insert(id, Active { asker: who, started: Instant::now(), display, recording: Some(rec), ended: None, ran: None, made: None })
    });
    crate::computer::announce("record", format!("started recording {}", display.map(|d| format!("display {d}")).unwrap_or_default()), None);
    crate::testmode::record("agent", json!({ "record": "start", "id": id }));
    tokio::spawn(watch(id, crate::grants::current()));
    Ok(id)
}

fn new_id() -> u64 {
    let mut b = [0u8; 8];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b).expect("random");
    // Within what JavaScript numbers hold exactly.
    (u64::from_le_bytes(b) >> 12).max(1)
}

/// Keeps an eye on a running recording: it ends at MAX_LENGTH, and ends and is deleted
/// when the owner stops agents or a protected app comes in front (for an agent under a
/// grant). Forgotten if nobody collects it.
async fn watch(id: u64, actor: crate::grants::Actor) {
    loop {
        tokio::time::sleep(CHECK_EVERY).await;
        let Some(started) = active(|a| a.get(&id).filter(|x| x.recording.is_some()).map(|x| x.started)) else { break };
        let why = if crate::computer::status().stopped {
            Some("The user stopped the agent, so the recording was deleted.".to_string())
        } else {
            crate::grants::ACTOR.scope(actor.clone(), async { crate::computer::screen_visible() }).await.err().map(|Refused(why)| format!("{why} The recording was deleted."))
        };
        if let Some(why) = why {
            let rec = active(|a| a.get_mut(&id).and_then(|x| {
                x.ended = Some(why.clone());
                x.recording.take()
            }));
            if let Some(rec) = rec {
                let made = tokio::task::spawn_blocking(move || rec.finish()).await.unwrap_or_default();
                for f in made.files {
                    let _ = std::fs::remove_file(f);
                }
                tracing::info!("agent recording {id} ended early: {why}");
            }
            break;
        }
        if started.elapsed() >= MAX_LENGTH {
            let rec = active(|a| a.get_mut(&id).and_then(|x| x.recording.take()));
            if let Some(rec) = rec {
                let made = tokio::task::spawn_blocking(move || rec.finish()).await.unwrap_or_default();
                active(|a| {
                    if let Some(x) = a.get_mut(&id) {
                        x.made = Some(made);
                        x.ran = Some(started.elapsed());
                    }
                });
                tracing::info!("agent recording {id} reached {} s; ended", MAX_LENGTH.as_secs());
            }
            break;
        }
    }
    tokio::time::sleep(UNCLAIMED).await;
    active(|a| a.remove(&id));
}

/// Ends recording `id` if it's still going, and says what it made.
async fn finish(id: u64) -> Result<Value, Refused> {
    let Some(x) = active(|a| a.remove(&id)) else {
        return Err(Refused(format!("No recording {id} is running.")));
    };
    if let Some(why) = x.ended {
        return Err(Refused(why));
    }
    let ran = x.ran.unwrap_or_else(|| x.started.elapsed());
    let made = match (x.recording, x.made) {
        (Some(rec), _) => tokio::task::spawn_blocking(move || rec.finish()).await.map_err(|e| Refused(e.to_string()))?,
        (None, Some(made)) => made,
        (None, None) => return Err(Refused(format!("Recording {id} is gone."))),
    };
    crate::computer::announce("record", format!("stopped recording after {:.0} s", ran.as_secs_f64()), None);
    crate::testmode::record("agent", json!({ "record": "stop", "id": id, "files": made.files.len() }));
    clip(id, x.display, ran, &made)
}

/// A finished recording as agents get it: where it is, and the MP4 itself when it's small.
fn clip(id: u64, display: Option<u32>, ran: Duration, made: &crate::recording::Made) -> Result<Value, Refused> {
    if made.files.is_empty() {
        return Err(Refused(if made.hevc {
            "Nothing was recorded: the screen is being sent in HEVC, which recordings don't keep. Try again when no viewer is using HEVC.".into()
        } else {
            "Nothing was recorded: no picture came in time. Try a longer recording.".into()
        }));
    }
    let sizes: Vec<u64> = made.files.iter().map(|f| std::fs::metadata(f).map_or(0, |m| m.len())).collect();
    let bytes: u64 = sizes.iter().sum();
    let mut v = json!({
        "id": id,
        "display": display,
        "duration_ms": ran.as_millis() as u64,
        "path": made.files[0],
        "parts": made.files,
        "bytes": bytes,
        "mime": "video/mp4",
    });
    // One part, small enough: the file itself (an agent may be on another computer).
    if made.files.len() == 1 && bytes <= INLINE_MAX {
        let data = std::fs::read(&made.files[0]).map_err(|e| Refused(format!("can't read the recording: {e}")))?;
        v["mp4"] = json!(base64::engine::general_purpose::STANDARD.encode(data));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_are_positive_and_capped() {
        assert_eq!(record_len(2.5).unwrap(), Duration::from_millis(2500));
        assert_eq!(record_len(1e100).unwrap(), Duration::from_secs_f64(MAX_SECONDS));
        assert_eq!(record_len(f64::INFINITY).unwrap(), Duration::from_secs_f64(MAX_SECONDS));
        assert!(record_len(0.0).is_err() && record_len(-1.0).is_err() && record_len(f64::NAN).is_err());
    }

    #[test]
    fn small_clips_come_back_inline() {
        let dir = std::env::temp_dir().join(format!("glass-clips-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.mp4");
        std::fs::write(&file, b"not really an mp4").unwrap();
        let made = crate::recording::Made { files: vec![file.clone()], hevc: false };
        let v = clip(7, Some(1), Duration::from_millis(1500), &made).unwrap();
        assert_eq!((v["id"].as_u64(), v["duration_ms"].as_u64(), v["bytes"].as_u64()), (Some(7), Some(1500), Some(17)));
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(v["mp4"].as_str().unwrap()).unwrap(), b"not really an mp4");
        // Two parts: paths only.
        let made = crate::recording::Made { files: vec![file.clone(), file.clone()], hevc: false };
        assert!(clip(7, None, Duration::ZERO, &made).unwrap().get("mp4").is_none());
        // Nothing made: why.
        let hevc = crate::recording::Made { files: vec![], hevc: true };
        assert!(clip(7, None, Duration::ZERO, &hevc).unwrap_err().0.contains("HEVC"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_recordings_are_pruned() {
        let dir = std::env::temp_dir().join(format!("glass-clips-prune-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old.mp4"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        prune(&dir, Duration::from_secs(3600));
        assert!(dir.join("old.mp4").exists(), "new enough");
        std::thread::sleep(Duration::from_millis(20));
        prune(&dir, Duration::from_millis(1));
        assert!(!dir.join("old.mp4").exists() && dir.join("notes.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
