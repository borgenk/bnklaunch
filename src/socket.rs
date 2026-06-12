//! Unix socket connection with SCM_RIGHTS file descriptor passing.
//!
//! Wayland uses Unix domain sockets for IPC, with file descriptors passed
//! out-of-band using the SCM_RIGHTS control message mechanism.
//! x86_64 Linux only.

use crate::arena::{ArrayString, ArrayVec};
use crate::env;
use crate::error::{Error, Result};
use crate::wire::{self, HEADER_SIZE, MAX_MESSAGE_SIZE};

use crate::syscall::{
    self, c_void, cmsg_data as CMSG_DATA, cmsg_firsthdr as CMSG_FIRSTHDR, cmsg_len as CMSG_LEN,
    cmsg_nxthdr as CMSG_NXTHDR, cmsg_space as CMSG_SPACE, cmsghdr, iovec, msghdr, recvmsg, sendmsg,
    sockaddr_un, Fd, RawFd, AF_UNIX, EINTR, F_GETFL, F_SETFL, MSG_CTRUNC, O_NONBLOCK, SCM_RIGHTS,
    SOCK_CLOEXEC, SOCK_STREAM, SOL_SOCKET,
};

/// Incoming-data buffer capacity: a full message plus headroom for the next
/// partial read.
const RECV_CAP: usize = 2 * MAX_MESSAGE_SIZE;
/// Most received fds awaiting consumption at once.
const MAX_RECV_FDS: usize = 16;
/// Payload capacity of one received message. Events the launcher reads are
/// small (the keymap arrives via an fd, not inline).
pub const MSG_PAYLOAD_CAP: usize = 16 * 1024;
/// Control-message buffer size, large enough for the fds we pass either way.
const CMSG_BUF: usize = 256;

/// A Wayland socket connection.
pub struct WaylandSocket {
    fd: RawFd,
    /// Buffer for incoming data
    recv_buf: ArrayVec<u8, RECV_CAP>,
    /// Received fds awaiting consumption, in arrival order. The wire protocol
    /// passes fds out of band but in the same order as the messages whose
    /// arguments declare them, so a message that carries an fd takes the front
    /// of this queue. Byte position cannot identify the owning message: one
    /// recvmsg may carry several messages and the kernel reports the fds against
    /// the chunk, not the message.
    recv_fds: ArrayVec<Fd, MAX_RECV_FDS>,
}

impl WaylandSocket {
    /// Connect to the Wayland compositor.
    pub fn connect() -> Result<Self> {
        let runtime_dir =
            env::var("XDG_RUNTIME_DIR").ok_or_else(|| Error::msg("XDG_RUNTIME_DIR not set"))?;
        let display = env::var("WAYLAND_DISPLAY").unwrap_or("wayland-0");

        let mut path: ArrayString<{ crate::fs::PATH_CAP }> = ArrayString::new();
        if display.starts_with('/') {
            let _ = path.push_str(display);
        } else {
            let _ = path.push_str(runtime_dir);
            let _ = path.push('/');
            let _ = path.push_str(display);
        }

        Self::connect_to(&path)
    }

    /// Connect to a specific socket path.
    pub fn connect_to(path: &str) -> Result<Self> {
        let bytes = path.as_bytes();
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
            // SAFETY: fd is the socket we opened above, closed once on this error
            // path before the owning struct exists.
            unsafe { syscall::close(fd) };
            return Err(Error::from_errno(-r));
        }

        Ok(Self {
            fd,
            recv_buf: ArrayVec::new(),
            recv_fds: ArrayVec::new(),
        })
    }

    /// Send a message, optionally with file descriptors.
    pub fn send(&mut self, data: &[u8], fds: &[RawFd]) -> Result<()> {
        if fds.is_empty() {
            self.write_all(data)?;
        } else {
            self.send_with_fds(data, fds)?;
        }
        Ok(())
    }

    /// Write a buffer in full, retrying short writes and interrupts.
    fn write_all(&self, data: &[u8]) -> Result<()> {
        let mut off = 0;
        while off < data.len() {
            let r = syscall::write_fd(self.fd, &data[off..]);
            if r < 0 {
                let e = -r as i32;
                if e == EINTR {
                    continue;
                }
                return Err(Error::from_errno(e));
            }
            off += r as usize;
        }
        Ok(())
    }

    /// Send data with file descriptors using SCM_RIGHTS.
    fn send_with_fds(&mut self, data: &[u8], fds: &[RawFd]) -> Result<()> {
        let mut iov = iovec {
            iov_base: data.as_ptr() as *mut c_void,
            iov_len: data.len(),
        };

        // Calculate control message size
        let cmsg_size = CMSG_SPACE(core::mem::size_of_val(fds));

        let mut cmsg_buf = [0u8; CMSG_BUF];

        let mut msg: msghdr = unsafe { core::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut c_void;
        msg.msg_controllen = cmsg_size;

        // Fill in the control message header
        let cmsg: *mut cmsghdr = unsafe { CMSG_FIRSTHDR(&msg) };
        if !cmsg.is_null() {
            unsafe {
                (*cmsg).cmsg_level = SOL_SOCKET;
                (*cmsg).cmsg_type = SCM_RIGHTS;

                (*cmsg).cmsg_len = CMSG_LEN(core::mem::size_of_val(fds));

                let fd_ptr = CMSG_DATA(cmsg) as *mut RawFd;
                for (i, &fd) in fds.iter().enumerate() {
                    core::ptr::write(fd_ptr.add(i), fd);
                }
            }
        }

        let result = loop {
            let r = unsafe { sendmsg(self.fd, &msg, 0) };
            if r == -(EINTR as isize) {
                continue;
            }
            break r;
        };
        if result < 0 {
            return Err(Error::from_errno(-result as i32));
        }

        // The fds ride with the first byte; send any unsent remainder as
        // plain data.
        let sent = result as usize;
        if sent < data.len() {
            self.write_all(&data[sent..])?;
        }

        Ok(())
    }

    /// Receive and buffer data from the socket.
    fn recv_data(&mut self) -> Result<usize> {
        let mut buf = [0u8; MAX_MESSAGE_SIZE];
        let mut iov = iovec {
            iov_base: buf.as_mut_ptr() as *mut c_void,
            iov_len: buf.len(),
        };

        // Allocate space for control messages (up to 10 file descriptors)
        let cmsg_size = CMSG_SPACE(10 * core::mem::size_of::<RawFd>());

        let mut cmsg_buf = [0u8; CMSG_BUF];

        let mut msg: msghdr = unsafe { core::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut c_void;
        msg.msg_controllen = cmsg_size;

        let n = loop {
            // recvmsg overwrites these; reset them so each retry offers the
            // full ancillary buffer.
            msg.msg_controllen = cmsg_size;
            msg.msg_flags = 0;
            let r = unsafe { recvmsg(self.fd, &mut msg, 0) };
            if r == -(EINTR as isize) {
                continue;
            }
            break r;
        };
        if n < 0 {
            return Err(Error::from_errno(-n as i32));
        }
        if n == 0 {
            return Err(Error::msg("connection closed"));
        }
        if msg.msg_flags & MSG_CTRUNC != 0 {
            return Err(Error::msg(
                "received message truncated its passed file descriptors",
            ));
        }

        // Append any passed fds to the queue in arrival order; they are matched
        // to messages by that order, not by byte position.
        let mut cmsg = unsafe { CMSG_FIRSTHDR(&msg) };
        while !cmsg.is_null() {
            unsafe {
                if (*cmsg).cmsg_level == SOL_SOCKET && (*cmsg).cmsg_type == SCM_RIGHTS {
                    let fd_ptr = CMSG_DATA(cmsg) as *const RawFd;
                    let fd_count = ((*cmsg).cmsg_len - core::mem::size_of::<cmsghdr>())
                        / core::mem::size_of::<RawFd>();
                    for i in 0..fd_count {
                        let fd = core::ptr::read(fd_ptr.add(i));
                        let _ = self.recv_fds.push(Fd::new(fd));
                    }
                }
                cmsg = CMSG_NXTHDR(&msg, cmsg);
            }
        }

        if self.recv_buf.extend_from_slice(&buf[..n as usize]).is_err() {
            return Err(Error::msg("receive buffer overflow"));
        }
        Ok(n as usize)
    }

    /// Read the next complete message.
    /// Returns (object_id, opcode, payload, fds).
    pub fn read_message(&mut self) -> Result<Message> {
        // Ensure we have at least a header
        while self.recv_buf.len() < HEADER_SIZE {
            self.recv_data()?;
        }

        // Parse header to get message size
        let (object_id, opcode, size) = wire::parse_header(&self.recv_buf)?;

        // Ensure we have the complete message
        while self.recv_buf.len() < size {
            self.recv_data()?;
        }

        // Copy out the payload (everything after the header), then drop the
        // message's bytes from the front of the buffer by shifting the rest down.
        let mut payload: ArrayVec<u8, MSG_PAYLOAD_CAP> = ArrayVec::new();
        if payload
            .extend_from_slice(&self.recv_buf[HEADER_SIZE..size])
            .is_err()
        {
            return Err(Error::msg("message payload too large"));
        }
        let remaining = self.recv_buf.len() - size;
        self.recv_buf.copy_within(size.., 0);
        self.recv_buf.truncate(remaining);

        Ok(Message {
            object_id,
            opcode,
            payload,
        })
    }

    /// Take the next received fd in arrival order, for a message whose argument
    /// declares one (a keyboard keymap, a clipboard write pipe). Returns None
    /// when none is queued.
    pub fn take_fd(&mut self) -> Option<Fd> {
        self.recv_fds.remove(0)
    }

    /// Flush any buffered writes. Writes go straight to the socket fd, so there
    /// is nothing buffered; this exists for call-site symmetry.
    pub fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    /// Set socket to non-blocking mode by toggling O_NONBLOCK on the fd.
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

    /// The underlying socket fd, for registering the connection with an event
    /// loop (an io_uring poll).
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for WaylandSocket {
    fn drop(&mut self) {
        // SAFETY: self.fd is the socket opened in connect_to, closed once here.
        // Any unconsumed received fds close themselves as their Fd values drop.
        unsafe { syscall::close(self.fd) };
    }
}

/// A received Wayland message. Passed fds are not carried here; they are taken
/// from the socket's arrival-order queue by the handler that needs one.
#[derive(Debug)]
pub struct Message {
    pub object_id: u32,
    pub opcode: u16,
    pub payload: ArrayVec<u8, MSG_PAYLOAD_CAP>,
}

impl Message {
    /// Create a parser for this message's payload.
    pub fn parser(&self) -> wire::MessageParser<'_> {
        wire::MessageParser::new(&self.payload)
    }
}
