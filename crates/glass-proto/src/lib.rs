//! Wire format shared by the host and (mirrored in `web/proto.js`) the browser client.
//!
//! All integers are little-endian.
//!
//! Datagrams (unreliable):
//!   host -> client  VIDEO  fragment of one encoded frame
//!   host -> client  PONG   reply to PING, used for clock sync
//!   client -> host  PING
//!   client -> host  KEYFRAME_REQUEST  [kind, stream] after the client detects loss
//!   host -> client  AUDIO  5 ms of 48 kHz s16 PCM (only to sessions that enabled audio)
//!   host -> client  AUDIO_OPUS  one Opus packet instead, to viewers whose hello offered "opus"
//!   host -> client  CURSOR where the pointer is (only to sessions that draw it themselves)
//!
//! Input travels client -> host on one reliable unidirectional stream as fixed-size
//! [`InputEvent`] records, so a lost key-up can never leave a key stuck down. Text (IME,
//! emoji, accented letters) rides the same stream as [`InputEvent::Text`] chunks, so it
//! stays in order with keys.

pub const KIND_VIDEO: u8 = 1;
pub const KIND_PING: u8 = 2;
pub const KIND_PONG: u8 = 3;
pub const KIND_KEYFRAME_REQUEST: u8 = 4;
pub const KIND_AUDIO: u8 = 5;
/// client -> host: the viewer's microphone, same layout as AUDIO (mono or stereo s16).
pub const KIND_MIC: u8 = 6;
/// host -> client: system audio as one Opus packet (48 kHz stereo), same 16-byte header as
/// AUDIO, for viewers that can decode it (hello `codecs` has "opus").
pub const KIND_AUDIO_OPUS: u8 = 7;
/// host -> client: where the pointer is, for a viewer that draws it itself (see [`CursorPos`]).
pub const KIND_CURSOR: u8 = 9;

/// 5 ms at 48 kHz: one packet fits a datagram (16 + 240 * 2 ch * 2 B = 976 bytes).
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_FRAMES_PER_PACKET: usize = 240;

/// kind u8 | channels u8 | reserved u16 | seq u32 | capture_us u64, then s16 LE interleaved PCM.
pub const AUDIO_HEADER_LEN: usize = 16;

pub fn audio_packet(seq: u32, capture_us: u64, channels: u8, pcm: &[i16]) -> Vec<u8> {
    let mut out = audio_header(KIND_AUDIO, seq, capture_us, channels, pcm.len() * 2);
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// 10 ms at 48 kHz: the frames in each Opus packet.
pub const OPUS_FRAMES_PER_PACKET: usize = 480;

/// An AUDIO_OPUS datagram: the AUDIO header, then one Opus packet.
pub fn opus_packet(seq: u32, capture_us: u64, channels: u8, opus: &[u8]) -> Vec<u8> {
    let mut out = audio_header(KIND_AUDIO_OPUS, seq, capture_us, channels, opus.len());
    out.extend_from_slice(opus);
    out
}

fn audio_header(kind: u8, seq: u32, capture_us: u64, channels: u8, payload: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(AUDIO_HEADER_LEN + payload);
    out.push(kind);
    out.push(channels);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&capture_us.to_le_bytes());
    out
}

/// The header of an AUDIO, AUDIO_OPUS or MIC datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    pub kind: u8,
    pub channels: u8,
    pub seq: u32,
    pub capture_us: u64,
}

impl AudioHeader {
    pub fn read(buf: &[u8]) -> Option<Self> {
        if buf.len() < AUDIO_HEADER_LEN || ![KIND_AUDIO, KIND_MIC, KIND_AUDIO_OPUS].contains(&buf[0]) {
            return None;
        }
        Some(Self {
            kind: buf[0],
            channels: buf[1],
            seq: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            capture_us: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
        })
    }
}

pub const FLAG_KEYFRAME: u8 = 1;
/// The frame is HEVC rather than H.264.
pub const FLAG_HEVC: u8 = 2;
/// A re-encode of a picture that hasn't changed (idle refinement, keyframe on request):
/// a viewer showing a lossless refresh of it may keep that on top.
pub const FLAG_REFRESH: u8 = 4;
/// Upper nibble of `flags`: which display stream the frame belongs to.
pub const STREAM_SHIFT: u8 = 4;
pub const MAX_STREAMS: usize = 4;

/// kind u8 | flags u8 | frag_idx u16 | frag_count u16 | checksum u16 | frame_id u32 | capture_us u64
///
/// `checksum` is [`fletcher16`] of the whole reassembled frame.
pub const VIDEO_HEADER_LEN: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    pub flags: u8,
    pub stream: u8,
    pub frag_idx: u16,
    pub frag_count: u16,
    pub frame_id: u32,
    /// Host clock (microseconds) at which the frame was captured.
    pub capture_us: u64,
    pub checksum: u16,
}

impl VideoHeader {
    pub fn write(&self, out: &mut [u8]) {
        out[0] = KIND_VIDEO;
        out[1] = (self.flags & 0x0f) | (self.stream << STREAM_SHIFT);
        out[2..4].copy_from_slice(&self.frag_idx.to_le_bytes());
        out[4..6].copy_from_slice(&self.frag_count.to_le_bytes());
        out[6..8].copy_from_slice(&self.checksum.to_le_bytes());
        out[8..12].copy_from_slice(&self.frame_id.to_le_bytes());
        out[12..20].copy_from_slice(&self.capture_us.to_le_bytes());
    }

    pub fn read(buf: &[u8]) -> Option<Self> {
        if buf.len() < VIDEO_HEADER_LEN || buf[0] != KIND_VIDEO {
            return None;
        }
        Some(Self {
            flags: buf[1] & 0x0f,
            stream: buf[1] >> STREAM_SHIFT,
            frag_idx: u16::from_le_bytes([buf[2], buf[3]]),
            frag_count: u16::from_le_bytes([buf[4], buf[5]]),
            checksum: u16::from_le_bytes([buf[6], buf[7]]),
            frame_id: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            capture_us: u64::from_le_bytes(buf[12..20].try_into().unwrap()),
        })
    }
}

/// Splits one encoded frame into datagrams no larger than `max_datagram`.
pub fn fragment(
    frame: &[u8],
    stream: u8,
    frame_id: u32,
    keyframe: bool,
    capture_us: u64,
    max_datagram: usize,
) -> Vec<Vec<u8>> {
    let flags = if keyframe { FLAG_KEYFRAME } else { 0 };
    fragment_with_flags(frame, stream, frame_id, flags, capture_us, max_datagram)
}

/// [`fragment`] with explicit flags (FLAG_KEYFRAME, FLAG_HEVC).
pub fn fragment_with_flags(
    frame: &[u8],
    stream: u8,
    frame_id: u32,
    flags: u8,
    capture_us: u64,
    max_datagram: usize,
) -> Vec<Vec<u8>> {
    let payload = max_datagram - VIDEO_HEADER_LEN;
    let checksum = fletcher16(frame);
    let frag_count = frame.len().div_ceil(payload).max(1);
    assert!(frag_count <= u16::MAX as usize, "frame too large to fragment");
    (0..frag_count)
        .map(|i| {
            let chunk = &frame[i * payload..((i + 1) * payload).min(frame.len())];
            let mut dgram = vec![0u8; VIDEO_HEADER_LEN + chunk.len()];
            VideoHeader {
                flags,
                stream,
                frag_idx: i as u16,
                frag_count: frag_count as u16,
                frame_id,
                capture_us,
                checksum,
            }
            .write(&mut dgram);
            dgram[VIDEO_HEADER_LEN..].copy_from_slice(chunk);
            dgram
        })
        .collect()
}

/// Fletcher-16 (mod 255), mirrored in web/proto.js. Detects corrupted reassembly before
/// a bad frame reaches the decoder.
pub fn fletcher16(data: &[u8]) -> u16 {
    let (mut a, mut b) = (0u32, 0u32);
    for chunk in data.chunks(4096) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 255;
        b %= 255;
    }
    ((b << 8) | a) as u16
}

pub const CURSOR_LEN: usize = 16;
/// The pointer is showing (apps hide it, e.g. while typing or playing video).
pub const CURSOR_VISIBLE: u8 = 1;
/// This viewer moved it last: its own mouse is where the pointer is.
pub const CURSOR_YOURS: u8 = 2;
/// The pointer is in the video for now (another viewer needs it there, or a recording):
/// don't draw a second one.
pub const CURSOR_BAKED: u8 = 4;
/// `stream` when the pointer is on no display being streamed.
pub const CURSOR_OFF_STREAM: u8 = 0xff;

/// kind u8 | stream u8 | flags u8 | reserved u8 | shape u32 | x f32 | y f32
///
/// `x`, `y`: 0..1 of display stream `stream`. `shape`: the id of the image (sent on the
/// control stream as `cursor`) the pointer has now.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CursorPos {
    pub stream: u8,
    pub flags: u8,
    pub shape: u32,
    pub x: f32,
    pub y: f32,
}

impl CursorPos {
    pub fn write(&self) -> [u8; CURSOR_LEN] {
        let mut out = [0u8; CURSOR_LEN];
        out[0] = KIND_CURSOR;
        out[1] = self.stream;
        out[2] = self.flags;
        out[4..8].copy_from_slice(&self.shape.to_le_bytes());
        out[8..12].copy_from_slice(&self.x.to_le_bytes());
        out[12..16].copy_from_slice(&self.y.to_le_bytes());
        out
    }

    pub fn read(buf: &[u8]) -> Option<Self> {
        if buf.len() < CURSOR_LEN || buf[0] != KIND_CURSOR {
            return None;
        }
        Some(Self {
            stream: buf[1],
            flags: buf[2],
            shape: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            x: f32::from_le_bytes(buf[8..12].try_into().unwrap()),
            y: f32::from_le_bytes(buf[12..16].try_into().unwrap()),
        })
    }
}

/// PING: kind u8 | pad [u8; 7] | client_ms f64
/// PONG: kind u8 | pad [u8; 7] | client_ms f64 (echoed) | host_us u64
pub fn pong(ping: &[u8], host_us: u64) -> Option<[u8; 24]> {
    if ping.len() < 16 || ping[0] != KIND_PING {
        return None;
    }
    let mut out = [0u8; 24];
    out[0] = KIND_PONG;
    out[8..16].copy_from_slice(&ping[8..16]);
    out[16..24].copy_from_slice(&host_us.to_le_bytes());
    Some(out)
}

pub const INPUT_EVENT_LEN: usize = 12;

/// type u8 | button u8 | keycode u16 | a f32 | b f32 (for moves, `button` is the stream)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// Absolute position, normalized to 0..1 of display stream `stream`.
    MouseMove { x: f32, y: f32, stream: u8 },
    MouseDown { button: u8 },
    MouseUp { button: u8 },
    /// Deltas in the host's desktop points (the units of a display's `points_width`: macOS
    /// points, Windows physical pixels, Linux logical pixels), positive = content moves
    /// up/left (matches DOM `wheel` sign). Fractions count: hosts carry them over.
    Scroll { dx: f32, dy: f32 },
    /// macOS virtual keycode (kVK_*), mapped from `KeyboardEvent.code` by the client.
    KeyDown { keycode: u16 },
    KeyUp { keycode: u16 },
    /// A game controller (browser Gamepad API, standard mapping). `kind`: 0 button
    /// (`value` 0..1), 1 axis (-1..1), 2 connected (`value` 1) or gone (0).
    Gamepad { pad: u8, kind: u8, index: u16, value: f32 },
    /// Relative motion in display points (pointer lock: games, 3D apps).
    MouseMoveBy { dx: f32, dy: f32 },
    /// A pen (Apple Pencil, stylus): its state for the mouse events that follow. `near`:
    /// in proximity (hovering or touching); false when it leaves. `pressure` 0..1, tilt in
    /// degrees (-90..90) toward +x / +y, `twist` the barrel rotation in degrees (0..359).
    Pen { near: bool, pressure: f32, tilt_x: i8, tilt_y: i8, twist: f32 },
    /// Up to [`TEXT_CHUNK`] bytes of UTF-8 text to type, whole characters only; `last` ends
    /// one piece of text (see [`text_events`] and [`TextReader`]).
    Text { len: u8, last: bool, bytes: [u8; TEXT_CHUNK] },
}

/// Text bytes in one [`InputEvent::Text`]: the record after its tag and length byte.
pub const TEXT_CHUNK: usize = INPUT_EVENT_LEN - 2;
/// The most text one piece may carry; hosts drop longer ones.
pub const MAX_TEXT_BYTES: usize = 4096;
const TEXT_LAST: u8 = 0x80;

/// `text` as [`InputEvent::Text`] chunks, split between characters: pieces of at most
/// [`MAX_TEXT_BYTES`], the last chunk of each marked. Empty text is no events.
pub fn text_events(text: &str) -> Vec<InputEvent> {
    let mut events = Vec::new();
    let mut piece = 0;
    let mut chunk = [0u8; TEXT_CHUNK];
    let mut len = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let mut utf8 = [0u8; 4];
        let c = c.encode_utf8(&mut utf8).as_bytes();
        chunk[len..len + c.len()].copy_from_slice(c);
        len += c.len();
        piece += c.len();
        let next = chars.peek().map_or(0, |c| c.len_utf8());
        let ends_piece = next == 0 || piece + next > MAX_TEXT_BYTES;
        if ends_piece || len + next > TEXT_CHUNK {
            events.push(InputEvent::Text { len: len as u8, last: ends_piece, bytes: chunk });
            chunk = [0u8; TEXT_CHUNK];
            len = 0;
            if ends_piece {
                piece = 0;
            }
        }
    }
    events
}

/// Puts [`InputEvent::Text`] chunks back together, one viewer's stream at a time.
#[derive(Debug, Default)]
pub struct TextReader {
    buf: Vec<u8>,
    /// The piece being read grew past [`MAX_TEXT_BYTES`]: dropped up to its last chunk.
    overflow: bool,
}

impl TextReader {
    /// Adds a chunk; returns the text once its last chunk is in (`None` for non-text events,
    /// unfinished text, and text too long or not UTF-8).
    pub fn push(&mut self, ev: &InputEvent) -> Option<String> {
        let InputEvent::Text { len, last, bytes } = ev else { return None };
        let len = (*len as usize).min(TEXT_CHUNK);
        if self.buf.len() + len > MAX_TEXT_BYTES {
            self.overflow = true;
            self.buf.clear();
        }
        if !self.overflow {
            self.buf.extend_from_slice(&bytes[..len]);
        }
        if !*last {
            return None;
        }
        let buf = std::mem::take(&mut self.buf);
        if std::mem::take(&mut self.overflow) {
            return None;
        }
        String::from_utf8(buf).ok().filter(|t| !t.is_empty())
    }
}

pub const GAMEPAD_BUTTON: u8 = 0;
pub const GAMEPAD_AXIS: u8 = 1;
pub const GAMEPAD_CONNECTED: u8 = 2;

impl InputEvent {
    pub fn read(buf: &[u8; INPUT_EVENT_LEN]) -> Option<Self> {
        let button = buf[1];
        let keycode = u16::from_le_bytes([buf[2], buf[3]]);
        let a = f32::from_le_bytes(buf[4..8].try_into().unwrap());
        let b = f32::from_le_bytes(buf[8..12].try_into().unwrap());
        Some(match buf[0] {
            1 => Self::MouseMove { x: a, y: b, stream: button },
            2 => Self::MouseDown { button },
            3 => Self::MouseUp { button },
            4 => Self::Scroll { dx: a, dy: b },
            5 => Self::KeyDown { keycode },
            6 => Self::KeyUp { keycode },
            7..=9 => Self::Gamepad {
                pad: button,
                kind: buf[0] - 7,
                index: keycode,
                value: a,
            },
            10 => Self::MouseMoveBy { dx: a, dy: b },
            11 => Self::Pen {
                near: button != 0,
                pressure: a.clamp(0.0, 1.0),
                tilt_x: keycode as u8 as i8,
                tilt_y: (keycode >> 8) as u8 as i8,
                twist: b,
            },
            12 => {
                let len = button & !TEXT_LAST;
                if len as usize > TEXT_CHUNK {
                    return None;
                }
                Self::Text { len, last: button & TEXT_LAST != 0, bytes: buf[2..].try_into().unwrap() }
            }
            _ => return None,
        })
    }

    pub fn write(&self) -> [u8; INPUT_EVENT_LEN] {
        if let Self::Text { len, last, bytes } = *self {
            let mut out = [0u8; INPUT_EVENT_LEN];
            out[0] = 12;
            out[1] = len.min(TEXT_CHUNK as u8) | if last { TEXT_LAST } else { 0 };
            out[2..].copy_from_slice(&bytes);
            return out;
        }
        let (kind, button, keycode, a, b) = match *self {
            Self::MouseMove { x, y, stream } => (1, stream, 0, x, y),
            Self::MouseDown { button } => (2, button, 0, 0.0, 0.0),
            Self::MouseUp { button } => (3, button, 0, 0.0, 0.0),
            Self::Scroll { dx, dy } => (4, 0, 0, dx, dy),
            Self::KeyDown { keycode } => (5, 0, keycode, 0.0, 0.0),
            Self::KeyUp { keycode } => (6, 0, keycode, 0.0, 0.0),
            Self::Gamepad {
                pad,
                kind,
                index,
                value,
            } => (7 + kind.min(2), pad, index, value, 0.0),
            Self::MouseMoveBy { dx, dy } => (10, 0, 0, dx, dy),
            Self::Pen { near, pressure, tilt_x, tilt_y, twist } => {
                (11, near as u8, u16::from_le_bytes([tilt_x as u8, tilt_y as u8]), pressure, twist)
            }
            Self::Text { .. } => unreachable!("written above"),
        };
        let mut out = [0u8; INPUT_EVENT_LEN];
        out[0] = kind;
        out[1] = button;
        out[2..4].copy_from_slice(&keycode.to_le_bytes());
        out[4..8].copy_from_slice(&a.to_le_bytes());
        out[8..12].copy_from_slice(&b.to_le_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_reassemble() {
        let frame: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let dgrams = fragment(&frame, 2, 7, true, 123, 1200);
        assert_eq!(dgrams.len(), 5000usize.div_ceil(1200 - VIDEO_HEADER_LEN));
        let mut joined = Vec::new();
        for (i, d) in dgrams.iter().enumerate() {
            assert!(d.len() <= 1200);
            let h = VideoHeader::read(d).unwrap();
            assert_eq!((h.frag_idx as usize, h.frame_id, h.capture_us), (i, 7, 123));
            assert_eq!((h.flags, h.stream), (FLAG_KEYFRAME, 2));
            assert_eq!(h.checksum, fletcher16(&frame));
            joined.extend_from_slice(&d[VIDEO_HEADER_LEN..]);
        }
        assert_eq!(joined, frame);
    }

    #[test]
    fn hevc_flag_rides_with_keyframe() {
        let d = fragment_with_flags(b"abc", 1, 2, FLAG_KEYFRAME | FLAG_HEVC, 3, 1200);
        let h = VideoHeader::read(&d[0]).unwrap();
        assert_eq!((h.flags, h.stream), (FLAG_KEYFRAME | FLAG_HEVC, 1));
    }

    #[test]
    fn fletcher16_known_values() {
        assert_eq!(fletcher16(b"abcde"), 0xC8F0);
        assert_eq!(fletcher16(b"abcdef"), 0x2057);
    }

    #[test]
    fn input_roundtrip() {
        for ev in [
            InputEvent::MouseMove { x: 0.25, y: 0.75, stream: 1 },
            InputEvent::MouseDown { button: 2 },
            InputEvent::Scroll { dx: -3.0, dy: 40.0 },
            InputEvent::KeyUp { keycode: 0x31 },
            InputEvent::Gamepad { pad: 1, kind: GAMEPAD_AXIS, index: 3, value: -0.5 },
            InputEvent::Gamepad { pad: 0, kind: GAMEPAD_CONNECTED, index: 0, value: 1.0 },
            InputEvent::MouseMoveBy { dx: 7.0, dy: -3.0 },
            InputEvent::Pen { near: true, pressure: 0.5, tilt_x: -45, tilt_y: 30, twist: 270.0 },
            InputEvent::Pen { near: false, pressure: 0.0, tilt_x: 0, tilt_y: 0, twist: 0.0 },
            text_events("日本")[0],
        ] {
            assert_eq!(InputEvent::read(&ev.write()), Some(ev));
        }
    }

    #[test]
    fn pen_wire_layout() {
        // Same bytes as web/proto.js's penEvent (tests/unit/proto.test.mjs).
        let b = InputEvent::Pen { near: true, pressure: 0.5, tilt_x: -45, tilt_y: 30, twist: 270.0 }.write();
        assert_eq!(&b[..4], &[11, 1, 0xd3, 0x1e]);
        assert_eq!(f32::from_le_bytes(b[4..8].try_into().unwrap()), 0.5);
        assert_eq!(f32::from_le_bytes(b[8..12].try_into().unwrap()), 270.0);
    }

    fn read_all(events: &[InputEvent]) -> Vec<String> {
        let mut reader = TextReader::default();
        events
            .iter()
            .filter_map(|ev| reader.push(&InputEvent::read(&ev.write()).unwrap()))
            .collect()
    }

    #[test]
    fn text_survives_chunking() {
        for text in ["a", "café", "日本語の入力", "👍🏽 ok", "한국어", "0123456789", "x\ny"] {
            assert_eq!(read_all(&text_events(text)), [text], "{text:?}");
        }
        assert!(text_events("").is_empty());
    }

    #[test]
    fn text_chunks_hold_whole_characters() {
        // 4-byte emoji: two per chunk, never split across chunks.
        let events = text_events("😀😀😀");
        assert_eq!(events.len(), 2);
        for ev in &events {
            let InputEvent::Text { len, bytes, .. } = ev else { panic!() };
            assert!(std::str::from_utf8(&bytes[..*len as usize]).is_ok());
        }
    }

    #[test]
    fn text_wire_layout() {
        // Same bytes as web/proto.js's textEvents (tests/unit/proto.test.mjs).
        let b = text_events("é!")[0].write();
        assert_eq!(&b[..5], &[12, 0x80 | 3, 0xc3, 0xa9, b'!']);
        assert!(b[5..].iter().all(|&x| x == 0));
        // A length past the chunk is no event.
        let mut bad = b;
        bad[1] = 11;
        assert_eq!(InputEvent::read(&bad), None);
    }

    #[test]
    fn long_text_splits_into_pieces_and_too_long_is_dropped() {
        let long = "é".repeat(MAX_TEXT_BYTES); // twice the limit in bytes
        let pieces = read_all(&text_events(&long));
        assert!(pieces.len() >= 2 && pieces.iter().all(|p| p.len() <= MAX_TEXT_BYTES));
        assert_eq!(pieces.concat(), long);
        // A sender that never marks the end gets nothing typed, and the next text is fine.
        let mut reader = TextReader::default();
        let chunk = InputEvent::Text { len: TEXT_CHUNK as u8, last: false, bytes: *b"aaaaaaaaaa" };
        for _ in 0..MAX_TEXT_BYTES {
            assert_eq!(reader.push(&chunk), None);
        }
        assert_eq!(reader.push(&InputEvent::Text { len: 1, last: true, bytes: *b"b\0\0\0\0\0\0\0\0\0" }), None);
        assert_eq!(text_events("next").iter().find_map(|e| reader.push(e)), Some("next".into()));
    }

    #[test]
    fn audio_packet_layout() {
        let pcm: Vec<i16> = (0..AUDIO_FRAMES_PER_PACKET as i16 * 2).collect();
        let p = audio_packet(9, 77, 2, &pcm);
        assert_eq!(p.len(), AUDIO_HEADER_LEN + pcm.len() * 2);
        assert!(p.len() <= 1200);
        assert_eq!((p[0], p[1]), (KIND_AUDIO, 2));
        assert_eq!(u32::from_le_bytes(p[4..8].try_into().unwrap()), 9);
        assert_eq!(u64::from_le_bytes(p[8..16].try_into().unwrap()), 77);
        assert_eq!(i16::from_le_bytes([p[18], p[19]]), 1);
    }

    #[test]
    fn opus_packet_layout() {
        let p = opus_packet(0xA1B2_C3D4, 1_234_567, 2, &[0xFC, 1, 2, 3]);
        // Same bytes as web/proto.js's readAudioHeader test (tests/unit/proto.test.mjs).
        assert_eq!(&p[..8], &[KIND_AUDIO_OPUS, 2, 0, 0, 0xD4, 0xC3, 0xB2, 0xA1]);
        assert_eq!(u64::from_le_bytes(p[8..16].try_into().unwrap()), 1_234_567);
        assert_eq!(&p[AUDIO_HEADER_LEN..], &[0xFC, 1, 2, 3]);
        let h = AudioHeader::read(&p).unwrap();
        assert_eq!(h, AudioHeader { kind: KIND_AUDIO_OPUS, channels: 2, seq: 0xA1B2_C3D4, capture_us: 1_234_567 });
        // PCM packets read the same way; anything else (or too short) doesn't.
        assert_eq!(AudioHeader::read(&audio_packet(3, 4, 2, &[0; 4])).map(|h| (h.kind, h.seq)), Some((KIND_AUDIO, 3)));
        assert!(AudioHeader::read(&p[..AUDIO_HEADER_LEN - 1]).is_none());
        assert!(AudioHeader::read(&[KIND_VIDEO; AUDIO_HEADER_LEN]).is_none());
        // The kinds stay distinct from every other datagram's.
        let kinds = [KIND_VIDEO, KIND_PING, KIND_PONG, KIND_KEYFRAME_REQUEST, KIND_AUDIO, KIND_MIC, KIND_AUDIO_OPUS];
        assert!(kinds.iter().enumerate().all(|(i, k)| !kinds[..i].contains(k)));
    }

    #[test]
    fn cursor_roundtrip_and_layout() {
        let c = CursorPos { stream: 1, flags: CURSOR_VISIBLE | CURSOR_YOURS, shape: 7, x: 0.25, y: 0.5 };
        let b = c.write();
        // Same bytes as web/proto.js's readCursor expects (tests/unit/proto.test.mjs).
        assert_eq!(&b[..8], &[KIND_CURSOR, 1, 3, 0, 7, 0, 0, 0]);
        assert_eq!(CursorPos::read(&b), Some(c));
        assert_eq!(CursorPos::read(&b[..15]), None);
    }

    #[test]
    fn pong_echoes_client_time() {
        let mut ping = [0u8; 16];
        ping[0] = KIND_PING;
        ping[8..16].copy_from_slice(&1234.5f64.to_le_bytes());
        let p = pong(&ping, 99).unwrap();
        assert_eq!(p[0], KIND_PONG);
        assert_eq!(f64::from_le_bytes(p[8..16].try_into().unwrap()), 1234.5);
        assert_eq!(u64::from_le_bytes(p[16..24].try_into().unwrap()), 99);
    }
}
