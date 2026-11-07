//! The clipboard, whole: the wl_data_device protocol state and the pipe the
//! data actually travels through.
//!
//! Wayland relays clipboard content between clients over a pipe. Reading the
//! selection means creating one, handing the write end to the compositor, and
//! draining the read end; owning the selection means the compositor hands us a
//! write end and asks for the bytes. Both require keyboard focus, which the
//! launcher holds while it is open.

use crate::editor::INPUT_CAP;
use crate::platform::arena::{ArrayString, ArrayVec};
use crate::platform::conn::Connection;
use crate::platform::error::{elog, Error, Result};
use crate::platform::protocol as proto;
use crate::platform::syscall::{self, Fd, RawFd};
use crate::platform::time::Instant;
use crate::platform::wire::Arg;

/// Largest clipboard paste captured. The launcher's input field is short, so a
/// huge paste is pointless and the rest is dropped.
pub const CLIP_CAP: usize = 4096;

/// Most destroyed sources awaiting the compositor's delete_id. Each one is
/// confirmed a round trip after its destroy, so even a key-repeated copy keeps
/// only a couple in flight.
const RETIRED_CAP: usize = 8;

/// How long a paste may take, from its request to the owner's last byte.
///
/// The deadline covers the whole transfer, not each read. A peer that opens the
/// pipe and never writes has to be given up on, but so does one that dribbles a
/// byte at a time: a per-read timeout alone lets it hold the paste open for as
/// long as it cares to.
const CLIPBOARD_TIMEOUT_MS: u64 = 2000;

/// A paste in flight: the pipe the clipboard's owner writes into, the bytes so
/// far, and when it started. The event loop polls the pipe and calls advance
/// when it is readable, so the launcher keeps working while the owner takes its
/// time.
pub struct Paste {
    read_end: Fd,
    buf: ArrayVec<u8, CLIP_CAP>,
    started: Instant,
}

impl Paste {
    /// A paste reading from read_end, which is made non-blocking so a read
    /// never waits on the owner.
    pub fn new(read_end: Fd) -> Result<Self> {
        let fd = read_end.as_raw_fd();
        let flags = syscall::fcntl(fd, syscall::F_GETFL, 0);
        if flags < 0 {
            return Err(Error::from_errno(-flags));
        }
        let r = syscall::fcntl(fd, syscall::F_SETFL, flags | syscall::O_NONBLOCK);
        if r < 0 {
            return Err(Error::from_errno(-r));
        }
        Ok(Self {
            read_end,
            buf: ArrayVec::new(),
            started: Instant::now(),
        })
    }

    /// The pipe, for the event loop to poll.
    pub fn fd(&self) -> RawFd {
        self.read_end.as_raw_fd()
    }

    /// Milliseconds left before the paste is given up on.
    pub fn remaining_ms(&self) -> u64 {
        CLIPBOARD_TIMEOUT_MS.saturating_sub(self.started.elapsed_ms())
    }

    /// Take whatever the pipe holds now, without waiting. True once the paste
    /// is done: the owner closed its end, or the cap was reached. An error once
    /// the deadline has passed with the owner still not done.
    pub fn advance(&mut self) -> Result<bool> {
        let mut chunk = [0u8; 4096];
        while !self.buf.is_full() {
            let want = chunk.len().min(self.buf.remaining());
            let n = syscall::read_fd(self.fd(), &mut chunk[..want]);
            if n > 0 {
                let _ = self.buf.extend_from_slice(&chunk[..n as usize]);
                continue;
            }
            if n == 0 {
                // EOF: the owner finished writing and closed its end.
                return Ok(true);
            }
            match -n as i32 {
                syscall::EINTR => continue,
                syscall::EAGAIN if self.remaining_ms() == 0 => {
                    return Err(Error::msg("clipboard read timed out"));
                }
                syscall::EAGAIN => return Ok(false),
                e => return Err(Error::from_errno(e)),
            }
        }
        Ok(true)
    }

    /// What has arrived, as text. The byte cap can split the final codepoint,
    /// so the valid prefix is kept rather than the whole paste dropped; the
    /// prefix stops at the first invalid byte, so this never mojibakes.
    pub fn text(&self) -> ArrayString<CLIP_CAP> {
        let buf = self.buf.as_slice();
        let mut text: ArrayString<CLIP_CAP> = ArrayString::new();
        let valid_len = core::str::from_utf8(buf).map_or_else(|e| e.valid_up_to(), |s| s.len());
        if let Ok(s) = core::str::from_utf8(&buf[..valid_len]) {
            let _ = text.push_str(s);
        }
        text
    }
}

/// What a paste gets at once: the text, when the launcher owns the selection,
/// or a transfer to wait on.
pub enum Received {
    Text(ArrayString<CLIP_CAP>),
    Pending(Paste),
}

/// Text MIME types accepted from the clipboard, ordered worst to best so the
/// derived Ord picks the richest one advertised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TextMime {
    Plain,
    Utf8,
}

impl TextMime {
    fn as_str(self) -> &'static str {
        match self {
            TextMime::Plain => "text/plain",
            TextMime::Utf8 => "text/plain;charset=utf-8",
        }
    }

    /// Map an advertised MIME string to one we accept, if any.
    fn from_mime(s: &str) -> Option<TextMime> {
        match s {
            "text/plain;charset=utf-8" => Some(TextMime::Utf8),
            "text/plain" => Some(TextMime::Plain),
            _ => None,
        }
    }
}

/// What the user asked the clipboard to do, deferred until the event loop can
/// service it: the copy and paste requests arrive on the keyboard, but they need
/// a round trip to the compositor.
#[derive(Default)]
pub enum Op {
    #[default]
    None,
    Copy(ArrayString<INPUT_CAP>),
    Cut(ArrayString<INPUT_CAP>),
    Paste,
}

/// The clipboard's protocol state.
///
/// The compositor describes an incoming clipboard in three steps: a data_offer
/// naming a new object, an offer event per MIME type it can supply, and a
/// selection event promoting one of those offers to the clipboard (or clearing
/// it). So the offer being described is tracked apart from the one in force.
#[derive(Default)]
pub struct Clipboard {
    /// The per-seat data device, our handle on the clipboard.
    pub device_id: Option<u32>,
    /// The data source we own while we are the clipboard owner.
    source_id: Option<u32>,
    /// The text that source serves.
    source_text: Option<ArrayString<INPUT_CAP>>,
    /// Sources we destroyed whose deletion the compositor has not confirmed. A
    /// send it queued for one before reading the destroy still arrives, and its
    /// pipe fd has to come off the connection's queue, or every later fd lands
    /// on the event after the one it came with.
    retired_sources: ArrayVec<u32, RETIRED_CAP>,
    /// The offer the compositor is describing, before it says what it is for.
    pending_offer: Option<u32>,
    /// The best text MIME seen on that offer so far.
    pending_mime: Option<TextMime>,
    /// The offer the compositor named as the clipboard, with its MIME.
    selection_offer: Option<u32>,
    selection_mime: Option<TextMime>,
}

impl Clipboard {
    /// Handle a clipboard event. True when the message was one of ours.
    pub fn handle(
        &mut self,
        conn: &mut Connection,
        object: u32,
        opcode: u16,
        body: &[u8],
    ) -> Result<bool> {
        if Some(object) == self.device_id {
            let mut r = crate::platform::wire::Reader::new(body);
            match opcode {
                proto::wl_data_device::EV_DATA_OFFER => {
                    let offer_id = r.u32()?;
                    // A second offer before any selection supersedes the first.
                    // Nothing will name the old one again, so destroy it, or it
                    // stays alive in the compositor for the whole session.
                    if let Some(old) = self.pending_offer.replace(offer_id) {
                        let _ = conn.request(old, proto::wl_data_offer::DESTROY, &[]);
                    }
                    self.pending_mime = None;
                }
                proto::wl_data_device::EV_SELECTION => {
                    let offer_id = r.u32()?;
                    let pending = self.pending_offer.take();
                    let pending_mime = self.pending_mime.take();

                    // Retire the offer held before this selection.
                    if let Some(old) = self.selection_offer.take() {
                        let _ = conn.request(old, proto::wl_data_offer::DESTROY, &[]);
                    }
                    self.selection_mime = None;
                    if offer_id != 0 {
                        self.selection_offer = Some(offer_id);
                        // The MIME list was collected against the pending offer,
                        // so it describes this selection only if they match.
                        if pending == Some(offer_id) {
                            self.selection_mime = pending_mime;
                        }
                    }
                    // A cleared selection, or one naming some other offer, leaves
                    // the pending offer with no owner.
                    if let Some(p) = pending {
                        if Some(p) != self.selection_offer {
                            let _ = conn.request(p, proto::wl_data_offer::DESTROY, &[]);
                        }
                    }
                }
                _ => {}
            }
            return Ok(true);
        }

        // The MIME types advertised on the offer being described. The owner
        // picks these strings: one that is not a type we accept is not worth
        // failing over, so a malformed event is dropped, not the session.
        if Some(object) == self.pending_offer && opcode == proto::wl_data_offer::EV_OFFER {
            let mut r = crate::platform::wire::Reader::new(body);
            if let Ok(mime) = r.string() {
                if let Some(m) = TextMime::from_mime(mime) {
                    self.pending_mime = Some(match self.pending_mime {
                        Some(cur) => cur.max(m),
                        None => m,
                    });
                }
            }
            return Ok(true);
        }

        // Our own source: serve the text we copied, and notice if we lose it.
        if Some(object) == self.source_id {
            match opcode {
                proto::wl_data_source::EV_SEND => {
                    // The compositor asks us to write the clipboard to an fd. The
                    // MIME it names is one of the two we advertised, and both are
                    // served the same bytes, so the argument is not read: parsing
                    // it would only add a way for this to fail.
                    if let Some(fd) = conn.take_fd() {
                        if let Some(ref text) = self.source_text {
                            send_text(fd.as_raw_fd(), text.as_bytes());
                        }
                        // fd closes as it drops here, which is the EOF the
                        // reader on the other end is waiting for.
                    }
                }
                proto::wl_data_source::EV_CANCELLED => {
                    // Another app took the clipboard.
                    self.retire_source(conn);
                }
                _ => {}
            }
            return Ok(true);
        }

        if self.retired_sources.contains(&object) {
            if opcode == proto::wl_data_source::EV_SEND {
                // Closing the fd unread gives the reader an empty transfer.
                drop(conn.take_fd());
            }
            return Ok(true);
        }

        Ok(false)
    }

    /// Destroy the source we own, keeping its id until the compositor confirms
    /// the deletion.
    fn retire_source(&mut self, conn: &mut Connection) {
        self.source_text = None;
        let Some(source) = self.source_id.take() else {
            return;
        };
        let _ = conn.request(source, proto::wl_data_source::DESTROY, &[]);
        // Full only when the compositor has stopped confirming deletions. The
        // oldest is then the least likely to still be sent to.
        if self.retired_sources.is_full() {
            self.retired_sources.remove(0);
        }
        let _ = self.retired_sources.push(source);
    }

    /// The compositor has deleted this id, so no more events can name the
    /// source it was, and the id may be handed out again.
    pub fn release_id(&mut self, id: u32) {
        self.retired_sources.retain(|&retired| retired != id);
    }

    /// Take ownership of the clipboard and serve text from it.
    ///
    /// The serial must come from the input event that triggered the copy; the
    /// compositor rejects a stale one.
    pub fn set(
        &mut self,
        conn: &mut Connection,
        manager_id: Option<u32>,
        next_id: &mut impl FnMut() -> u32,
        serial: u32,
        text: &str,
    ) -> Result<()> {
        let manager_id = manager_id.ok_or_else(|| Error::msg("no data device manager"))?;
        let device_id = self.device_id.ok_or_else(|| Error::msg("no data device"))?;

        self.retire_source(conn);

        let source_id = next_id();
        conn.request(
            manager_id,
            proto::wl_data_device_manager::CREATE_DATA_SOURCE,
            &[Arg::NewId(source_id)],
        )?;
        for mime in [TextMime::Utf8, TextMime::Plain] {
            conn.request(
                source_id,
                proto::wl_data_source::OFFER,
                &[Arg::Str(mime.as_str())],
            )?;
        }
        conn.request(
            device_id,
            proto::wl_data_device::SET_SELECTION,
            &[Arg::Object(source_id), Arg::Uint(serial)],
        )?;
        conn.flush()?;

        self.source_id = Some(source_id);
        let mut stored: ArrayString<INPUT_CAP> = ArrayString::new();
        let _ = stored.push_str(text);
        self.source_text = Some(stored);
        Ok(())
    }

    /// Ask for the text on the clipboard. When the launcher owns the selection
    /// the text is already in hand; otherwise the owner is asked to write it
    /// into a pipe, and the paste waits on that.
    pub fn receive(&mut self, conn: &mut Connection) -> Result<Received> {
        if let Some(ref owned) = self.source_text {
            let mut text: ArrayString<CLIP_CAP> = ArrayString::new();
            let _ = text.push_str(owned.as_str());
            return Ok(Received::Text(text));
        }

        let offer_id = self
            .selection_offer
            .ok_or_else(|| Error::msg("no clipboard selection"))?;
        let mime = self
            .selection_mime
            .ok_or_else(|| Error::msg("the clipboard has no text on it"))?;

        let mut fds = [0i32; 2];
        let r = syscall::pipe2(&mut fds, syscall::O_CLOEXEC);
        if r < 0 {
            return Err(Error::from_errno(-r));
        }
        // Own both ends at once, so an early return closes them. Both come fresh
        // from pipe2 and are owned by nobody else.
        let read_end = Fd::new(fds[0]);
        let write_end = Fd::new(fds[1]);

        conn.request_with_fd(
            offer_id,
            proto::wl_data_offer::RECEIVE,
            &[Arg::Str(mime.as_str())],
            &[write_end.as_raw_fd()],
        )?;
        conn.flush()?;

        // Drop our write end: the compositor keeps its own copy through
        // SCM_RIGHTS, so the pipe reaches EOF once the owner has finished.
        drop(write_end);

        Ok(Received::Pending(Paste::new(read_end)?))
    }
}

/// Hand the selection's text to whoever asked for it. The pipe is the
/// requester's, so it is made non-blocking first: one that never drains its
/// pipe must not stall the launcher. The text is at most INPUT_CAP bytes, under
/// a pipe's smallest capacity, so an empty pipe takes it all in one write, and a
/// requester whose pipe cannot gets what fit.
fn send_text(fd: RawFd, data: &[u8]) {
    let flags = syscall::fcntl(fd, syscall::F_GETFL, 0);
    if flags >= 0 {
        syscall::fcntl(fd, syscall::F_SETFL, flags | syscall::O_NONBLOCK);
    }
    let mut offset = 0;
    while offset < data.len() {
        let n = syscall::write_fd(fd, &data[offset..]);
        if n == -(syscall::EINTR as isize) {
            continue;
        }
        if n <= 0 {
            break;
        }
        offset += n as usize;
    }
    if offset < data.len() {
        elog!(
            "bnklaunch: a clipboard request took {offset} of {} bytes: its pipe is full",
            data.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pipe whose write end is filled with data and closed, standing in for a
    /// clipboard owner that hands over its text and finishes.
    fn pipe_with(data: &[u8]) -> Fd {
        let mut fds = [0i32; 2];
        assert_eq!(syscall::pipe2(&mut fds, syscall::O_CLOEXEC), 0);
        let (read_end, write_end) = (Fd::new(fds[0]), Fd::new(fds[1]));
        let mut off = 0;
        while off < data.len() {
            let n = syscall::write_fd(write_end.as_raw_fd(), &data[off..]);
            assert!(n > 0, "write to pipe failed");
            off += n as usize;
        }
        drop(write_end); // EOF for the reader
        read_end
    }

    /// Advance a paste whose owner has already written everything and closed.
    fn pasted(data: &[u8]) -> ArrayString<CLIP_CAP> {
        let mut paste = Paste::new(pipe_with(data)).expect("paste");
        assert!(
            paste.advance().expect("advance"),
            "a closed pipe is a finished paste"
        );
        paste.text()
    }

    #[test]
    fn reads_until_the_writer_closes() {
        assert_eq!(pasted(b"hello clipboard"), "hello clipboard");
    }

    #[test]
    fn empty_pipe_yields_empty_text() {
        assert!(pasted(b"").is_empty());
    }

    #[test]
    fn stops_at_the_cap() {
        let data = vec![b'x'; CLIP_CAP + 100];
        assert_eq!(pasted(&data).len(), CLIP_CAP);
    }

    #[test]
    fn a_codepoint_split_by_the_cap_is_dropped_not_mangled() {
        // Fill to one byte short of the cap, then push a 2-byte char across it.
        // The cap keeps the leading byte only, and decoding stops before it.
        let mut data = vec![b'a'; CLIP_CAP - 1];
        data.extend_from_slice("é".as_bytes());
        let text = pasted(&data);
        assert_eq!(text.len(), CLIP_CAP - 1);
        assert!(text.as_str().chars().all(|c| c == 'a'));
    }

    /// The owner writes in pieces. Each advance takes what is there and returns
    /// without waiting for more, and the text is whole once the owner closes.
    #[test]
    fn a_paste_advances_as_its_owner_writes() {
        let mut fds = [0i32; 2];
        assert_eq!(syscall::pipe2(&mut fds, syscall::O_CLOEXEC), 0);
        let (read_end, write_end) = (Fd::new(fds[0]), Fd::new(fds[1]));
        let mut paste = Paste::new(read_end).expect("paste");

        assert_eq!(paste.advance(), Ok(false));
        assert_eq!(syscall::write_fd(write_end.as_raw_fd(), b"ab"), 2);
        assert_eq!(paste.advance(), Ok(false));
        assert_eq!(syscall::write_fd(write_end.as_raw_fd(), b"c"), 1);
        drop(write_end);
        assert_eq!(paste.advance(), Ok(true), "the owner closed");
        assert_eq!(paste.text(), "abc");
    }

    #[test]
    fn an_owner_that_never_writes_runs_out_the_deadline() {
        // The write end stays open and idle, so no data and no EOF ever arrive.
        let mut fds = [0i32; 2];
        assert_eq!(syscall::pipe2(&mut fds, syscall::O_CLOEXEC), 0);
        let (read_end, _write_end) = (Fd::new(fds[0]), Fd::new(fds[1]));
        let mut paste = Paste::new(read_end).expect("paste");
        let start = Instant::now();
        while paste.remaining_ms() > 0 {
            assert_eq!(paste.advance(), Ok(false));
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(paste.advance().is_err());
        assert!(start.elapsed_ms() >= CLIPBOARD_TIMEOUT_MS);
    }
}
