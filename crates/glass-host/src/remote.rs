//! Remote access without Tailscale: a browser anywhere reaches this host over WebRTC — a
//! direct, end-to-end encrypted UDP connection (relayed by TURN only when a direct path is
//! impossible). glassdesk.dev only introduces the two sides; the host keeps one outgoing
//! WebSocket to it and opens no ports.
//!
//! The host has its own key pair. Links carry the public key, and the host signs every
//! answer it gives, so a browser knows it reached *this* host even if the introducer were
//! compromised (the signature covers both sides' DTLS fingerprints).
//!
//! Each WebRTC peer is bridged to the ordinary WebTransport server on a local address
//! (loopback, unless `--bind` doesn't include it), so sessions, sign-in and everything
//! else work exactly as on the tailnet:
//! - data channel "d" (unordered, no retransmits) ⇄ datagrams
//! - a channel per stream: "bi" ⇄ bidirectional stream, "uni" ⇄ unidirectional stream
//!   (the host opens "uni" channels for streams it starts). A text message "end" finishes
//!   the stream.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelInit};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCConfigurationBuilder, RTCIceGatheringState, RTCIceServer, RTCPeerConnectionIceEvent,
    RTCPeerConnectionState, RTCSessionDescription, Registry, register_default_interceptors,
};

/// Largest data channel message we send (browsers accept far more; this keeps latency even).
const CHUNK: usize = 16 * 1024;
/// Datagrams are dropped rather than queued past this, so video never lags behind.
const DATAGRAM_BACKLOG: usize = 256 * 1024;
/// Video waiting in the WebRTC send buffer is held to about this much time at the current
/// bitrate. A byte cap alone let a slow relayed link (2–3 Mbps through TURN) queue most of
/// a second of frames, so latency swung between fine and awful as the queue filled and
/// drained.
const VIDEO_QUEUE_MS: u64 = 120;
/// The least queue allowed, so a frame or two always fits at low bitrates.
const MIN_VIDEO_BACKLOG: usize = 32 * 1024;

/// What the video data channel actually delivers, turned into a bitrate cap. Video over
/// the data channel (browsers without RTP H.264, like a Tesla's) has no congestion control
/// of its own, and the shared rate controller won't go below a fraction of its maximum:
/// on a relay that carries less than that, every frame queued, then dropped.
#[derive(Default)]
struct LinkMeter {
    /// Bytes handed to the channel since the last tick.
    sent: u64,
    last_outstanding: usize,
    cap: Option<u32>,
    /// Ticks in a row without backing up.
    calm: u32,
}

/// The send buffer's floor: bytes still in flight to the browser (not yet acknowledged),
/// which never drain however little is queued. Through a relay with a 160 ms round trip
/// that's tens of KB, more than the whole video queue: counted as queued, it kept every
/// frame (and the keyframe to recover with) out. The least seen in the last second.
#[derive(Default)]
struct InFlight {
    window_min: Option<usize>,
    floor: usize,
}

impl InFlight {
    fn sample(&mut self, outstanding: usize) {
        self.window_min = Some(self.window_min.map_or(outstanding, |m| m.min(outstanding)));
    }

    /// Bytes actually waiting behind what's in flight. Past the hard cap everything counts,
    /// so a floor that's wrong can never let the buffer grow without limit.
    fn queued(&self, outstanding: usize) -> usize {
        if outstanding > DATAGRAM_BACKLOG {
            return outstanding;
        }
        outstanding.saturating_sub(self.floor)
    }

    /// Once a second. `bps`: the bitrate now. The floor is held to what a long round trip
    /// (`MAX_RTT_MS`) could have in flight: on a link that never drains, the least seen is
    /// partly queue, and taking all of it as "in flight" would ratchet the queue up.
    fn tick(&mut self, bps: u32) {
        if let Some(min) = self.window_min.take() {
            let most = (bps as u64 / 8 * MAX_RTT_MS / 1000).min(DATAGRAM_BACKLOG as u64 / 2) as usize;
            self.floor = min.min(most);
        }
    }
}

/// The longest round trip the in-flight floor allows for.
const MAX_RTT_MS: u64 = 400;

/// The least a link is capped to: still a picture, if a slow one.
const MIN_LINK_CAP: u32 = 250_000;
/// Share of the measured link for video (audio and control ride along).
const LINK_SHARE: f64 = 0.85;
/// Above this the cap is pointless: lifted.
const MAX_LINK_CAP: u32 = 100_000_000;

impl LinkMeter {
    fn sent(&mut self, bytes: usize) {
        self.sent += bytes as u64;
    }

    /// Once a second or so. Returns the new cap when it changes (`Some(None)`: lifted).
    /// `outstanding`: the whole send buffer; `queued`: the part behind what's in flight.
    fn tick(&mut self, outstanding: usize, queued: usize, dropped: bool, budget: usize, elapsed: Duration) -> Option<Option<u32>> {
        let secs = elapsed.as_secs_f64().max(0.001);
        // What left the queue: what went in, less how much it grew.
        let drained = self.sent as i64 - (outstanding as i64 - self.last_outstanding as i64);
        let delivered_bps = drained.max(0) as f64 * 8.0 / secs;
        self.sent = 0;
        self.last_outstanding = outstanding;
        let before = self.cap;
        if dropped || queued > budget / 2 {
            // Backed up, so what drained is about what the link carries, at best: video is
            // unreliable, so messages the relay throws away leave the buffer too and look
            // delivered. So while frames keep being dropped, back off further each second
            // rather than trust the measurement.
            self.calm = 0;
            let measured = (delivered_bps * LINK_SHARE) as u32;
            let next = match self.cap {
                Some(cap) if dropped => measured.min((cap as f64 * LINK_SHARE) as u32),
                Some(cap) => measured.min(cap),
                None => measured,
            };
            self.cap = Some(next.max(MIN_LINK_CAP));
        } else if let Some(cap) = self.cap {
            self.calm += 1;
            // Calm for a few seconds: try a bit more each second.
            if self.calm >= 3 {
                let next = (cap as f64 * 1.15) as u32;
                self.cap = (next < MAX_LINK_CAP).then_some(next);
            }
        }
        (self.cap != before).then_some(self.cap)
    }
}

/// How many bytes of video may be queued at `bps` (0: unknown, the byte cap).
fn video_backlog(bps: u32) -> usize {
    if bps == 0 {
        return DATAGRAM_BACKLOG;
    }
    (bps as u64 / 8 * VIDEO_QUEUE_MS / 1000).clamp(MIN_VIDEO_BACKLOG as u64, DATAGRAM_BACKLOG as u64) as usize
}
const STUN: &str = "stun:stun.cloudflare.com:3478";

// ---- Key and identity ----

pub struct HostKey {
    pair: EcdsaKeyPair,
}

impl HostKey {
    /// Loads this host's key, creating it on first use (mode 0600).
    pub fn load_or_create() -> Result<Self> {
        let path = dir().join("host-key.pk8");
        let rng = SystemRandom::new();
        let pkcs8 = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => {
                let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
                    .map_err(|_| anyhow::anyhow!("couldn't generate a key"))?;
                write_private(&path, doc.as_ref())?;
                doc.as_ref().to_vec()
            }
        };
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &rng)
            .map_err(|_| anyhow::anyhow!("{} is not a valid key", path.display()))?;
        Ok(Self { pair })
    }

    /// The public key, uncompressed P-256 point, base64url: what links carry.
    pub fn public(&self) -> String {
        b64url(self.pair.public_key().as_ref())
    }

    /// Where the introducer files this host: derived from the key, so only the key's
    /// holder can claim it.
    pub fn id(&self) -> String {
        id_for(self.pair.public_key().as_ref())
    }

    /// ECDSA P-256 / SHA-256 signature in IEEE P1363 form (what WebCrypto verifies).
    pub fn sign(&self, msg: &[u8]) -> Result<String> {
        let sig = self
            .pair
            .sign(&SystemRandom::new(), msg)
            .map_err(|_| anyhow::anyhow!("signing failed"))?;
        Ok(b64url(sig.as_ref()))
    }
}

pub fn id_for(public_key: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, public_key);
    b64url(&digest.as_ref()[..16])
}

/// What the host signs for an answer: both SDPs, so neither side can be swapped.
pub fn signed_payload(offer_sdp: &str, answer_sdp: &str) -> Vec<u8> {
    format!("glass-rtc-v1\n{offer_sdp}\n--\n{answer_sdp}").into_bytes()
}

fn dir() -> PathBuf {
    crate::config_dir().join("remote")
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)?.write_all(bytes)?;
    Ok(())
}

pub fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Decodes base64url (padding optional; standard `+/` accepted too).
pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        } as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

// ---- On/off ----

/// Remote access is opt-in: `glass link --anywhere` turns it on, `glass remote off` off.
/// A policy can decide instead.
pub fn enabled() -> bool {
    crate::policy::load().flag("remote").unwrap_or_else(|| dir().join("enabled").exists())
}

pub fn set_enabled(on: bool) -> Result<()> {
    crate::policy::load().check("remote", if on { "on" } else { "off" })?;
    let flag = dir().join("enabled");
    if on {
        std::fs::create_dir_all(dir())?;
        std::fs::write(flag, b"1")?;
    } else {
        let _ = std::fs::remove_file(flag);
    }
    Ok(())
}

/// The introducer: a self-hosted one (`glass config relay`), else https://glassdesk.dev.
/// GLASS_BASE overrides both, as for updates (tests use it).
pub fn base() -> String {
    if std::env::var_os("GLASS_BASE").is_none()
        && let Some(relay) = crate::settings::load().relay
    {
        return relay;
    }
    crate::update::base_url()
}

/// This computer's address from anywhere, with no credential in it: whoever opens it still
/// has to sign in. (`link_url` is the one-time link that lets a device in.)
pub fn open_url(key: &HostKey) -> String {
    format!("{}/app/#r={}", base().trim_end_matches('/'), key.public())
}

/// A link that opens this host from anywhere.
pub fn link_url(key: &HostKey, code: &str) -> String {
    format!(
        "{}/app/#r={}&l={code}",
        base().trim_end_matches('/'),
        key.public()
    )
}

// ---- The loopback bridge's proof ----

/// A per-process secret the bridge presents to the WebTransport server, so it (and only
/// it) can say which remote address a session really comes from: otherwise every remote
/// viewer would share 127.0.0.1's sign-in lockout.
pub fn bridge_secret() -> &'static str {
    static SECRET: OnceLock<String> = OnceLock::new();
    SECRET.get_or_init(|| {
        let mut b = [0u8; 16];
        let _ = ring::rand::SecureRandom::fill(&SystemRandom::new(), &mut b);
        b64url(&b)
    })
}

/// A session that came through the local bridge, as the bridge vouches for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Relay {
    /// The viewer's real address.
    pub peer: IpAddr,
    /// Which bridge (and so which WebRTC peer) this is.
    pub bridge: u64,
    /// Video goes to this viewer as RTP tracks, not as datagrams on the session.
    pub rtp_video: bool,
}

/// Reads `/rtc?peer=<ip>&b=<bridge>&video=rtp&k=<secret>` from a session request path.
pub fn relayed_peer(path: &str) -> Option<Relay> {
    let query = path.strip_prefix("/rtc?")?;
    let (mut peer, mut bridge, mut rtp_video, mut ok) = (None, 0, false, false);
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("peer", v)) => peer = v.parse().ok(),
            Some(("b", v)) => bridge = v.parse().unwrap_or(0),
            Some(("video", v)) => rtp_video = v == "rtp",
            Some(("k", v)) => ok = crate::control::token_matches(v, bridge_secret()),
            _ => {}
        }
    }
    peer.filter(|_| ok).map(|peer| Relay {
        peer,
        bridge,
        rtp_video,
    })
}

/// What the server tells a bridge about its session: signed in yet, and watching (a hidden
/// tab pauses video).
struct BridgeSignals {
    authed: tokio::sync::watch::Sender<bool>,
    watching: tokio::sync::watch::Sender<bool>,
}

static BRIDGES: std::sync::Mutex<Option<std::collections::HashMap<u64, BridgeSignals>>> =
    std::sync::Mutex::new(None);

fn with_bridge(id: u64, f: impl FnOnce(&BridgeSignals)) {
    if let Some(b) = BRIDGES.lock().unwrap().as_ref().and_then(|m| m.get(&id)) {
        f(b);
    }
}

pub fn bridge_authed(id: u64) {
    with_bridge(id, |b| {
        let _ = b.authed.send(true);
    });
}

pub fn bridge_watching(id: u64, on: bool) {
    with_bridge(id, |b| {
        let _ = b.watching.send(on);
    });
}

/// Registers a bridge; it's removed again when the guard drops.
fn register_bridge() -> (u64, crate::rtp::Viewer, BridgeGuard) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (authed_tx, authed) = tokio::sync::watch::channel(false);
    let (watching_tx, watching) = tokio::sync::watch::channel(true);
    BRIDGES
        .lock()
        .unwrap()
        .get_or_insert_with(Default::default)
        .insert(
            id,
            BridgeSignals {
                authed: authed_tx,
                watching: watching_tx,
            },
        );
    (
        id,
        crate::rtp::Viewer {
            authed,
            watching,
            id,
        },
        BridgeGuard(id),
    )
}

struct BridgeGuard(u64);

impl Drop for BridgeGuard {
    fn drop(&mut self) {
        if let Some(m) = BRIDGES.lock().unwrap().as_mut() {
            m.remove(&self.0);
        }
        crate::server::set_remote_cap(self.0, None);
    }
}

// ---- Signaling ----

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
enum FromIntroducer {
    Challenge {
        nonce: String,
    },
    Ready,
    Offer {
        session: String,
        sdp: String,
        #[serde(default)]
        ip: Option<String>,
        #[serde(default, rename = "iceServers")]
        ice_servers: Vec<IceServer>,
    },
    /// An action your account asked for, signed by a pinned passkey (see `actions`).
    Command {
        id: String,
        cmd: String,
        assertion: crate::actions::Assertion,
    },
    /// A passkey asking to be pinned for actions.
    Enroll {
        id: String,
        cred: String,
        key: String,
        mac: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Serialize)]
#[serde(tag = "t", rename_all = "camelCase")]
enum ToIntroducer {
    /// The answer to a command or an enrollment.
    Result {
        id: String,
        ok: bool,
        message: String,
    },
    Auth {
        key: String,
        sig: String,
    },
    Answer {
        session: String,
        sdp: String,
        sig: String,
    },
    Error {
        session: String,
        msg: String,
    },
}

#[derive(Clone, Deserialize)]
struct IceServer {
    urls: Vec<String>,
    #[serde(default)]
    username: String,
    #[serde(default)]
    credential: String,
}

/// Where remote access stands, for `glass status`: 0 off, 1 connecting to the introducer,
/// 2 ready (the introducer has accepted this computer, so a link or account can reach it).
static STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn set_state(state: u8) {
    STATE.store(state, std::sync::atomic::Ordering::Relaxed);
}

/// "off", "connecting" or "ready".
pub fn state() -> &'static str {
    match STATE.load(std::sync::atomic::Ordering::Relaxed) {
        0 => "off",
        1 => "connecting",
        _ => "ready",
    }
}

/// Keeps a connection to the introducer while remote access is on.
/// `dial` is the address of this host's WebTransport socket (loopback when it is bound there).
pub fn watch(wt_port: u16, dial: IpAddr) {
    tokio::spawn(async move {
        let mut delay = Duration::from_secs(1);
        loop {
            if !enabled() {
                set_state(0);
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
            set_state(1);
            match signal(wt_port, dial).await {
                Ok(()) => delay = Duration::from_secs(1),
                Err(e) => {
                    set_state(1);
                    warn!("remote access: {e:#}; retrying in {}s", delay.as_secs());
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(60));
                }
            }
        }
    });
}

async fn signal(wt_port: u16, dial: IpAddr) -> Result<()> {
    let key = Arc::new(HostKey::load_or_create()?);
    let url = format!(
        "{}/signal/host/{}",
        base().replacen("http", "ws", 1),
        key.id()
    );
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .context("connecting to the introducer")?;
    let (mut tx, mut rx) = ws.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<ToIntroducer>();
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    let mut last_pong = std::time::Instant::now();
    let mut check = tokio::time::interval(Duration::from_secs(3));
    loop {
        tokio::select! {
            msg = rx.next() => {
                let Some(msg) = msg else { anyhow::bail!("introducer closed the connection") };
                let Message::Text(text) = msg? else { continue };
                if text.as_str() == "pong" {
                    last_pong = std::time::Instant::now();
                    continue;
                }
                match serde_json::from_str::<FromIntroducer>(&text) {
                    Ok(FromIntroducer::Challenge { nonce }) => {
                        let sig = key.sign(format!("glass-signal-v1\n{nonce}").as_bytes())?;
                        let auth = ToIntroducer::Auth { key: key.public(), sig };
                        tx.send(Message::text(serde_json::to_string(&auth)?)).await?;
                    }
                    Ok(FromIntroducer::Ready) => {
                        set_state(2);
                        info!("remote access ready (host {})", key.id());
                    }
                    Ok(FromIntroducer::Offer { session, sdp, ip, ice_servers }) => {
                        let (key, out_tx) = (key.clone(), out_tx.clone());
                        let ip = ip.and_then(|ip| ip.parse::<IpAddr>().ok());
                        info!(
                            "remote offer from {} ({})",
                            ip.map(|i| i.to_string()).unwrap_or_else(|| "?".into()),
                            candidate_summary(&sdp)
                        );
                        tokio::spawn(async move {
                            let reply = match answer(&key, &sdp, ip, ice_servers, wt_port, dial).await {
                                Ok((answer_sdp, sig)) => ToIntroducer::Answer { session, sdp: answer_sdp, sig },
                                Err(e) => {
                                    warn!("remote peer: {e:#}");
                                    ToIntroducer::Error { session, msg: "the host couldn't answer".into() }
                                }
                            };
                            let _ = out_tx.send(reply);
                        });
                    }
                    Ok(FromIntroducer::Command { id, cmd, assertion }) => {
                        let out_tx = out_tx.clone();
                        tokio::spawn(async move {
                            let (ok, message, deferred) = tokio::task::spawn_blocking(move || crate::actions::command(&cmd, &assertion))
                                .await
                                .unwrap_or((false, "the action failed".into(), None));
                            let _ = out_tx.send(ToIntroducer::Result { id, ok, message });
                            if let Some(d) = deferred {
                                // After the answer has gone: it would cut the connection first.
                                tokio::time::sleep(Duration::from_millis(1500)).await;
                                let _ = tokio::task::spawn_blocking(move || crate::actions::finish(d)).await;
                            }
                        });
                    }
                    Ok(FromIntroducer::Enroll { id, cred, key, mac }) => {
                        let out_tx = out_tx.clone();
                        tokio::spawn(async move {
                            let r = tokio::task::spawn_blocking(move || crate::actions::enroll(&cred, &key, &mac)).await;
                            let (ok, message) = match r {
                                Ok(Ok(m)) => (true, m),
                                Ok(Err(e)) => (false, format!("{e:#}")),
                                Err(_) => (false, "it failed".into()),
                            };
                            let _ = out_tx.send(ToIntroducer::Result { id, ok, message });
                        });
                    }
                    Ok(FromIntroducer::Other) => {}
                    Err(e) => debug!("introducer: {e}"),
                }
            }
            Some(out) = out_rx.recv() => {
                tx.send(Message::text(serde_json::to_string(&out)?)).await?;
            }
            _ = ping.tick() => {
                // A heartbeat glassdesk.dev answers: a connection that looks open but goes
                // nowhere (after a network change) would otherwise swallow every offer.
                if last_pong.elapsed() > Duration::from_secs(60) {
                    anyhow::bail!("glassdesk.dev stopped answering");
                }
                tx.send(Message::text("ping")).await?;
            }
            _ = check.tick() => {
                if !enabled() {
                    info!("remote access turned off");
                    let _ = tx.send(Message::Close(None)).await;
                    return Ok(());
                }
            }
        }
    }
}

/// "2 host (mdns), 1 srflx v6, 1 relay v4": what a browser offered, for diagnosing failures.
fn candidate_summary(sdp: &str) -> String {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for line in sdp.lines().filter(|l| l.starts_with("a=candidate:")) {
        let f: Vec<&str> = line.split_whitespace().collect();
        // a=candidate:<foundation> <component> <proto> <priority> <address> <port> typ <type>
        let (Some(addr), Some(kind)) = (f.get(4), f.get(7)) else {
            continue;
        };
        let family = if addr.ends_with(".local") {
            "mdns"
        } else if addr.contains(':') {
            "v6"
        } else {
            "v4"
        };
        *counts.entry(format!("{kind} {family}")).or_default() += 1;
    }
    if counts.is_empty() {
        return "no candidates".into();
    }
    counts
        .iter()
        .map(|(k, n)| format!("{n} {k}"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---- Peer and bridge ----

struct Handler {
    gathered: tokio::sync::mpsc::Sender<()>,
    /// Which kinds of candidate we have so far: (server-reflexive, relay).
    found: tokio::sync::watch::Sender<(bool, bool)>,
    channels: tokio::sync::mpsc::UnboundedSender<Arc<dyn DataChannel>>,
    state: tokio::sync::watch::Sender<RTCPeerConnectionState>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        use webrtc::peer_connection::RTCIceCandidateType as Kind;
        match event.candidate.typ {
            Kind::Srflx => self.found.send_modify(|f| f.0 = true),
            Kind::Relay => self.found.send_modify(|f| f.1 = true),
            _ => {}
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.state.send(state);
    }
    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        let _ = self.channels.send(channel);
    }
}

/// Where peer connections listen: any port, except that a test host can be pinned to one
/// (`GLASS_TEST_RTC_PORT`, IPv4) so bench/ can shape exactly that traffic.
fn udp_addrs() -> Vec<String> {
    match std::env::var("GLASS_TEST_RTC_PORT").ok().filter(|_| crate::testmode::enabled()) {
        Some(port) => vec![format!("0.0.0.0:{port}")],
        None => vec!["0.0.0.0:0".to_string(), "[::]:0".to_string()],
    }
}

/// Answers a browser's offer; returns the answer SDP and its signature. The bridge runs
/// until the peer goes away.
async fn answer(
    key: &HostKey,
    offer_sdp: &str,
    remote_ip: Option<IpAddr>,
    ice_servers: Vec<IceServer>,
    wt_port: u16,
    dial: IpAddr,
) -> Result<(String, String)> {
    let has_turn = !ice_servers.is_empty();
    let mut servers = vec![RTCIceServer {
        urls: vec![STUN.into()],
        ..Default::default()
    }];
    servers.extend(ice_servers.into_iter().map(|s| RTCIceServer {
        urls: s.urls,
        username: s.username,
        credential: s.credential,
    }));
    // Video as RTP tracks when the browser offered video m-lines (and RTP isn't switched
    // off); otherwise the data-channel path below carries it, as before.
    let rtp_tracks = if std::env::var("GLASS_REMOTE_RTP").as_deref() == Ok("0") {
        0
    } else {
        crate::rtp::offered_video(offer_sdp)
    };
    let (mut media, registry, target) = if rtp_tracks > 0 {
        let mut media = crate::rtp::media_engine()?;
        let (registry, target) = crate::rtp::registry(&mut media)?;
        (media, registry, Some(target))
    } else {
        let mut media = MediaEngine::default();
        let registry = register_default_interceptors(Registry::new(), &mut media)?;
        (media, registry, None)
    };
    let _ = &mut media;
    let (gathered_tx, mut gathered) = tokio::sync::mpsc::channel(1);
    let (channels_tx, channels) = tokio::sync::mpsc::unbounded_channel();
    let (state_tx, state) = tokio::sync::watch::channel(RTCPeerConnectionState::New);
    let (found_tx, mut found) = tokio::sync::watch::channel((false, false));
    let handler = Arc::new(Handler {
        gathered: gathered_tx,
        found: found_tx,
        channels: channels_tx,
        state: state_tx,
    });
    let runtime = webrtc::runtime::default_runtime().context("no async runtime")?;
    let pc = PeerConnectionBuilder::new()
        .with_configuration(
            RTCConfigurationBuilder::new()
                .with_ice_servers(servers)
                .build(),
        )
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_runtime(runtime)
        .with_udp_addrs(udp_addrs())
        .build()
        .await?;
    let pc: Arc<dyn PeerConnection> = Arc::new(pc);

    pc.set_remote_description(RTCSessionDescription::offer(offer_sdp.to_string())?)
        .await?;
    // Tracks bind to the offered video m-lines in order (stream i → m-line i).
    let video = match target {
        Some(target) => Some((crate::rtp::add_tracks(&pc, rtp_tracks).await?, target)),
        None => None,
    };
    let answer = pc.create_answer(None).await?;
    pc.set_local_description(answer).await?;
    // Non-trickle: one message each way. Gathering is quick (host + STUN, maybe TURN).
    // Gathering may never report "complete" (unreachable interfaces, slow mDNS). Answer as
    // soon as we know our public address (and a relay, when one is configured): that's
    // what lets a browser elsewhere reach us. Host candidates arrive at once.
    let want_relay = has_turn;
    let enough = async {
        let _ = found
            .wait_for(|&(srflx, relay)| srflx && (relay || !want_relay))
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let cap = Duration::from_millis(if want_relay { 2500 } else { 1500 });
    tokio::select! {
        _ = gathered.recv() => {}
        _ = enough => {}
        _ = tokio::time::sleep(cap) => {}
    }
    let answer_sdp = pc
        .local_description()
        .await
        .context("no local description")?
        .sdp;
    let sig = key.sign(&signed_payload(offer_sdp, &answer_sdp))?;

    tokio::spawn(async move {
        if let Err(e) = bridge(pc.clone(), channels, state, remote_ip, wt_port, dial, video).await {
            info!("remote peer ended: {e:#}");
        }
        let _ = pc.close().await;
    });
    Ok((answer_sdp, sig))
}

async fn bridge(
    pc: Arc<dyn PeerConnection>,
    mut channels: tokio::sync::mpsc::UnboundedReceiver<Arc<dyn DataChannel>>,
    mut state: tokio::sync::watch::Receiver<RTCPeerConnectionState>,
    remote_ip: Option<IpAddr>,
    wt_port: u16,
    dial: IpAddr,
    video: Option<(Vec<crate::rtp::VideoOut>, Arc<std::sync::atomic::AtomicU64>)>,
) -> Result<()> {
    // Wait for the browser to connect before opening the loopback session.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match *state.borrow_and_update() {
                RTCPeerConnectionState::Connected => return Ok(()),
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                    anyhow::bail!("connection failed")
                }
                _ => {}
            }
            state.changed().await?;
        }
    })
    .await
    .context("the browser never connected")??;
    let peer = remote_ip
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "0.0.0.0".into());
    info!("remote peer connected ({peer})");

    let config = wtransport::ClientConfig::builder()
        .with_bind_default()
        .with_no_cert_validation()
        .build();
    let endpoint = wtransport::Endpoint::client(config)?;
    let (bridge_id, viewer, _registration) = register_bridge();
    let rtp_video = video.is_some();
    let watching = viewer.watching.clone();
    let url = format!(
        "https://{}:{wt_port}/rtc?peer={peer}&b={bridge_id}{}&k={}",
        crate::listen::host(dial),
        if rtp_video { "&video=rtp" } else { "" },
        bridge_secret()
    );
    let conn = endpoint
        .connect(url)
        .await
        .context("local WebTransport session")?;
    match video {
        // Video goes out as RTP; congestion control sets this viewer's bitrate cap.
        Some((outs, target)) => {
            tokio::spawn(async move {
                if let Err(e) = crate::rtp::pump(outs, target, viewer).await {
                    info!("remote video ended: {e:#}");
                }
            });
        }
        None => crate::server::remote_viewer_joined(),
    }

    let mut datagram_channel: Option<Arc<dyn DataChannel>> = None;
    // Frame acknowledgements from the browser, on their way to the host: the bridge reads
    // them too, so its own flow control knows what's on screen. Once they come, the host
    // sets this viewer's bitrate from them (end to end, through this bridge), and the link
    // meter steps aside.
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel::<(u8, u32)>();
    let mut acking = false;
    let mut bridge_drops = 0u64;
    // Video over the data channel: measure the link and cap the bitrate to it (RTP video
    // has its own congestion control).
    let mut meter = LinkMeter::default();
    let mut meter_drops = 0u64;
    let mut meter_tick = tokio::time::interval(Duration::from_secs(1));
    let mut inflight = InFlight::default();
    let mut last_tick = std::time::Instant::now();
    let mut gate = FrameGate::default();
    let mut last_keyframe_ask = std::time::Instant::now();
    let mut last_congestion_report = std::time::Instant::now();
    let mut last_drop_report = std::time::Instant::now();
    // Flow control (crate::flow): while frames sent haven't left the data channel's buffer
    // (it counts them until the browser acknowledges them), capture is held, so frames
    // aren't made only to be dropped here. The gate stays as a safety net.
    let mut link = crate::flow::Link::default();
    let mut flow = crate::flow::Viewer::new();
    let mut flow_poll = tokio::time::interval(crate::flow::POLL);
    flow_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            channel = channels.recv() => {
                let Some(channel) = channel else { return Ok(()) };
                let label = channel.label().await.unwrap_or_default();
                match label.as_str() {
                    "d" => {
                        datagram_channel = Some(channel.clone());
                        let conn = conn.clone();
                        let ack_tx = ack_tx.clone();
                        tokio::spawn(async move {
                            while let Some(event) = channel.poll().await {
                                match event {
                                    DataChannelEvent::OnMessage(m) => {
                                        if let Some(ack) = glass_proto::read_frame_ack(&m.data[..]) {
                                            let _ = ack_tx.send(ack);
                                        }
                                        let _ = conn.send_datagram(&m.data[..]);
                                    }
                                    DataChannelEvent::OnClose => break,
                                    _ => {}
                                }
                            }
                        });
                    }
                    "bi" => {
                        let (send, recv) = conn.open_bi().await?.await?;
                        tokio::spawn(pipe_channel_to_stream(channel.clone(), send));
                        tokio::spawn(pipe_stream_to_channel(recv, channel));
                    }
                    "uni" => {
                        let send = conn.open_uni().await?.await?;
                        tokio::spawn(pipe_channel_to_stream(channel, send));
                    }
                    other => debug!("ignoring data channel {other:?}"),
                }
            }
            datagram = conn.receive_datagram() => {
                let datagram = datagram?;
                if let Some(dc) = &datagram_channel {
                    let outstanding = dc.outstanding_bytes().await.unwrap_or(0);
                    let payload = datagram.payload();
                    inflight.sample(outstanding);
                    let queued = inflight.queued(outstanding);
                    let now = std::time::Instant::now();
                    link.drained(outstanding, now);
                    match gate.admit(&payload, queued, video_backlog(crate::server::applied_bitrate())) {
                        Admit::Send => {
                            if dc.try_send(BytesMut::from(&payload[..])).await.is_ok() {
                                meter.sent(payload.len());
                                match glass_proto::VideoHeader::read(&payload) {
                                    Some(h) => link.sent(h.stream, h.frame_id, payload.len(), now),
                                    None => link.other(payload.len()),
                                }
                            }
                        }
                        Admit::Drop => {
                            bridge_drops += 1;
                            meter_drops += 1;
                        }
                    }
                    flow.update(&mut link, now);
                    // Tell the bitrate controller: the browser can't see these drops. (With
                    // acknowledgements, the host sees them as frames that never arrive.)
                    if acking {
                        gate.dropped_frames = 0;
                    }
                    if gate.dropped_frames > 0 && last_congestion_report.elapsed() >= Duration::from_secs(1) {
                        crate::server::bridge_congestion(std::mem::take(&mut gate.dropped_frames));
                        last_congestion_report = std::time::Instant::now();
                    }
                    // Waiting for a keyframe the browser can't ask for (it sees no gap when
                    // nothing arrives at all): ask on its behalf.
                    // Ask once the send buffer has drained, so the keyframe and the frames
                    // after it fit; asking sooner just loops on keyframes alone.
                    for stream in gate.waiting_for_keyframe() {
                        if queued < video_backlog(crate::server::applied_bitrate()) / 4
                            && last_keyframe_ask.elapsed() >= Duration::from_millis(500)
                        {
                            last_keyframe_ask = std::time::Instant::now();
                            let _ = conn.send_datagram([glass_proto::KIND_KEYFRAME_REQUEST, stream]);
                        }
                    }
                }
                // Tells apart loss here (the WebRTC send buffer is full) from loss on the way.
                if bridge_drops > 0 && last_drop_report.elapsed() >= Duration::from_secs(5) {
                    info!(
                        "remote peer ({peer}): bridge dropped {bridge_drops} packets (link backed up; {} KB in flight, link cap {})",
                        inflight.floor / 1024,
                        meter.cap.map_or("none".to_string(), |c| format!("{:.1} Mbps", c as f64 / 1e6)),
                    );
                    bridge_drops = 0;
                    last_drop_report = std::time::Instant::now();
                }
            }
            stream = conn.accept_uni() => {
                let recv = stream?;
                let init = RTCDataChannelInit { ordered: true, ..Default::default() };
                let channel = pc.create_data_channel("uni", Some(init)).await?;
                tokio::spawn(async move {
                    // Wait until it's open before sending.
                    while let Some(event) = channel.poll().await {
                        if matches!(event, DataChannelEvent::OnOpen) { break; }
                    }
                    pipe_stream_to_channel(recv, channel).await;
                });
            }
            Some((stream, id)) = ack_rx.recv() => {
                if !acking {
                    acking = true;
                    crate::server::set_remote_cap(bridge_id, None);
                    debug!("remote peer ({peer}): frames acknowledged; the host paces video by them");
                }
                let now = std::time::Instant::now();
                link.acked(stream, id, now);
                flow.update(&mut link, now);
            }
            _ = flow_poll.tick(), if link.busy() => {
                if let Some(dc) = &datagram_channel {
                    let now = std::time::Instant::now();
                    link.drained(dc.outstanding_bytes().await.unwrap_or(0), now);
                    // Paused: what's in flight says nothing about the link.
                    if !*watching.borrow() {
                        link.forget();
                    }
                    flow.update(&mut link, now);
                }
            }
            _ = meter_tick.tick(), if !rtp_video && !acking => {
                if let Some(dc) = &datagram_channel {
                    let outstanding = dc.outstanding_bytes().await.unwrap_or(0);
                    let budget = video_backlog(crate::server::applied_bitrate());
                    let queued = inflight.queued(outstanding);
                    inflight.tick(crate::server::applied_bitrate());
                    if let Some(cap) = meter.tick(outstanding, queued, meter_drops > 0, budget, last_tick.elapsed()) {
                        match cap {
                            Some(bps) => debug!("remote peer ({peer}): link carries ~{:.1} Mbps of video", bps as f64 / 1e6),
                            None => debug!("remote peer ({peer}): link cap lifted"),
                        }
                        crate::server::set_remote_cap(bridge_id, cap);
                    }
                }
                meter_drops = 0;
                last_tick = std::time::Instant::now();
            }
            changed = state.changed() => {
                changed?;
                if matches!(*state.borrow(), RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed | RTCPeerConnectionState::Disconnected) {
                    info!("remote peer disconnected ({peer})");
                    conn.close(0u32.into(), b"peer gone");
                    return Ok(());
                }
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum Admit {
    Send,
    Drop,
}

/// Decides, a whole video frame at a time, what the WebRTC send buffer can take. Dropping
/// single packets when it's full would leave nearly every frame with a hole: bandwidth
/// spent on frames that can't be decoded. And once a frame is gone, the frames after it
/// can't be decoded either until the next keyframe, so those go too.
#[derive(Default)]
struct FrameGate {
    /// Per stream: the frame being passed through and whether it's sent or dropped.
    current: [Option<(u32, bool)>; glass_proto::MAX_STREAMS],
    /// Per stream: a frame was dropped, so wait for a keyframe.
    broken: [bool; glass_proto::MAX_STREAMS],
    /// Frames dropped since the bitrate controller was last told.
    dropped_frames: u32,
}

/// Rough bytes on the wire per video fragment.
const FRAGMENT_BYTES: usize = 1250;

impl FrameGate {
    /// Streams whose picture is broken until a keyframe arrives.
    fn waiting_for_keyframe(&self) -> impl Iterator<Item = u8> + '_ {
        self.broken
            .iter()
            .enumerate()
            .filter(|(_, b)| **b)
            .map(|(i, _)| i as u8)
    }

    /// `budget`: how much video may be queued now (see [`video_backlog`]).
    fn admit(&mut self, datagram: &[u8], outstanding: usize, budget: usize) -> Admit {
        let Some(h) = (datagram.first() == Some(&glass_proto::KIND_VIDEO))
            .then(|| glass_proto::VideoHeader::read(datagram))
            .flatten()
        else {
            // Audio (Opus where the browser decodes it: ~100 kbps, not 1.5 Mbps of PCM) and
            // pings: small; drop only if things are badly backed up.
            return if outstanding < DATAGRAM_BACKLOG * 2 {
                Admit::Send
            } else {
                Admit::Drop
            };
        };
        // A lost fragment sent again: it isn't a new frame, and it's what ends a stall.
        if h.flags & glass_proto::FLAG_RETRANSMIT != 0 {
            return if outstanding < DATAGRAM_BACKLOG * 2 { Admit::Send } else { Admit::Drop };
        }
        let s = (h.stream as usize).min(glass_proto::MAX_STREAMS - 1);
        if let Some((id, send)) = self.current[s]
            && id == h.frame_id
        {
            return if send { Admit::Send } else { Admit::Drop };
        }
        // A new frame: decide for all of it.
        let keyframe = h.flags & glass_proto::FLAG_KEYFRAME != 0;
        let send = if keyframe {
            // Keyframes can be bigger than the whole budget; send once the queue has
            // mostly drained, or the picture would never recover.
            outstanding < budget / 2
        } else {
            // A frame bigger than the budget (a game's scene change) still goes when the
            // queue is nearly empty, or it would be dropped every time.
            !self.broken[s]
                && (outstanding + h.frag_count as usize * FRAGMENT_BYTES <= budget || outstanding < budget / 4)
        };
        self.broken[s] = if keyframe {
            !send
        } else {
            self.broken[s] || !send
        };
        self.current[s] = Some((h.frame_id, send));
        if !send {
            self.dropped_frames += 1;
        }
        if send { Admit::Send } else { Admit::Drop }
    }
}

async fn pipe_channel_to_stream(channel: Arc<dyn DataChannel>, mut send: wtransport::SendStream) {
    while let Some(event) = channel.poll().await {
        match event {
            DataChannelEvent::OnMessage(m) if m.is_string => {
                if &m.data[..] == b"end" {
                    let _ = send.finish().await;
                    return;
                }
            }
            DataChannelEvent::OnMessage(m) => {
                if send.write_all(&m.data).await.is_err() {
                    return;
                }
            }
            DataChannelEvent::OnClose => return,
            _ => {}
        }
    }
}

async fn pipe_stream_to_channel(mut recv: wtransport::RecvStream, channel: Arc<dyn DataChannel>) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) => {
                // Backpressure: wait while the browser is behind.
                let _ = channel.writable().await;
                if channel.send(BytesMut::from(&buf[..n])).await.is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = channel.send_text("end").await;
                return;
            }
            Err(_) => {
                let _ = channel.close().await;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_candidates() {
        let sdp = "v=0\r\na=candidate:1 1 udp 2113937151 abc.local 5000 typ host\r\n\
                   a=candidate:2 1 udp 1677729535 2600:1700::1 6000 typ srflx raddr 0.0.0.0 rport 0\r\n\
                   a=candidate:3 1 udp 33562623 104.30.1.2 3478 typ relay raddr 1.2.3.4 rport 1\r\n";
        assert_eq!(
            candidate_summary(sdp),
            "1 host mdns, 1 relay v4, 1 srflx v6"
        );
        assert_eq!(candidate_summary("v=0"), "no candidates");
    }

    fn video(frame_id: u32, frag_idx: u16, frag_count: u16, keyframe: bool) -> Vec<u8> {
        let h = glass_proto::VideoHeader {
            flags: if keyframe {
                glass_proto::FLAG_KEYFRAME
            } else {
                0
            },
            stream: 0,
            frag_idx,
            frag_count,
            frame_id,
            capture_us: 0,
            checksum: 0,
        };
        let mut buf = vec![0u8; 64];
        h.write(&mut buf);
        buf
    }

    #[test]
    fn bytes_in_flight_dont_count_as_queued() {
        let mut f = InFlight::default();
        // Through a relay, ~26 KB is always in flight: the least seen in a second.
        for o in [60_000, 26_000, 41_000] {
            f.sample(o);
        }
        assert_eq!(f.queued(41_000), 41_000, "no floor until a second has been seen");
        f.tick(1_300_000);
        assert_eq!(f.queued(41_000), 15_000);
        assert_eq!(f.queued(20_000), 0);
        // A second with no samples keeps the floor.
        f.tick(1_300_000);
        assert_eq!(f.queued(36_000), 10_000);
        // A link that never drains: the least seen (200 KB) is mostly queue. The floor
        // stops at what 400 ms could have in flight at 1.3 Mbps (65 KB)...
        f.sample(200_000);
        f.tick(1_300_000);
        assert_eq!(f.queued(200_000), 135_000);
        // ...and past the hard cap, everything counts.
        assert_eq!(f.queued(DATAGRAM_BACKLOG + 1), DATAGRAM_BACKLOG + 1);
        // So a keyframe gets out once what's queued, not what's in flight, has drained.
        let mut g = FrameGate::default();
        let budget = video_backlog(1_300_000);
        assert_eq!(g.admit(&video(1, 0, 40, true), f.queued(30_000), budget), Admit::Send);
    }

    #[test]
    fn the_link_meter_caps_to_what_the_link_delivers() {
        let sec = Duration::from_secs(1);
        let budget = video_backlog(2_400_000);
        let mut m = LinkMeter::default();
        // Keeping up: no cap.
        m.sent(100_000);
        assert_eq!(m.tick(0, 0, false, budget, sec), None);
        // Backed up: 300 KB handed over, the queue grew by 150 KB, so ~150 KB/s (1.2 Mbps)
        // got through; video gets 85% of it.
        m.sent(300_000);
        assert_eq!(m.tick(150_000, 150_000, true, budget, sec), Some(Some(1_020_000)));
        // Still dropping though it seems to drain as fast: backs off 15% more anyway
        // (dropped messages look delivered).
        m.sent(150_000);
        assert_eq!(m.tick(150_000, 150_000, true, budget, sec), Some(Some(867_000)));
        // Draining slowly: the cap follows the link down, not below the floor.
        m.sent(0);
        assert_eq!(m.tick(140_000, 140_000, true, budget, sec), Some(Some(MIN_LINK_CAP)));
        // Calm: held for two seconds, then raised 15% a second.
        m.sent(10_000);
        assert_eq!(m.tick(0, 0, false, budget, sec), None);
        assert_eq!(m.tick(0, 0, false, budget, sec), None);
        assert_eq!(m.tick(0, 0, false, budget, sec), Some(Some(287_500)));
        assert_eq!(m.tick(0, 0, false, budget, sec), Some(Some(330_625)));
    }

    #[test]
    fn the_video_queue_is_held_to_a_short_time() {
        // 2.4 Mbps through a relay: ~36 KB, not the 256 KB cap (most of a second there).
        assert_eq!(video_backlog(2_400_000), 36_000);
        assert_eq!(video_backlog(500_000), MIN_VIDEO_BACKLOG);
        assert_eq!(video_backlog(100_000_000), DATAGRAM_BACKLOG);
        assert_eq!(video_backlog(0), DATAGRAM_BACKLOG);

        let budget = video_backlog(2_400_000);
        let mut g = FrameGate::default();
        // 20 KB queued: a 20-fragment (~25 KB) frame would pass 36 KB: dropped.
        assert_eq!(g.admit(&video(1, 0, 20, false), 20_000, budget), Admit::Drop);
        // A huge frame on a nearly empty queue still goes (after a keyframe heals frame 1).
        assert_eq!(g.admit(&video(2, 0, 80, true), 0, budget), Admit::Send);
        assert_eq!(g.admit(&video(3, 0, 60, false), 5_000, budget), Admit::Send);
    }

    #[test]
    fn drops_whole_frames_and_waits_for_a_keyframe() {
        let mut g = FrameGate::default();
        // Room: the whole frame goes, every fragment, even as the buffer fills.
        assert_eq!(g.admit(&video(1, 0, 3, false), 0, DATAGRAM_BACKLOG), Admit::Send);
        assert_eq!(
            g.admit(&video(1, 2, 3, false), DATAGRAM_BACKLOG, DATAGRAM_BACKLOG),
            Admit::Send
        );
        // No room for frame 2: all of it is dropped...
        assert_eq!(
            g.admit(&video(2, 0, 3, false), DATAGRAM_BACKLOG, DATAGRAM_BACKLOG),
            Admit::Drop
        );
        assert_eq!(g.admit(&video(2, 1, 3, false), 0, DATAGRAM_BACKLOG), Admit::Drop);
        // ...and so is frame 3, though there's room: it can't be decoded without 2.
        assert_eq!(g.admit(&video(3, 0, 3, false), 0, DATAGRAM_BACKLOG), Admit::Drop);
        assert_eq!(g.waiting_for_keyframe().collect::<Vec<_>>(), [0]);
        // A keyframe goes once the queue has drained, even if it's huge, and heals it.
        assert_eq!(
            g.admit(&video(4, 0, 400, true), DATAGRAM_BACKLOG, DATAGRAM_BACKLOG),
            Admit::Drop
        );
        assert_eq!(g.admit(&video(5, 0, 400, true), 0, DATAGRAM_BACKLOG), Admit::Send);
        assert_eq!(g.admit(&video(6, 0, 3, false), 0, DATAGRAM_BACKLOG), Admit::Send);
        assert_eq!(g.waiting_for_keyframe().count(), 0);
    }

    #[test]
    fn audio_keeps_flowing_while_video_backs_off() {
        let mut g = FrameGate::default();
        let opus = glass_proto::opus_packet(0, 0, 2, &[0xFC; 120]);
        let pcm = glass_proto::audio_packet(0, 0, 2, &[0; 480]);
        // The queue is past what video may use: its frames go, audio of either kind doesn't.
        assert_eq!(g.admit(&video(1, 0, 3, false), DATAGRAM_BACKLOG, DATAGRAM_BACKLOG), Admit::Drop);
        assert_eq!(g.admit(&opus, DATAGRAM_BACKLOG, DATAGRAM_BACKLOG), Admit::Send);
        assert_eq!(g.admit(&pcm, DATAGRAM_BACKLOG, DATAGRAM_BACKLOG), Admit::Send);
        // Only a badly backed-up link drops it.
        assert_eq!(g.admit(&opus, DATAGRAM_BACKLOG * 2, DATAGRAM_BACKLOG), Admit::Drop);
    }

    #[test]
    fn base64url_matches_the_spec() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
        for bytes in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            &[0xfb, 0xff, 0x00, 0x10],
        ] {
            assert_eq!(b64url_decode(&b64url(bytes)).as_deref(), Some(bytes));
        }
        assert_eq!(b64url_decode("Zm8=").as_deref(), Some(&b"fo"[..]));
        assert_eq!(b64url_decode("Zm 8"), None);
    }

    #[test]
    fn only_the_bridge_can_name_a_peer() {
        let ok = format!("/rtc?peer=203.0.113.9&b=7&video=rtp&k={}", bridge_secret());
        assert_eq!(
            relayed_peer(&ok),
            Some(Relay {
                peer: "203.0.113.9".parse().unwrap(),
                bridge: 7,
                rtp_video: true
            })
        );
        let plain = format!("/rtc?peer=203.0.113.9&k={}", bridge_secret());
        assert_eq!(relayed_peer(&plain).map(|r| r.rtp_video), Some(false));
        assert_eq!(relayed_peer("/rtc?peer=203.0.113.9&k=guess"), None);
        assert_eq!(relayed_peer("/"), None);
    }
}
