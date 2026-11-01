//! Hands captured frames from a capture thread to an encoder thread, newest only.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::soft_encode::Frame;

/// Hands captured frames to a monitor's encoder. Holds only the newest: a frame that
/// arrives while the encoder is busy replaces the one waiting, so the encoder never
/// starts on a picture that went stale in a queue. Pixel buffers are reused, so a 4K
/// frame (33 MB) is copied into memory that's already mapped, not freshly allocated.
#[derive(Default)]
pub struct Mailbox {
    state: Mutex<MailboxState>,
    ready: Condvar,
}

#[derive(Default)]
struct MailboxState {
    frame: Option<(Frame, u64)>,
    spare: Option<Vec<u8>>,
    closed: bool,
}

impl Mailbox {
    /// A buffer to copy the next frame into: the waiting frame's (it's about to be
    /// replaced), else one the encoder gave back.
    pub fn buffer(&self) -> Vec<u8> {
        let mut state = self.state.lock().unwrap();
        state
            .frame
            .take()
            .map(|(f, _)| f.data)
            .or_else(|| state.spare.take())
            .unwrap_or_default()
    }

    pub fn put(&self, frame: Frame, captured_us: u64) {
        self.state.lock().unwrap().frame = Some((frame, captured_us));
        self.ready.notify_one();
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<(Frame, u64), RecvTimeoutError> {
        let state = self.state.lock().unwrap();
        let (mut state, _) = self
            .ready
            .wait_timeout_while(state, timeout, |s| s.frame.is_none() && !s.closed)
            .unwrap();
        match state.frame.take() {
            Some(f) => Ok(f),
            None if state.closed => Err(RecvTimeoutError::Disconnected),
            None => Err(RecvTimeoutError::Timeout),
        }
    }

    /// Gives a frame's buffer back for the next capture.
    pub fn recycle(&self, data: Vec<u8>) {
        self.state.lock().unwrap().spare = Some(data);
    }
}

/// The capture side of a mailbox; dropping it tells the encoder capture has stopped.
pub struct FrameSender(pub std::sync::Arc<Mailbox>);

impl Drop for FrameSender {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().closed = true;
        self.0.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::soft_encode::PixelFormat;

    fn frame(fill: u8) -> Frame {
        Frame {
            width: 2,
            height: 1,
            stride: 8,
            format: PixelFormat::Bgrx,
            data: vec![fill; 8],
        }
    }

    #[test]
    fn mailbox_keeps_only_the_newest_frame() {
        let mailbox = Arc::new(Mailbox::default());
        let tx = FrameSender(mailbox.clone());
        tx.0.put(frame(1), 1);
        tx.0.put(frame(2), 2);
        let (f, at) = mailbox.recv_timeout(Duration::ZERO).unwrap();
        assert_eq!((f.data[0], at), (2, 2));
        assert!(matches!(mailbox.recv_timeout(Duration::from_millis(5)), Err(RecvTimeoutError::Timeout)));
        drop(tx);
        assert!(matches!(mailbox.recv_timeout(Duration::from_secs(5)), Err(RecvTimeoutError::Disconnected)));
    }

    #[test]
    fn mailbox_reuses_buffers() {
        let mailbox = Mailbox::default();
        let data = Vec::with_capacity(4096);
        let ptr = data.as_ptr();
        mailbox.recycle(data);
        let buffer = mailbox.buffer();
        assert_eq!(buffer.as_ptr(), ptr, "the encoder's spare");
        let mut waiting = frame(3);
        waiting.data.reserve(4096);
        let ptr = waiting.data.as_ptr();
        mailbox.put(waiting, 0);
        let buffer = mailbox.buffer();
        assert_eq!(buffer.as_ptr(), ptr, "the frame it replaces");
    }
}
