//! Remote access through a WebSocket, for viewers on networks where no direct UDP path
//! can form (a strict NAT on their side, and a host that accepts no unsolicited UDP). The
//! page and this socket come through the same HTTPS tunnel (cloudflared, for example) that
//! serves the self-hosted introducer, so the host still opens no ports.
//!
//! Like the WebRTC bridge in `remote`, each socket is joined to the ordinary WebTransport
//! server on a local address, so sign-in and everything else work as on the tailnet. The
//! socket carries binary messages, each starting with a kind byte:
//!
//! - `0` datagram: the rest is one datagram (both ways)
//! - `1` open: `id` (i32 LE) and a kind, 0 bidirectional or 1 unidirectional. The browser
//!   opens streams with positive ids; the host announces its own (unidirectional) with
//!   negative ones
//! - `2` data: `id`, then bytes for that stream
//! - `3` end: `id`; the stream is finished
//! - `4` ack (browser to host): a u64 LE, every byte of binary messages received so far
//!
//! TCP never drops anything, so video is paced the way the data channel's is: what's sent
//! but not yet acknowledged counts as queued, and whole frames are dropped here (newest
//! wins) rather than let a queue build up on the way.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, Response, StatusCode, header};
use axum::response::IntoResponse;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tracing::{debug, info, warn};

use crate::remote::{Admit, CHUNK, DATAGRAM_BACKLOG, FrameGate, InFlight, LinkMeter, video_backlog};

const DGRAM: u8 = 0;
const OPEN: u8 = 1;
const DATA: u8 = 2;
const END: u8 = 3;
const ACK: u8 = 4;
const KIND_BI: u8 = 0;
const KIND_UNI: u8 = 1;

/// Stream data waits while this much is sent but not yet acknowledged, so a file download
/// can't fill the way and starve video.
const STREAM_WINDOW: u64 = 1024 * 1024;

/// `GET /ws-bridge`: upgrades to a WebSocket and bridges it to a local session.
pub async fn handle(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut req: Request,
    wt_port: u16,
    dial: IpAddr,
) -> Response<Body> {
    let headers = req.headers();
    let websocket = headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let Some(accept) = headers
        .get(header::SEC_WEBSOCKET_KEY)
        .filter(|_| websocket)
        .map(|key| derive_accept_key(key.as_bytes()))
    else {
        // Something on the way (a proxy, a tunnel) can strip the upgrade: say what came.
        let names: Vec<&str> = headers.keys().map(|k| k.as_str()).collect();
        info!(
            "websocket bridge: {} {:?} without a WebSocket upgrade (upgrade: {:?}, connection: {:?}; headers: {})",
            req.method(),
            req.version(),
            headers.get(header::UPGRADE),
            headers.get(header::CONNECTION),
            names.join(", ")
        );
        return (StatusCode::BAD_REQUEST, "expected a WebSocket").into_response();
    };
    let viewer = viewer_ip(peer.ip().to_canonical(), headers);
    let upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        let io = match upgrade.await {
            Ok(u) => TokioIo::new(u),
            Err(e) => {
                warn!("websocket peer ({viewer}): upgrade failed: {e}");
                return;
            }
        };
        let ws = WebSocketStream::from_raw_socket(io, Role::Server, None).await;
        if let Err(e) = run(ws, viewer, wt_port, dial).await {
            info!("websocket peer ended ({viewer}): {e:#}");
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "upgrade")
        .header(header::SEC_WEBSOCKET_ACCEPT, accept)
        .body(Body::empty())
        .unwrap()
}

/// The browser's address. A tunnel running on this computer connects from loopback and
/// names the browser in a header; a header from anyone else is ignored.
fn viewer_ip(peer: IpAddr, headers: &HeaderMap) -> IpAddr {
    if !peer.is_loopback() {
        return peer;
    }
    ["cf-connecting-ip", "x-forwarded-for"]
        .iter()
        .find_map(|name| {
            headers
                .get(*name)?
                .to_str()
                .ok()?
                .split(',')
                .next()?
                .trim()
                .parse()
                .ok()
        })
        .unwrap_or(peer)
}

fn frame(kind: u8, id: Option<i32>, body: &[u8]) -> (Message, u64) {
    let mut v = Vec::with_capacity(5 + body.len());
    v.push(kind);
    if let Some(id) = id {
        v.extend_from_slice(&id.to_le_bytes());
    }
    v.extend_from_slice(body);
    let len = v.len() as u64;
    (Message::binary(v), len)
}

/// A stream message's id and what follows it.
fn read_id(rest: &Bytes) -> Option<(i32, Bytes)> {
    let id = i32::from_le_bytes(rest.get(..4)?.try_into().ok()?);
    Some((id, rest.slice(4..)))
}

/// The least round trip is taken over this long, so a route that got slower is noticed.
const RTT_WINDOW: Duration = Duration::from_secs(10);
/// Longer than this isn't a round trip, it's a queue.
const MAX_RTT: Duration = Duration::from_secs(1);

/// The round trip, from the browser's byte acks: an ack covering a message says it arrived,
/// and when that message was sent is known. Over a long route (Baku to California through
/// a tunnel is 200–300 ms) a link carrying `bps` always has about `bps × round trip` on the
/// way; only what's beyond that is a queue. Counted as queued, it made every frame look
/// backed up and pushed the bitrate down to almost nothing.
#[derive(Default)]
struct RoundTrip {
    /// (bytes sent so far, when), oldest first.
    sent: VecDeque<(u64, Instant)>,
    least: Option<(Duration, Instant)>,
}

impl RoundTrip {
    fn sent(&mut self, total: u64, now: Instant) {
        if self.sent.back().is_none_or(|(_, at)| now.duration_since(*at) >= Duration::from_millis(5)) {
            self.sent.push_back((total, now));
            if self.sent.len() > 4096 {
                self.sent.pop_front();
            }
        }
    }

    /// The browser has received `acked` bytes: the round trip it took, when it says.
    fn acked(&mut self, acked: u64, now: Instant) -> Option<Duration> {
        let mut at = None;
        while let Some(&(total, t)) = self.sent.front() {
            if total > acked {
                break;
            }
            at = Some(t);
            self.sent.pop_front();
        }
        let rtt = now.duration_since(at?).min(MAX_RTT);
        if self.least.is_none_or(|(least, since)| rtt < least || now.duration_since(since) >= RTT_WINDOW) {
            self.least = Some((rtt, now));
        }
        Some(rtt)
    }

    fn least(&self) -> Option<Duration> {
        self.least.map(|(d, _)| d)
    }

    /// What a link carrying `bps` has on the way in one round trip.
    fn on_the_way(&self, bps: u32) -> usize {
        self.least().map_or(0, |d| (bps as f64 / 8.0 * d.as_secs_f64()) as usize)
    }
}

/// Bytes waiting behind what's simply on the way.
fn queued(outstanding: usize, on_the_way: usize, floor: usize) -> usize {
    // Past the hard cap everything counts, so a wrong estimate can't let it grow unbounded.
    if outstanding > DATAGRAM_BACKLOG + on_the_way {
        return outstanding;
    }
    outstanding.saturating_sub(on_the_way.max(floor))
}

/// Sent and acknowledged byte counts, shared with the stream readers.
#[derive(Default)]
struct Window {
    sent: AtomicU64,
    acked: AtomicU64,
}

impl Window {
    fn outstanding(&self) -> u64 {
        self.sent
            .load(Ordering::Relaxed)
            .saturating_sub(self.acked.load(Ordering::Relaxed))
    }
}

async fn run<S>(ws: WebSocketStream<S>, peer: IpAddr, wt_port: u16, dial: IpAddr) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut source) = ws.split();
    info!("websocket peer connected ({peer})");
    let config = wtransport::ClientConfig::builder()
        .with_bind_default()
        .with_no_cert_validation()
        .build();
    let endpoint = wtransport::Endpoint::client(config)?;
    let (bridge_id, viewer, _registration) = crate::remote::register_bridge();
    let watching = viewer.watching.clone();
    let url = format!(
        "https://{}:{wt_port}/rtc?peer={peer}&b={bridge_id}&k={}",
        crate::listen::host(dial),
        crate::remote::bridge_secret()
    );
    let conn = endpoint
        .connect(url)
        .await
        .context("local WebTransport session")?;
    crate::server::remote_viewer_joined();

    // One task owns the socket; everything else hands it messages. Pings keep the tunnel
    // from closing it while video is paused.
    let (out, mut out_rx) = mpsc::channel::<Message>(1024);
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(20));
        loop {
            let msg = tokio::select! {
                m = out_rx.recv() => match m { Some(m) => m, None => break },
                _ = ping.tick() => Message::Ping(Bytes::new()),
            };
            if sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let window = Arc::new(Window::default());
    let mut streams: HashMap<i32, mpsc::Sender<Option<Bytes>>> = HashMap::new();
    let mut next_incoming: i32 = -1;
    let mut acking = false;
    let mut bridge_drops = 0u64;
    let mut meter = LinkMeter::default();
    let mut meter_drops = 0u64;
    let mut meter_tick = tokio::time::interval(Duration::from_secs(1));
    let mut inflight = InFlight::default();
    let mut round_trip = RoundTrip::default();
    let mut rtt_report = Instant::now();
    let mut last_tick = Instant::now();
    let mut gate = FrameGate::default();
    let mut last_keyframe_ask = Instant::now();
    let mut last_congestion_report = Instant::now();
    let mut last_drop_report = Instant::now();
    let mut link = crate::flow::Link::default();
    let mut flow = crate::flow::Viewer::new();
    let mut flow_poll = tokio::time::interval(crate::flow::POLL);
    flow_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let ended = loop {
        tokio::select! {
            msg = source.next() => {
                let data = match msg {
                    None => break "the browser closed the connection",
                    Some(Err(e)) => { debug!("websocket peer ({peer}): {e}"); break "the connection broke" }
                    Some(Ok(Message::Binary(b))) => b,
                    Some(Ok(Message::Close(_))) => break "the browser closed the connection",
                    Some(Ok(_)) => continue,
                };
                let Some(&kind) = data.first() else { continue };
                let rest = data.slice(1..);
                match kind {
                    DGRAM => {
                        if let Some((stream, id)) = glass_proto::read_frame_ack(&rest) {
                            if !acking {
                                acking = true;
                                crate::server::set_remote_cap(bridge_id, None);
                                debug!("websocket peer ({peer}): frames acknowledged; the host paces video by them");
                            }
                            let now = Instant::now();
                            link.acked(stream, id, now);
                            flow.update(&mut link, now);
                        }
                        let _ = conn.send_datagram(&rest);
                    }
                    ACK => {
                        if let Some(n) = rest.get(..8).and_then(|b| b.try_into().ok()) {
                            let acked = u64::from_le_bytes(n);
                            window.acked.store(acked, Ordering::Relaxed);
                            let now = Instant::now();
                            if let Some(rtt) = round_trip.acked(acked, now) {
                                link.set_rtt(rtt, now);
                            }
                            link.drained(window.outstanding() as usize, now);
                            flow.update(&mut link, now);
                        }
                    }
                    OPEN => {
                        let Some((id, rest)) = read_id(&rest) else { continue };
                        let (tx, rx) = mpsc::channel(64);
                        streams.insert(id, tx);
                        if rest.first() == Some(&KIND_BI) {
                            let (send, recv) = conn.open_bi().await?.await?;
                            tokio::spawn(write_stream(rx, send));
                            tokio::spawn(read_stream(recv, id, out.clone(), window.clone()));
                        } else {
                            let send = conn.open_uni().await?.await?;
                            tokio::spawn(write_stream(rx, send));
                        }
                    }
                    DATA => {
                        let Some((id, body)) = read_id(&rest) else { continue };
                        if let Some(tx) = streams.get(&id) {
                            let _ = tx.send(Some(body)).await;
                        }
                    }
                    END => {
                        let Some((id, _)) = read_id(&rest) else { continue };
                        if let Some(tx) = streams.remove(&id) {
                            let _ = tx.send(None).await;
                        }
                    }
                    other => debug!("websocket peer ({peer}): unknown message kind {other}"),
                }
            }
            datagram = conn.receive_datagram() => {
                let Ok(datagram) = datagram else { break "the session ended" };
                let payload = datagram.payload();
                let outstanding = window.outstanding() as usize;
                inflight.sample(outstanding);
                let queued = queued(outstanding, round_trip.on_the_way(crate::server::applied_bitrate()), inflight.floor);
                let now = Instant::now();
                link.drained(outstanding, now);
                match gate.admit(&payload, queued, video_backlog(crate::server::applied_bitrate())) {
                    Admit::Send => {
                        let (msg, len) = frame(DGRAM, None, &payload);
                        if out.try_send(msg).is_ok() {
                            let total = window.sent.fetch_add(len, Ordering::Relaxed) + len;
                            round_trip.sent(total, now);
                            meter.sent(payload.len());
                            match glass_proto::VideoHeader::read(&payload) {
                                Some(h) => link.sent(h.stream, h.frame_id, payload.len(), now),
                                None => link.other(payload.len()),
                            }
                        } else {
                            bridge_drops += 1;
                            meter_drops += 1;
                        }
                    }
                    Admit::Drop => {
                        bridge_drops += 1;
                        meter_drops += 1;
                    }
                }
                flow.update(&mut link, now);
                if acking {
                    gate.dropped_frames = 0;
                }
                if gate.dropped_frames > 0 && last_congestion_report.elapsed() >= Duration::from_secs(1) {
                    crate::server::bridge_congestion(std::mem::take(&mut gate.dropped_frames));
                    last_congestion_report = Instant::now();
                }
                // The browser sees no gap when nothing arrives at all: ask for the keyframe
                // on its behalf, once the way has drained enough for it to fit.
                for stream in gate.waiting_for_keyframe() {
                    if queued < video_backlog(crate::server::applied_bitrate()) / 4
                        && last_keyframe_ask.elapsed() >= Duration::from_millis(500)
                    {
                        last_keyframe_ask = Instant::now();
                        let _ = conn.send_datagram([glass_proto::KIND_KEYFRAME_REQUEST, stream]);
                    }
                }
                if bridge_drops > 0 && last_drop_report.elapsed() >= Duration::from_secs(5) {
                    info!(
                        "websocket peer ({peer}): dropped {bridge_drops} packets (link backed up; round trip {}, {} KB on the way, link cap {})",
                        round_trip.least().map_or("?".to_string(), |d| format!("{} ms", d.as_millis())),
                        round_trip.on_the_way(crate::server::applied_bitrate()) / 1024,
                        meter.cap.map_or("none".to_string(), |c| format!("{:.1} Mbps", c as f64 / 1e6)),
                    );
                    bridge_drops = 0;
                    last_drop_report = Instant::now();
                }
            }
            stream = conn.accept_uni() => {
                let Ok(recv) = stream else { break "the session ended" };
                let id = next_incoming;
                next_incoming -= 1;
                let (msg, len) = frame(OPEN, Some(id), &[KIND_UNI]);
                if out.send(msg).await.is_err() {
                    break "the connection broke";
                }
                window.sent.fetch_add(len, Ordering::Relaxed);
                tokio::spawn(read_stream(recv, id, out.clone(), window.clone()));
            }
            _ = flow_poll.tick(), if link.busy() => {
                let now = Instant::now();
                link.drained(window.outstanding() as usize, now);
                // Paused: what's in flight says nothing about the link.
                if !*watching.borrow() {
                    link.forget();
                }
                flow.update(&mut link, now);
            }
            _ = meter_tick.tick() => {
                let outstanding = window.outstanding() as usize;
                let budget = video_backlog(crate::server::applied_bitrate());
                let queued = queued(outstanding, round_trip.on_the_way(crate::server::applied_bitrate()), inflight.floor);
                if rtt_report.elapsed() >= Duration::from_secs(30)
                    && let Some(rtt) = round_trip.least()
                {
                    rtt_report = Instant::now();
                    info!(
                        "websocket peer ({peer}): round trip {} ms, video {:.1} Mbps",
                        rtt.as_millis(),
                        crate::server::applied_bitrate() as f64 / 1e6
                    );
                }
                // Always: what's merely on the way (the tunnel's round trip, the ack
                // interval) must never count as queued, or every frame looks backed up.
                inflight.tick(crate::server::applied_bitrate());
                if acking {
                    meter_drops = 0;
                    last_tick = Instant::now();
                    continue;
                }
                if let Some(cap) = meter.tick(outstanding, queued, meter_drops > 0, budget, last_tick.elapsed()) {
                    match cap {
                        Some(bps) => debug!("websocket peer ({peer}): link carries ~{:.1} Mbps of video", bps as f64 / 1e6),
                        None => debug!("websocket peer ({peer}): link cap lifted"),
                    }
                    crate::server::set_remote_cap(bridge_id, cap);
                }
                meter_drops = 0;
                last_tick = Instant::now();
            }
            _ = conn.closed() => break "the session ended",
        }
    };
    info!("websocket peer disconnected ({peer}): {ended}");
    conn.close(0u32.into(), b"peer gone");
    drop(out);
    let _ = writer.await;
    Ok(())
}

async fn write_stream(mut rx: mpsc::Receiver<Option<Bytes>>, mut send: wtransport::SendStream) {
    while let Some(item) = rx.recv().await {
        match item {
            Some(bytes) => {
                if send.write_all(&bytes).await.is_err() {
                    return;
                }
            }
            None => {
                let _ = send.finish().await;
                return;
            }
        }
    }
}

async fn read_stream(
    mut recv: wtransport::RecvStream,
    id: i32,
    out: mpsc::Sender<Message>,
    window: Arc<Window>,
) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        // Backpressure: wait while the browser is behind.
        while window.outstanding() > STREAM_WINDOW {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let (msg, len, done) = match recv.read(&mut buf).await {
            Ok(Some(n)) => {
                let (msg, len) = frame(DATA, Some(id), &buf[..n]);
                (msg, len, false)
            }
            Ok(None) | Err(_) => {
                let (msg, len) = frame(END, Some(id), &[]);
                (msg, len, true)
            }
        };
        if out.send(msg).await.is_err() {
            return;
        }
        window.sent.fetch_add(len, Ordering::Relaxed);
        if done {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_carry_kind_and_id() {
        let (msg, len) = frame(DATA, Some(-3), b"hi");
        assert_eq!(len, 7);
        let Message::Binary(b) = msg else { panic!("binary") };
        assert_eq!(b[0], DATA);
        assert_eq!(read_id(&b.slice(1..)), Some((-3, Bytes::from_static(b"hi"))));
        let (msg, len) = frame(DGRAM, None, b"abc");
        assert_eq!(len, 4);
        assert!(matches!(msg, Message::Binary(b) if &b[..] == b"\0abc"));
    }

    #[test]
    fn round_trip_comes_from_acks() {
        let t0 = Instant::now();
        let mut r = RoundTrip::default();
        r.sent(1000, t0);
        r.sent(2000, t0 + Duration::from_millis(10));
        assert_eq!(r.acked(500, t0 + Duration::from_millis(50)), None);
        assert_eq!(r.acked(1500, t0 + Duration::from_millis(250)), Some(Duration::from_millis(250)));
        assert_eq!(r.acked(2000, t0 + Duration::from_millis(250)), Some(Duration::from_millis(240)));
        assert_eq!(r.least(), Some(Duration::from_millis(240)));
        // 2 Mbps for 240 ms: 60 KB on the way, none of it queued.
        assert_eq!(r.on_the_way(2_000_000), 60_000);
        assert_eq!(queued(60_000, r.on_the_way(2_000_000), 0), 0);
        assert_eq!(queued(80_000, r.on_the_way(2_000_000), 0), 20_000);
        assert_eq!(queued(DATAGRAM_BACKLOG + 70_000, 60_000, 0), DATAGRAM_BACKLOG + 70_000);
    }

    #[test]
    fn only_a_local_tunnel_names_the_viewer() {
        let mut h = HeaderMap::new();
        h.insert("cf-connecting-ip", "203.0.113.7".parse().unwrap());
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        let remote: IpAddr = "198.51.100.2".parse().unwrap();
        assert_eq!(viewer_ip(local, &h), "203.0.113.7".parse::<IpAddr>().unwrap());
        assert_eq!(viewer_ip(remote, &h), remote);
        assert_eq!(viewer_ip(local, &HeaderMap::new()), local);
    }
}
