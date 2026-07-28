//! The Wayland connection: owns the socket, buffers outgoing requests, and
//! parses framed messages out of the incoming byte stream.
//!
//! File descriptors travel out of band, as SCM_RIGHTS ancillary data, and the
//! cmsg plumbing for that lives in syscall alongside the other kernel-facing
//! code, so nothing here builds a msghdr.
//!
//! The socket is non-blocking, because the event loop waits on it through
//! io_uring rather than inside a read. Two consequences shape the code here.
//! fill takes no timeout: the waiting happens a layer up, and a drained socket
//! simply reports a would-block. And flush has to answer a would-block itself
//! rather than pass it on, because a write can be cut short halfway through a
//! request: the bytes already in the compositor's buffer are half a message, and
//! only finishing them makes the stream whole again.

use crate::platform::arena::{ArrayString, ArrayVec};
use crate::platform::env;
use crate::platform::error::{Error, Result};
use crate::platform::syscall::{
    self, pollfd, sockaddr_un, Fd, RawFd, AF_UNIX, EAGAIN, EINTR, F_GETFL, F_SETFL, O_NONBLOCK,
    POLLIN, POLLOUT, SOCK_CLOEXEC, SOCK_STREAM,
};
use crate::platform::wire::{self, Arg, Message, HEADER_SIZE, MAX_MESSAGE_SIZE, MSG_BODY_CAP};

/// Incoming-data buffer: a full message plus headroom for the next partial read.
const RECV_CAP: usize = 2 * MAX_MESSAGE_SIZE;
/// Outgoing-request buffer. A frame is attach, damage, and commit; the bind
/// burst at startup is a handful of short requests. Neither is close to this.
const OUT_CAP: usize = 8 * 1024;
/// Capacity of the scratch buffer one fd-carrying request is encoded into.
const REQUEST_CAP: usize = 1024;
/// Most received fds awaiting consumption at once.
const MAX_RECV_FDS: usize = 16;
/// How long to wait for the compositor's parting words after a failed send.
const DIAGNOSE_TIMEOUT_MS: i32 = 200;

/// A connection to the compositor.
pub struct Connection {
    fd: RawFd,
    /// Requests buffered since the last flush.
    out: ArrayVec<u8, OUT_CAP>,
    in_buf: ArrayVec<u8, RECV_CAP>,
    /// How many bytes at the front of in_buf next_message has already consumed.
    /// Each parsed message advances this cursor instead of shifting the rest of
    /// the buffer down, which would cost O(remaining) per message; the consumed
    /// prefix is compacted away once per fill.
    in_pos: usize,
    /// Received fds awaiting consumption, in arrival order. The wire protocol
    /// passes fds out of band but in the same order as the messages whose
    /// arguments declare them, so a message that carries an fd takes the front
    /// of this queue. Byte position cannot identify the owning message: one
    /// recvmsg may carry several messages, and the kernel reports the fds
    /// against the chunk, not the message.
    fds: ArrayVec<Fd, MAX_RECV_FDS>,
}

impl Connection {
    /// Connect to $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY (default wayland-0). An
    /// absolute WAYLAND_DISPLAY is used as it stands.
    pub fn connect() -> Result<Self> {
        let runtime_dir =
            env::var("XDG_RUNTIME_DIR").ok_or_else(|| Error::msg("XDG_RUNTIME_DIR is not set"))?;
        let display = env::var("WAYLAND_DISPLAY").unwrap_or("wayland-0");

        let mut path: ArrayString<{ crate::platform::fs::PATH_CAP }> = ArrayString::new();
        let built = if display.starts_with('/') {
            path.push_str(display)
        } else {
            path.push_str(runtime_dir)
                .and_then(|_| path.push('/'))
                .and_then(|_| path.push_str(display))
        };
        built.map_err(|_| Error::msg("wayland socket path too long"))?;

        Self::connect_to(&path)
    }

    /// Connect to a specific socket path.
    pub fn connect_to(path: &str) -> Result<Self> {
        let bytes = path.as_bytes();
        // SAFETY: sockaddr_un is a plain C struct of an integer and a byte
        // array, so an all-zero bit pattern is a valid value and leaves the path
        // NUL-terminated.
        let mut addr: sockaddr_un = unsafe { core::mem::zeroed() };
        addr.sun_family = AF_UNIX as u16;
        // Leave room for the trailing NUL the kernel expects.
        if bytes.len() >= addr.sun_path.len() {
            return Err(Error::msg("socket path too long"));
        }
        addr.sun_path[..bytes.len()].copy_from_slice(bytes);

        let fd = syscall::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(Error::from_errno(-fd));
        }

        // addrlen spans the family plus the used path bytes and their NUL.
        let addrlen = (core::mem::size_of::<u16>() + bytes.len() + 1) as u32;
        let r = syscall::connect(fd, &addr, addrlen);
        if r < 0 {
            // SAFETY: fd is the socket opened just above, closed once on this
            // error path before the owning struct exists.
            unsafe { syscall::close(fd) };
            return Err(Error::from_errno(-r));
        }

        Ok(Self {
            fd,
            out: ArrayVec::new(),
            in_buf: ArrayVec::new(),
            in_pos: 0,
            fds: ArrayVec::new(),
        })
    }

    /// The socket's raw fd, so the event loop can poll it. Read-only: the
    /// connection keeps ownership and does all the actual I/O.
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Queue a request for the next flush.
    pub fn request(&mut self, object: u32, opcode: u16, args: &[Arg]) -> Result<()> {
        wire::encode(&mut self.out, object, opcode, args)
    }

    /// Send everything queued so far, then send one request carrying fds as
    /// ancillary data. Flushing first keeps the fds attached to the request that
    /// declares them, with no earlier bytes riding along.
    pub fn request_with_fd(
        &mut self,
        object: u32,
        opcode: u16,
        args: &[Arg],
        fds: &[RawFd],
    ) -> Result<()> {
        self.flush()?;

        let mut msg: ArrayVec<u8, REQUEST_CAP> = ArrayVec::new();
        wire::encode(&mut msg, object, opcode, args)?;

        let sent = loop {
            let r = syscall::send_with_fds(self.fd, &msg, fds);
            if r >= 0 {
                break r as usize;
            }
            match -r as i32 {
                EINTR => continue,
                EAGAIN => {
                    self.wait_writable()?;
                    continue;
                }
                e => return Err(self.diagnose_send_error(Error::from_errno(e))),
            }
        };

        // The fds ride with the first byte, so any remainder is plain data. Push
        // it through the buffered path, which knows how to finish a short write.
        if sent < msg.len() {
            self.out
                .extend_from_slice(&msg[sent..])
                .map_err(|_| Error::msg("wayland send buffer full"))?;
            self.flush()?;
        }
        Ok(())
    }

    /// Write every queued request to the socket.
    ///
    /// A partial write followed by EAGAIN is the case this exists for. The
    /// socket is non-blocking, so a compositor that is slow to drain its end
    /// leaves half a request in flight; returning an error there would strand
    /// the tail and the next request would be appended straight onto it, which
    /// the compositor reads as a corrupt stream and kills the connection over.
    /// Wait for the socket to become writable and finish the job.
    pub fn flush(&mut self) -> Result<()> {
        let mut sent = 0;
        while sent < self.out.len() {
            let r = syscall::send_with_fds(self.fd, &self.out[sent..], &[]);
            if r >= 0 {
                sent += r as usize;
                continue;
            }
            match -r as i32 {
                EINTR => continue,
                EAGAIN => self.wait_writable()?,
                e => {
                    // The connection is dead; drop the queue rather than let a
                    // later flush resend a fragment of it.
                    self.out.clear();
                    return Err(self.diagnose_send_error(Error::from_errno(e)));
                }
            }
        }
        self.out.clear();
        Ok(())
    }

    /// Block until the socket can take more bytes. Only reached mid-write, when
    /// the compositor's receive buffer is full, which is exactly when a blocking
    /// socket would have parked in write anyway.
    fn wait_writable(&self) -> Result<()> {
        let mut pfd = pollfd {
            fd: self.fd,
            events: POLLOUT,
            revents: 0,
        };
        loop {
            let r = syscall::poll(core::slice::from_mut(&mut pfd), -1);
            if r == -(EINTR as isize) {
                continue;
            }
            if r < 0 {
                return Err(Error::from_errno(-r as i32));
            }
            return Ok(());
        }
    }

    /// A send failed, typically with EPIPE: the compositor closed the socket. On
    /// a protocol violation it sends wl_display.error and then closes, and since
    /// we were writing we never read it, so the errno alone only says "peer
    /// gone". Drain whatever it sent before disconnecting and surface that
    /// instead, which is the real reason. Best effort: on failure, fall back to
    /// the send error.
    fn diagnose_send_error(&mut self, send_err: Error) -> Error {
        let mut chunk = [0u8; 4096];
        loop {
            let mut pfd = pollfd {
                fd: self.fd,
                events: POLLIN,
                revents: 0,
            };
            let ready = syscall::poll(core::slice::from_mut(&mut pfd), DIAGNOSE_TIMEOUT_MS);
            if ready <= 0 {
                break; // idle, timed out, or errored: nothing more is coming
            }
            let mut fds: ArrayVec<Fd, MAX_RECV_FDS> = ArrayVec::new();
            match syscall::recv_with_fds(self.fd, &mut chunk, &mut fds) {
                // 0 bytes means the peer closed; anything negative is a reset.
                Ok(n) if n > 0 => {
                    if self.in_buf.extend_from_slice(&chunk[..n as usize]).is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }

        // Scan the buffered stream for wl_display.error. Both ids are fixed by
        // the protocol, so this needs no interface knowledge.
        let mut off = self.in_pos;
        while off + HEADER_SIZE <= self.in_buf.len() {
            let Ok((object, opcode, size)) = wire::parse_header(&self.in_buf[off..]) else {
                break;
            };
            if off + size > self.in_buf.len() {
                break;
            }
            if object == crate::platform::protocol::WL_DISPLAY
                && opcode == crate::platform::protocol::wl_display::EV_ERROR
            {
                let mut r = wire::Reader::new(&self.in_buf[off + HEADER_SIZE..off + size]);
                if let (Ok(bad_object), Ok(code), Ok(text)) = (r.u32(), r.u32(), r.string()) {
                    crate::platform::error::elog!(
                        "bnklaunch: wayland protocol error from object {bad_object} (code {code}): {text}"
                    );
                    return Error::msg("the compositor reported a protocol error");
                }
            }
            off += size;
        }
        send_err
    }

    /// Read whatever the compositor has sent into the input buffer. Any fds that
    /// arrive as ancillary data (a keyboard keymap, a clipboard transfer fd) are
    /// queued for take_fd. Returns the bytes read; a would-block error is how a
    /// drained socket reports that it is empty.
    pub fn fill(&mut self) -> Result<usize> {
        // Drop the messages consumed since the last fill in one shift, rather
        // than shifting per message in next_message.
        self.compact();

        let mut chunk = [0u8; MAX_MESSAGE_SIZE];
        let n = syscall::recv_with_fds(self.fd, &mut chunk, &mut self.fds)?;
        if n < 0 {
            return Err(Error::from_errno(-n as i32));
        }
        if n == 0 {
            return Err(Error::msg("the compositor closed the connection"));
        }
        let n = n as usize;
        self.in_buf
            .extend_from_slice(&chunk[..n])
            .map_err(|_| Error::msg("wayland receive buffer overflow"))?;
        Ok(n)
    }

    /// Pull one fully buffered message off the front of the stream, if there is
    /// one. None means the buffer holds only a partial message and the caller
    /// should fill.
    pub fn next_message(&mut self) -> Result<Option<Message>> {
        let buf = &self.in_buf[self.in_pos..];
        if buf.len() < HEADER_SIZE {
            return Ok(None);
        }
        let (object, opcode, size) = wire::parse_header(buf)?;
        if buf.len() < size {
            return Ok(None);
        }

        let mut body: ArrayVec<u8, MSG_BODY_CAP> = ArrayVec::new();
        body.extend_from_slice(&buf[HEADER_SIZE..size])
            .map_err(|_| Error::msg("wayland message body too large"))?;
        self.in_pos += size;

        Ok(Some(Message {
            object,
            opcode,
            body,
        }))
    }

    /// Take the next received fd in arrival order, for a message whose argument
    /// declares one. None when none is queued.
    pub fn take_fd(&mut self) -> Option<Fd> {
        self.fds.remove(0)
    }

    /// Discard the already-consumed prefix of the input buffer in a single
    /// shift, so a burst of messages costs one compaction rather than one per
    /// message.
    fn compact(&mut self) {
        if self.in_pos == 0 {
            return;
        }
        let remaining = self.in_buf.len() - self.in_pos;
        self.in_buf.copy_within(self.in_pos.., 0);
        self.in_buf.truncate(remaining);
        self.in_pos = 0;
    }

    /// Toggle O_NONBLOCK on the socket.
    pub fn set_nonblocking(&mut self, nonblocking: bool) -> Result<()> {
        let flags = syscall::fcntl(self.fd, F_GETFL, 0);
        if flags < 0 {
            return Err(Error::from_errno(-flags));
        }
        let updated = if nonblocking {
            flags | O_NONBLOCK
        } else {
            flags & !O_NONBLOCK
        };
        let r = syscall::fcntl(self.fd, F_SETFL, updated);
        if r < 0 {
            return Err(Error::from_errno(-r));
        }
        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: self.fd is the socket opened in connect_to, closed once here.
        // Any unconsumed received fds close themselves as their Fd values drop.
        unsafe { syscall::close(self.fd) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::protocol as proto;

    /// A Connection wired to one end of a socketpair, with the other end handed
    /// back so a test can play compositor. No real compositor, no mocks: the
    /// same syscalls the real connection makes, against a real socket.
    fn connected_pair() -> (Connection, Fd) {
        let mut fds = [0i32; 2];
        let r = syscall::socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, &mut fds);
        assert_eq!(r, 0, "socketpair failed");
        let conn = Connection {
            fd: fds[0],
            out: ArrayVec::new(),
            in_buf: ArrayVec::new(),
            in_pos: 0,
            fds: ArrayVec::new(),
        };
        (conn, Fd::new(fds[1]))
    }

    /// Everything the peer end can read right now.
    fn drain(peer: &Fd) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let mut pfd = pollfd {
                fd: peer.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            };
            if syscall::poll(core::slice::from_mut(&mut pfd), 100) <= 0 {
                break;
            }
            let n = syscall::read_fd(peer.as_raw_fd(), &mut chunk);
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n as usize]);
        }
        out
    }

    #[test]
    fn requests_are_buffered_until_flush() {
        let (mut conn, peer) = connected_pair();
        conn.request(1, proto::wl_display::SYNC, &[Arg::NewId(2)])
            .expect("queue");
        assert!(drain(&peer).is_empty(), "nothing goes out before a flush");

        conn.flush().expect("flush");
        let sent = drain(&peer);
        let (object, opcode, size) = wire::parse_header(&sent).expect("header");
        assert_eq!((object, opcode, size), (1, proto::wl_display::SYNC, 12));
    }

    #[test]
    fn a_flush_coalesces_every_queued_request_into_one_write() {
        let (mut conn, peer) = connected_pair();
        // The frame path: attach, damage, commit. One hand-off, in order.
        for op in [
            proto::wl_surface::ATTACH,
            proto::wl_surface::DAMAGE,
            proto::wl_surface::COMMIT,
        ] {
            conn.request(7, op, &[]).expect("queue");
        }
        conn.flush().expect("flush");

        let sent = drain(&peer);
        let mut off = 0;
        for want in [
            proto::wl_surface::ATTACH,
            proto::wl_surface::DAMAGE,
            proto::wl_surface::COMMIT,
        ] {
            let (object, opcode, size) = wire::parse_header(&sent[off..]).expect("header");
            assert_eq!(object, 7);
            assert_eq!(opcode, want);
            off += size;
        }
        assert_eq!(off, sent.len(), "no trailing bytes");
    }

    #[test]
    fn messages_are_framed_out_of_a_partial_stream() {
        let (mut conn, peer) = connected_pair();

        // The peer writes one and a half messages: a whole sync callback done,
        // then only the header of the next.
        let mut whole: ArrayVec<u8, 64> = ArrayVec::new();
        wire::encode(&mut whole, 3, proto::wl_callback::EV_DONE, &[Arg::Uint(9)]).expect("encode");
        let mut partial: ArrayVec<u8, 64> = ArrayVec::new();
        wire::encode(
            &mut partial,
            4,
            proto::wl_display::EV_DELETE_ID,
            &[Arg::Uint(5)],
        )
        .expect("encode");

        let mut stream = Vec::new();
        stream.extend_from_slice(&whole);
        stream.extend_from_slice(&partial[..4]); // half a header
        assert!(syscall::write_fd(peer.as_raw_fd(), &stream) > 0);

        conn.fill().expect("fill");
        let msg = conn.next_message().expect("parse").expect("one message");
        assert_eq!(msg.object, 3);
        assert_eq!(msg.opcode, proto::wl_callback::EV_DONE);
        assert_eq!(msg.reader().u32().expect("arg"), 9);

        // The half message is not a message yet.
        assert!(conn.next_message().expect("parse").is_none());

        // The rest arrives and completes it.
        assert!(syscall::write_fd(peer.as_raw_fd(), &partial[4..]) > 0);
        conn.fill().expect("fill");
        let msg = conn.next_message().expect("parse").expect("second message");
        assert_eq!(msg.object, 4);
        assert_eq!(msg.opcode, proto::wl_display::EV_DELETE_ID);
    }

    #[test]
    fn the_consumed_prefix_is_compacted_once_per_fill() {
        let (mut conn, peer) = connected_pair();
        let mut stream: ArrayVec<u8, 256> = ArrayVec::new();
        for i in 0..4u32 {
            wire::encode(&mut stream, i + 1, 0, &[Arg::Uint(i)]).expect("encode");
        }
        assert!(syscall::write_fd(peer.as_raw_fd(), &stream) > 0);

        conn.fill().expect("fill");
        for i in 0..4u32 {
            let msg = conn.next_message().expect("parse").expect("message");
            assert_eq!(msg.object, i + 1);
        }
        assert!(conn.next_message().expect("parse").is_none());
        // Four messages read, one cursor: the buffer was never shifted.
        assert_eq!(conn.in_pos, stream.len());

        // The next fill drops all four in a single shift.
        assert!(syscall::write_fd(peer.as_raw_fd(), &stream[..12]) > 0);
        conn.fill().expect("fill");
        assert_eq!(conn.in_pos, 0);
        assert_eq!(conn.in_buf.len(), 12);
    }

    #[test]
    fn a_passed_fd_arrives_on_the_queue() {
        let (mut conn, peer) = connected_pair();

        // The peer passes an fd the way the compositor passes a keymap.
        let mut pipe = [0i32; 2];
        assert_eq!(syscall::pipe2(&mut pipe, syscall::O_CLOEXEC), 0);
        let (read_end, write_end) = (Fd::new(pipe[0]), Fd::new(pipe[1]));

        let mut msg: ArrayVec<u8, 64> = ArrayVec::new();
        wire::encode(&mut msg, 6, proto::wl_keyboard::EV_KEYMAP, &[Arg::Uint(1)]).expect("encode");
        let sent = syscall::send_with_fds(peer.as_raw_fd(), &msg, &[read_end.as_raw_fd()]);
        assert_eq!(sent as usize, msg.len());

        assert!(conn.take_fd().is_none(), "queue starts empty");
        conn.fill().expect("fill");
        let received = conn.take_fd().expect("the fd came through");
        assert!(conn.take_fd().is_none(), "exactly one fd was passed");

        // It is a live duplicate of the pipe, not just any number: a byte
        // written to the original write end reads out of the received fd.
        assert!(syscall::write_fd(write_end.as_raw_fd(), b"k") > 0);
        let mut got = [0u8; 1];
        assert_eq!(syscall::read_fd(received.as_raw_fd(), &mut got), 1);
        assert_eq!(&got, b"k");
    }

    #[test]
    fn a_flush_finishes_a_write_the_socket_could_not_take_at_once() {
        // On a non-blocking socket a write can take some bytes and then say
        // EAGAIN. Reporting that as an error would strand the tail of a half-sent
        // request, and the next request would be appended straight onto the
        // fragment, which the compositor reads as a corrupt stream. So the flush
        // has to wait for the socket and finish what it started.
        let (mut conn, peer) = connected_pair();
        conn.set_nonblocking(true).expect("nonblocking");

        // Stuff the socket until the kernel will not take another byte, so the
        // flush below is guaranteed to meet EAGAIN rather than merely risk it.
        let stuffing = [b'.'; 4096];
        let mut prefill = 0usize;
        loop {
            let n = syscall::send_with_fds(conn.fd, &stuffing, &[]);
            if n < 0 {
                assert_eq!(-n as i32, EAGAIN, "the socket should be full, not broken");
                break;
            }
            prefill += n as usize;
        }
        assert!(prefill > 0, "the socket took nothing at all");

        // Now queue a real request. Its first send cannot make any progress.
        conn.request(7, proto::wl_surface::COMMIT, &[])
            .expect("queue");
        let request_len = conn.out.len();

        // The peer starts reading, which is what eventually makes the socket
        // writable again. flush has to wait for that instead of giving up.
        let peer_fd = peer.as_raw_fd();
        let expected = prefill + request_len;
        let reader = std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut chunk = [0u8; 4096];
            while seen.len() < expected {
                let n = syscall::read_fd(peer_fd, &mut chunk);
                if n <= 0 {
                    break;
                }
                seen.extend_from_slice(&chunk[..n as usize]);
            }
            seen
        });

        conn.flush()
            .expect("flush completes across the would-block");
        assert!(conn.out.is_empty());

        let seen = reader.join().expect("reader");
        assert_eq!(seen.len(), expected, "every byte arrived");

        // The request is whole and sits after the stuffing, not spliced into it.
        let tail = &seen[prefill..];
        let (object, opcode, size) = wire::parse_header(tail).expect("header");
        assert_eq!((object, opcode, size), (7, proto::wl_surface::COMMIT, 8));
        assert_eq!(size, tail.len());

        core::mem::forget(peer); // the reader thread owns the fd now
    }
}
