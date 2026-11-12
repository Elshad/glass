//! What the computer is playing (a track in Spotify, Music, a browser…), so a viewer's
//! media controls (Chrome's media hub, the iPad lock screen) show it and control it.
//!
//! macOS: a small helper (native/now-playing) loaded into /usr/bin/perl, since macOS only
//! answers MediaRemote for its own processes; it prints a JSON line per change and takes
//! commands on stdin. Linux: `playerctl` (MPRIS). Windows: the system's media sessions
//! (GlobalSystemMediaTransportControlsSessionManager, what the volume flyout shows), on a
//! thread. Runs only while someone is connected.

use std::io::{BufRead, BufReader, Write};

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::control::ServerMsg;

enum Helper {
    /// The macOS helper, playerctl, or the test source.
    Process(Child),
    /// Windows: a thread following the media sessions, until this is set.
    #[cfg(windows)]
    Thread(std::sync::Arc<std::sync::atomic::AtomicBool>),
}

impl Drop for Helper {
    fn drop(&mut self) {
        match self {
            Helper::Process(child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            #[cfg(windows)]
            Helper::Thread(stop) => stop.store(true, std::sync::atomic::Ordering::Relaxed),
        }
    }
}

static HELPER: Mutex<Option<Helper>> = Mutex::new(None);
/// The latest report (artwork kept from the last report that carried one).
static LATEST: Mutex<Option<Value>> = Mutex::new(None);

/// What's playing now, for a viewer that just connected.
pub fn current() -> Option<Value> {
    LATEST.lock().unwrap().clone()
}

/// Starts the helper while anyone is connected, stops it when nobody is.
pub fn watch(events: broadcast::Sender<ServerMsg>) {
    let mut viewers = crate::viewers::subscribe();
    tokio::spawn(async move {
        loop {
            let connected = !viewers.borrow_and_update().is_empty();
            let running = HELPER.lock().unwrap().is_some();
            if connected && !running {
                start(events.clone());
            } else if !connected && running {
                stop();
            }
            if viewers.changed().await.is_err() {
                return;
            }
        }
    });
}

fn stop() {
    HELPER.lock().unwrap().take();
    *LATEST.lock().unwrap() = None;
}

/// How a source's lines become reports.
type Reader = fn(&str) -> Option<Value>;

fn start(events: broadcast::Sender<ServerMsg>) {
    #[cfg(windows)]
    if !crate::testmode::enabled() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        *HELPER.lock().unwrap() = Some(Helper::Thread(stop.clone()));
        std::thread::spawn(move || {
            win::run(&stop, |info| {
                if !stop.load(Ordering::Relaxed) {
                    report(info, &events);
                }
            })
        });
        return;
    }
    let spawned = if crate::testmode::enabled() { test_source().map(|c| (c, parse as Reader)) } else { spawn() };
    let (mut child, read) = match spawned {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("now playing: unavailable ({e:#})");
            return;
        }
    };
    let Some(out) = child.stdout.take() else { return };
    *HELPER.lock().unwrap() = Some(Helper::Process(child));
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            let Some(info) = read(&line) else { continue };
            report(info, &events);
        }
    });
}

/// Keeps a report as the latest and sends it to viewers.
fn report(mut info: Value, events: &broadcast::Sender<ServerMsg>) {
    let mut latest = LATEST.lock().unwrap();
    // A report without artwork keeps the last artwork, if it's the same track's.
    if info.get("artwork").is_none()
        && let Some(prev) = latest.as_ref()
        && prev["artworkId"] == info["artworkId"]
        && let Some(art) = prev.get("artwork")
    {
        info["artwork"] = art.clone();
        info["artworkType"] = prev["artworkType"].clone();
    }
    let send = strip_unchanged_artwork(&info, latest.as_ref());
    *latest = Some(info);
    drop(latest);
    let _ = events.send(ServerMsg::NowPlaying { info: send });
}

/// Sends the artwork only when it changed (it's the big part).
fn strip_unchanged_artwork(info: &Value, prev: Option<&Value>) -> Value {
    let mut out = info.clone();
    if prev.is_some_and(|p| p["artworkId"] == info["artworkId"] && p.get("artwork").is_some())
        && let Some(o) = out.as_object_mut()
    {
        o.remove("artwork");
        o.remove("artworkType");
    }
    out
}

#[cfg(target_os = "macos")]
fn spawn() -> anyhow::Result<(Child, Reader)> {
    const SCRIPT: &str = r#"use DynaLoader; my $l = DynaLoader::dl_load_file($ARGV[0]) or die DynaLoader::dl_error(); my $s = DynaLoader::dl_find_symbol($l, "glass_now_playing") or die "no symbol"; DynaLoader::dl_install_xsub("main::run", $s); run();"#;
    let lib = helper_path().ok_or_else(|| anyhow::anyhow!("no now-playing helper in this build"))?;
    let child = Command::new("/usr/bin/perl")
        .args(["-e", SCRIPT])
        .arg(lib)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    Ok((child, parse))
}

/// `<prefix>/share/glass/libglassnowplaying.dylib`, or a checkout's build in target/.
#[cfg(target_os = "macos")]
fn helper_path() -> Option<std::path::PathBuf> {
    const NAME: &str = "libglassnowplaying.dylib";
    if let Some(p) = std::env::var_os("GLASS_NOW_PLAYING_HELPER") {
        return Some(p.into());
    }
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let root = exe.parent()?.parent()?;
    [root.join("share/glass").join(NAME), root.join("now-playing").join(NAME)]
        .into_iter()
        .find(|p| p.exists())
}

/// Linux: MPRIS through `playerctl`, one tab-separated line per change.
#[cfg(target_os = "linux")]
fn spawn() -> anyhow::Result<(Child, Reader)> {
    let format = "{{status}}\t{{title}}\t{{artist}}\t{{album}}\t{{mpris:length}}\t{{position}}\t{{mpris:trackid}}";
    let child = Command::new("playerctl")
        .args(["--follow", "metadata", "--format", format])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    Ok((child, parse_playerctl))
}

#[cfg(target_os = "linux")]
fn parse_playerctl(line: &str) -> Option<Value> {
    let f: Vec<&str> = line.split('\t').collect();
    let secs = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).map(|us| us / 1e6);
    let text = |i: usize| f.get(i).filter(|s| !s.is_empty()).map(|s| s.to_string());
    let playing = f.first() == Some(&"Playing");
    parse(
        &json!({
            "playing": playing,
            "rate": if playing { 1 } else { 0 },
            "title": text(1),
            "artist": text(2),
            "album": text(3),
            "duration": secs(4),
            "elapsed": secs(5),
            "artworkId": f.get(6).unwrap_or(&""),
        })
        .to_string(),
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn spawn() -> anyhow::Result<(Child, Reader)> {
    anyhow::bail!("not supported here")
}

/// Tests: one fixed track; commands are recorded.
fn test_source() -> anyhow::Result<Child> {
    let track = json!({
        "title": "Gravity", "artist": "John Mayer", "album": "Continuum",
        "playing": true, "rate": 1, "duration": 245.0, "elapsed": 12.0, "artworkId": "test",
        "artwork": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==",
        "artworkType": "image/png",
    });
    Ok(Command::new("sh")
        .args(["-c", &format!("printf '%s\\n' '{track}'; cat >/dev/null")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?)
}

fn parse(line: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(line).ok()?;
    // Nothing playing at all (no app has a track): say so as `null`-ish.
    if v.get("title").is_none_or(Value::is_null) && v.get("artist").is_none_or(Value::is_null) {
        return Some(json!({ "artworkId": "" }));
    }
    Some(v)
}

/// A media control from a viewer: play, pause, toggle, next, previous, or seek.
pub fn command(action: &str, position: Option<f64>) {
    let line = match action {
        "play" | "pause" | "toggle" | "next" | "previous" => action.to_string(),
        "seek" => match position {
            Some(p) if p.is_finite() && p >= 0.0 => format!("seek {p:.3}"),
            _ => return,
        },
        _ => return,
    };
    if crate::testmode::enabled() {
        crate::testmode::record("media", json!(line));
        return;
    }
    #[cfg(target_os = "macos")]
    if let Some(Helper::Process(child)) = HELPER.lock().unwrap().as_mut()
        && let Some(stdin) = child.stdin.as_mut()
    {
        let _ = writeln!(stdin, "{line}");
    }
    #[cfg(target_os = "linux")]
    {
        let args: Vec<String> = match line.split_once(' ') {
            Some(("seek", p)) => vec!["position".into(), p.into()],
            _ => vec![if line == "toggle" { "play-pause".into() } else { line.clone() }],
        };
        let _ = Command::new("playerctl").args(&args).status();
    }
    #[cfg(windows)]
    win::command(line);
}

/// What a Windows media session says, in its own units: 100 ns ticks, `updated` as a
/// FILETIME (since 1601).
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
#[derive(Default)]
struct Track {
    app: String,
    title: String,
    artist: String,
    album: String,
    playing: bool,
    rate: Option<f64>,
    start: i64,
    end: i64,
    position: i64,
    updated: i64,
}

/// A Windows track as the report the other platforms make. The position is as of `at`
/// (when the app last said), which viewers carry forward while it plays.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn shape(t: &Track) -> Value {
    use std::hash::{Hash, Hasher};
    const TICKS: f64 = 10_000_000.0;
    /// FILETIME ticks between 1601 and 1970.
    const UNIX_EPOCH: i64 = 116_444_736_000_000_000;
    if t.title.is_empty() && t.artist.is_empty() {
        return json!({ "artworkId": "" });
    }
    // Windows has no track id: the track's own details stand in for one.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (&t.app, &t.title, &t.artist, &t.album).hash(&mut h);
    let mut v = json!({
        "playing": t.playing,
        "rate": if t.playing { t.rate.filter(|r| *r > 0.0).unwrap_or(1.0) } else { 0.0 },
        "artworkId": format!("{:016x}", h.finish()),
    });
    for (key, text) in [("title", &t.title), ("artist", &t.artist), ("album", &t.album)] {
        if !text.is_empty() {
            v[key] = json!(text);
        }
    }
    if t.end > t.start {
        v["duration"] = json!((t.end - t.start) as f64 / TICKS);
        v["elapsed"] = json!((t.position - t.start).clamp(0, t.end - t.start) as f64 / TICKS);
        if t.updated > UNIX_EPOCH {
            v["at"] = json!((t.updated - UNIX_EPOCH) / 10_000);
        }
    }
    v
}

#[cfg(windows)]
mod win {
    //! The system's media sessions: the current one's details, again whenever it says they
    //! changed (or another app takes over), and its transport controls.

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use base64::Engine;
    use serde_json::{Value, json};
    use windows::Foundation::TypedEventHandler;
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSession as Session,
        GlobalSystemMediaTransportControlsSessionManager as Manager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
    };
    use windows::Storage::Streams::DataReader;
    use windows::core::{Result, RuntimeType};

    use super::Track;

    /// Artwork bigger than this isn't sent (as on macOS).
    const ARTWORK_MAX: u64 = 2_000_000;

    fn manager() -> Result<Manager> {
        Manager::RequestAsync()?.join()
    }

    /// An event handler that wakes the watcher (`true`: the current session changed).
    fn nudge<S: RuntimeType + 'static, A: RuntimeType + 'static>(tx: &mpsc::Sender<bool>, session: bool) -> TypedEventHandler<S, A> {
        let tx = tx.clone();
        TypedEventHandler::new(move |_, _| {
            let _ = tx.send(session);
            Ok(())
        })
    }

    /// The current session with our handlers on it, taken off again when dropped.
    struct Watched {
        session: Session,
        app: String,
        tokens: [i64; 3],
    }

    impl Watched {
        fn new(session: Session, tx: &mpsc::Sender<bool>) -> Result<Self> {
            let tokens = [
                session.MediaPropertiesChanged(&nudge(tx, false))?,
                session.PlaybackInfoChanged(&nudge(tx, false))?,
                session.TimelinePropertiesChanged(&nudge(tx, false))?,
            ];
            let app = session.SourceAppUserModelId().map(|s| s.to_string()).unwrap_or_default();
            Ok(Self { session, app, tokens })
        }
    }

    impl Drop for Watched {
        fn drop(&mut self) {
            let _ = self.session.RemoveMediaPropertiesChanged(self.tokens[0]);
            let _ = self.session.RemovePlaybackInfoChanged(self.tokens[1]);
            let _ = self.session.RemoveTimelinePropertiesChanged(self.tokens[2]);
        }
    }

    pub fn run(stop: &AtomicBool, mut report: impl FnMut(Value)) {
        crate::audio::com_init();
        if let Err(e) = follow(stop, &mut report) {
            tracing::debug!("now playing: unavailable ({e})");
        }
    }

    fn follow(stop: &AtomicBool, report: &mut impl FnMut(Value)) -> Result<()> {
        let manager = manager()?;
        let (tx, rx) = mpsc::channel();
        let token = manager.CurrentSessionChanged(&nudge(&tx, true))?;
        let mut watched: Option<Watched> = None;
        let mut session_changed = true;
        let mut last: Option<Value> = None;
        // The track whose artwork viewers have, and the one we last tried (with how often:
        // apps often add it a moment after the track changes, some never do).
        let mut art_sent = String::new();
        let mut art_tried = (String::new(), 0u8);
        while !stop.load(Ordering::Relaxed) {
            if session_changed {
                // The old session's handlers come off first.
                drop(watched.take());
                watched = manager.GetCurrentSession().ok().and_then(|s| Watched::new(s, &tx).ok());
            }
            let session = watched.as_ref().map(|w| &w.session);
            let info = super::shape(&session.and_then(|s| track(s).ok()).unwrap_or_default());
            let id = info["artworkId"].as_str().unwrap_or_default().to_string();
            let mut out = info.clone();
            if !id.is_empty() && id != art_sent {
                if art_tried.0 != id {
                    art_tried = (id.clone(), 0);
                }
                if art_tried.1 < 5 {
                    art_tried.1 += 1;
                    if let Some((data, kind)) = session.and_then(artwork) {
                        out["artwork"] = json!(base64::engine::general_purpose::STANDARD.encode(data));
                        out["artworkType"] = json!(kind);
                        art_sent = id;
                    }
                }
            }
            if last.as_ref() != Some(&info) || out.get("artwork").is_some() {
                report(out);
                last = Some(info);
            }

            session_changed = match rx.recv_timeout(Duration::from_secs(1)) {
                // A track change fires a few events at once: take them together.
                Ok(s) => rx.try_iter().fold(s, |a, b| a || b),
                // Nothing said: check another app hasn't taken over unannounced.
                Err(_) => {
                    let now = manager.GetCurrentSession().ok().and_then(|s| s.SourceAppUserModelId().ok());
                    now.map(|a| a.to_string()) != watched.as_ref().map(|w| w.app.clone())
                }
            };
        }
        let _ = manager.RemoveCurrentSessionChanged(token);
        Ok(())
    }

    fn track(s: &Session) -> Result<Track> {
        let props = s.TryGetMediaPropertiesAsync()?.join()?;
        let playback = s.GetPlaybackInfo()?;
        let timeline = s.GetTimelineProperties()?;
        Ok(Track {
            app: s.SourceAppUserModelId()?.to_string(),
            title: props.Title()?.to_string(),
            artist: props.Artist()?.to_string(),
            album: props.AlbumTitle()?.to_string(),
            playing: playback.PlaybackStatus()? == Status::Playing,
            rate: playback.PlaybackRate().and_then(|r| r.Value()).ok(),
            start: timeline.StartTime()?.Duration,
            end: timeline.EndTime()?.Duration,
            position: timeline.Position()?.Duration,
            updated: timeline.LastUpdatedTime()?.UniversalTime,
        })
    }

    /// The track's picture and its MIME type.
    fn artwork(s: &Session) -> Option<(Vec<u8>, String)> {
        let props = s.TryGetMediaPropertiesAsync().ok()?.join().ok()?;
        let stream = props.Thumbnail().ok()?.OpenReadAsync().ok()?.join().ok()?;
        let size = stream.Size().ok()?;
        if size == 0 || size > ARTWORK_MAX {
            return None;
        }
        let kind = stream.ContentType().map(|t| t.to_string()).ok().filter(|t| t.starts_with("image/"));
        let reader = DataReader::CreateDataReader(&stream).ok()?;
        reader.LoadAsync(size as u32).ok()?.join().ok()?;
        let mut data = vec![0u8; size as usize];
        reader.ReadBytes(&mut data).ok()?;
        Some((data, kind.unwrap_or_else(|| "image/jpeg".into())))
    }

    /// A viewer's play, pause, toggle, next, previous or `seek <seconds>`, to the current
    /// session, off the caller's thread (the app can take a moment to answer).
    pub fn command(line: String) {
        std::thread::spawn(move || {
            crate::audio::com_init();
            if let Err(e) = send(&line) {
                tracing::debug!("now playing: {line}: {e}");
            }
        });
    }

    fn send(line: &str) -> Result<()> {
        let s = manager()?.GetCurrentSession()?;
        let done = match line.split_once(' ') {
            Some(("seek", secs)) => {
                let secs: f64 = secs.parse().unwrap_or(0.0);
                let start = s.GetTimelineProperties()?.StartTime()?.Duration;
                s.TryChangePlaybackPositionAsync(start + (secs * 10_000_000.0) as i64)?
            }
            _ => match line {
                "play" => s.TryPlayAsync()?,
                "pause" => s.TryPauseAsync()?,
                "toggle" => s.TryTogglePlayPauseAsync()?,
                "next" => s.TrySkipNextAsync()?,
                "previous" => s.TrySkipPreviousAsync()?,
                _ => return Ok(()),
            },
        };
        done.join()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_is_sent_once_per_track() {
        let a = json!({ "title": "A", "artworkId": "1", "artwork": "xx" });
        assert!(strip_unchanged_artwork(&a, None).get("artwork").is_some());
        assert!(strip_unchanged_artwork(&a, Some(&a)).get("artwork").is_none());
        let b = json!({ "title": "B", "artworkId": "2", "artwork": "yy" });
        assert!(strip_unchanged_artwork(&b, Some(&a)).get("artwork").is_some());
    }

    #[test]
    fn windows_tracks_look_like_the_others() {
        let t = Track {
            app: "Spotify.exe".into(),
            title: "Gravity".into(),
            artist: "John Mayer".into(),
            playing: true,
            end: 245 * 10_000_000,
            position: 12 * 10_000_000,
            // 2024-01-01T00:00:00Z as a FILETIME.
            updated: 116_444_736_000_000_000 + 1_704_067_200 * 10_000_000,
            ..Default::default()
        };
        let v = shape(&t);
        assert_eq!(v["title"], "Gravity");
        assert_eq!(v["artist"], "John Mayer");
        assert!(v.get("album").is_none(), "empty fields are left out");
        assert_eq!((v["playing"].as_bool(), v["rate"].as_f64()), (Some(true), Some(1.0)));
        assert_eq!((v["duration"].as_f64(), v["elapsed"].as_f64()), (Some(245.0), Some(12.0)));
        assert_eq!(v["at"], 1_704_067_200_000i64);
        // The same track keeps its id; another track gets a new one.
        let id = v["artworkId"].as_str().unwrap().to_string();
        assert!(!id.is_empty());
        assert_eq!(shape(&Track { position: 0, playing: false, ..t }).get("artworkId").unwrap(), id.as_str());
        let paused = shape(&Track { title: "Daughters".into(), ..Default::default() });
        assert_ne!(paused["artworkId"], id.as_str());
        assert_eq!(paused["rate"].as_f64(), Some(0.0));
        assert!(paused.get("duration").is_none() && paused.get("at").is_none(), "no timeline, no position");
        // No session, or one with nothing in it.
        assert_eq!(shape(&Track::default()), json!({ "artworkId": "" }));
    }

    #[test]
    fn nothing_playing_is_empty() {
        assert_eq!(parse("{}").unwrap(), json!({ "artworkId": "" }));
        assert_eq!(parse(r#"{"title":"T","artworkId":"x"}"#).unwrap()["title"], "T");
        assert!(parse("not json").is_none());
    }
}
