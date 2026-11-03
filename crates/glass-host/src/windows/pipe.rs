//! A named-pipe handle opened for overlapped I/O, read on one thread and written on another.
//! With ordinary (synchronous) handles Windows runs one operation at a time per file
//! object, even through duplicated handles: a read waiting for data blocks every write, so
//! two sides each waiting to read before they write never get anywhere. Each operation
//! here is overlapped and then waited for, so reads and writes go at once.

use std::io;

use windows::Win32::Foundation::{CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::CreateEventW;

pub struct Pipe(HANDLE);

// SAFETY: a kernel handle; overlapped operations on it may run from any thread at once.
unsafe impl Send for Pipe {}
unsafe impl Sync for Pipe {}

impl Pipe {
    /// Takes ownership of a handle opened with FILE_FLAG_OVERLAPPED.
    pub fn from_handle(handle: HANDLE) -> Self {
        Self(handle)
    }

    pub fn handle(&self) -> HANDLE {
        self.0
    }

    /// Opens the client end of `name` for overlapped I/O.
    pub fn open(name: &str) -> io::Result<Self> {
        use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING};
        // SAFETY: a NUL-terminated name; the handle is owned by the returned Pipe.
        let handle = unsafe {
            CreateFileW(
                &windows::core::HSTRING::from(name),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(io::Error::from)?;
        Ok(Self(handle))
    }

    /// Server end: waits for a client to connect. True when one is there.
    pub fn connect(&self) -> bool {
        use windows::Win32::System::Pipes::ConnectNamedPipe;
        // SAFETY: a server pipe handle and a live OVERLAPPED.
        self.overlapped(|ov| unsafe { ConnectNamedPipe(self.0, Some(ov)) }).is_ok()
    }

    /// Runs one overlapped operation and waits for it to finish; how many bytes it moved
    /// (0 at the end of the pipe).
    fn overlapped(&self, start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>) -> io::Result<usize> {
        // SAFETY: the event and OVERLAPPED live until the operation has finished (we wait
        // for it before returning); the event is closed after.
        unsafe {
            let event = CreateEventW(None, true, false, None).map_err(io::Error::from)?;
            let mut ov = OVERLAPPED { hEvent: event, ..Default::default() };
            let result = match start(&mut ov) {
                Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => Ok(0),
                Err(e) if e.code() != ERROR_IO_PENDING.to_hresult() => Err(io::Error::from(e)),
                // Done already, or under way: either way the count comes from here.
                _ => {
                    let mut n = 0u32;
                    GetOverlappedResult(self.0, &ov, &mut n, true).map(|_| n as usize).map_err(io::Error::from)
                }
            };
            let _ = CloseHandle(event);
            match result {
                Err(e) if e.raw_os_error() == Some(ERROR_BROKEN_PIPE.0 as i32) => Ok(0),
                other => other,
            }
        }
    }
}

impl io::Read for &Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: buf outlives the wait in `overlapped`.
        self.overlapped(|ov| unsafe { ReadFile(self.0, Some(buf), None, Some(ov)) })
    }
}

impl io::Write for &Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: buf outlives the wait in `overlapped`.
        self.overlapped(|ov| unsafe { WriteFile(self.0, Some(buf), None, Some(ov)) })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // SAFETY: owned handle, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}
