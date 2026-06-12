//! Clipboard transfer helpers.
//!
//! Once a selection offer's data fd is in hand, these drain the pipe the
//! compositor relays the owner's data through and decode it as text.

use crate::arena::{ArrayString, ArrayVec};
use crate::error::{Error, Result};
use crate::syscall::{self, Fd};

/// Largest clipboard paste captured. The launcher's input field is short, so a
/// huge paste is pointless and the rest is dropped.
pub const CLIP_CAP: usize = 4096;

/// How long to wait for the clipboard owner to produce data before giving up.
/// Without it a peer that opens the pipe but never writes or closes would hang
/// the paste forever.
const CLIPBOARD_TIMEOUT_MS: i32 = 2000;

/// Drain a clipboard pipe read end and decode it as UTF-8 text.
pub fn read_text(read_end: Fd) -> Result<ArrayString<CLIP_CAP>> {
    let buf = read_pipe(read_end)?;
    // The byte cap can split the final codepoint, so decode the valid prefix
    // rather than dropping the whole paste when the tail is a partial codepoint.
    // The prefix stops at the first invalid byte, so this still never mojibakes.
    let mut text: ArrayString<CLIP_CAP> = ArrayString::new();
    let valid_len = core::str::from_utf8(&buf).map_or_else(|e| e.valid_up_to(), |s| s.len());
    if let Ok(s) = core::str::from_utf8(&buf[..valid_len]) {
        let _ = text.push_str(s);
    }
    Ok(text)
}

/// Drain a pipe read end into a buffer, bounded by a byte cap and a per-wait
/// timeout. Each read is gated on poll so a peer that holds the pipe open
/// without writing cannot block us indefinitely.
fn read_pipe(read_end: Fd) -> Result<ArrayVec<u8, CLIP_CAP>> {
    let fd = read_end.as_raw_fd();
    let mut buf: ArrayVec<u8, CLIP_CAP> = ArrayVec::new();
    let mut chunk = [0u8; 8192];

    while !buf.is_full() {
        let mut pfd = syscall::pollfd {
            fd,
            events: syscall::POLLIN,
            revents: 0,
        };
        let ready = loop {
            let r = syscall::poll(core::slice::from_mut(&mut pfd), CLIPBOARD_TIMEOUT_MS);
            if r == -(syscall::EINTR as isize) {
                continue;
            }
            break r;
        };
        if ready < 0 {
            return Err(Error::from_errno(-ready as i32));
        }
        if ready == 0 {
            return Err(Error::msg("clipboard read timed out"));
        }

        let want = chunk.len().min(buf.remaining());
        let n = loop {
            let r = syscall::read_fd(fd, &mut chunk[..want]);
            if r == -(syscall::EINTR as isize) {
                continue;
            }
            break r;
        };
        if n < 0 {
            let err = -n as i32;
            if err == syscall::EAGAIN {
                continue;
            }
            return Err(Error::from_errno(err));
        }
        if n == 0 {
            break; // EOF: the owner finished writing and closed its end.
        }
        let _ = buf.extend_from_slice(&chunk[..n as usize]);
    }

    Ok(buf)
}
