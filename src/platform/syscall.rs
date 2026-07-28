//! Raw Linux syscall wrappers.
//!
//! Direct syscall access for x86_64 Linux.

#![allow(dead_code)]
#![allow(non_camel_case_types)]

use crate::platform::arena::ArrayVec;
use crate::platform::error::{Error, Result};

/// A raw file descriptor.
pub type RawFd = i32;

/// An owned file descriptor, closed when dropped. Stands in for std's OwnedFd
/// so received fds (a keymap, a clipboard pipe) free themselves when their
/// holder goes away.
#[derive(Debug)]
pub struct Fd(RawFd);

impl Fd {
    pub fn new(fd: RawFd) -> Self {
        Fd(fd)
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: self owns this fd and closes it exactly once, here.
        unsafe { close(self.0) };
    }
}

/// Longest path the kernel path arguments handle, including the trailing NUL.
pub const PATH_CAP: usize = 1024;

/// A path held as a valid C string: the bytes a kernel path argument reads,
/// with exactly one trailing NUL and no interior NUL. Building one checks that
/// invariant once, so the path syscalls that take a CPath read it as a C string
/// with no further obligation on the caller, which is what lets those wrappers
/// be safe.
pub struct CPath {
    buf: [u8; PATH_CAP],
}

impl CPath {
    /// Build a CPath from a string, or None if it holds an interior NUL or does
    /// not fit with room for the terminator.
    pub fn new(path: &str) -> Option<CPath> {
        let bytes = path.as_bytes();
        if bytes.len() + 1 > PATH_CAP || bytes.contains(&0) {
            return None;
        }
        let mut buf = [0u8; PATH_CAP];
        buf[..bytes.len()].copy_from_slice(bytes);
        // The tail stays zero, so buf[bytes.len()] terminates the string.
        Some(CPath { buf })
    }

    fn as_ptr(&self) -> *const u8 {
        self.buf.as_ptr()
    }
}

// Syscall numbers for x86_64 Linux
mod nr {
    pub const READ: usize = 0;
    pub const WRITE: usize = 1;
    pub const CLOSE: usize = 3;
    pub const LSEEK: usize = 8;
    pub const POLL: usize = 7;
    pub const MMAP: usize = 9;
    pub const MUNMAP: usize = 11;
    pub const DUP2: usize = 33;
    pub const SOCKET: usize = 41;
    pub const CONNECT: usize = 42;
    pub const FORK: usize = 57;
    pub const SETPGID: usize = 109;
    pub const EXIT_GROUP: usize = 231;
    pub const SENDMSG: usize = 46;
    pub const RECVMSG: usize = 47;
    pub const FCNTL: usize = 72;
    pub const FLOCK: usize = 73;
    pub const FTRUNCATE: usize = 77;
    pub const RENAME: usize = 82;
    pub const MKDIR: usize = 83;
    pub const GETDENTS64: usize = 217;
    pub const NEWFSTATAT: usize = 262;
    pub const CLOCK_GETTIME: usize = 228;
    pub const OPENAT: usize = 257;
    pub const MEMFD_CREATE: usize = 319;
    pub const IO_URING_SETUP: usize = 425;
    pub const IO_URING_ENTER: usize = 426;
    pub const RT_SIGACTION: usize = 13;
    pub const PIPE2: usize = 293;
    pub const SOCKETPAIR: usize = 53;
}

// Constants
pub const O_CLOEXEC: i32 = 0o2000000;
pub const PROT_READ: i32 = 0x1;
pub const PROT_WRITE: i32 = 0x2;
pub const MAP_SHARED: i32 = 0x01;
pub const MAP_PRIVATE: i32 = 0x02;
/// Pre-fault the whole mapping at mmap time so the io_uring rings never take a
/// page fault on the hot submit/complete path.
pub const MAP_POPULATE: i32 = 0x8000;
pub const MFD_CLOEXEC: u32 = 0x0001;
pub const SOL_SOCKET: i32 = 1;
pub const SCM_RIGHTS: i32 = 0x01;
/// msg_flags bit set by recvmsg when the ancillary buffer was too small to
/// hold the passed fds.
pub const MSG_CTRUNC: i32 = 0x8;
/// sendmsg flag: report a write to a hung-up peer as EPIPE instead of raising
/// SIGPIPE, whose default action would kill the process before any error path
/// in this program runs.
pub const MSG_NOSIGNAL: i32 = 0x4000;

/// errno for a syscall interrupted by a signal; retry it.
pub const EINTR: i32 = 4;

/// errno returned by a non-blocking read with no data available; retry it.
pub const EAGAIN: i32 = 11;

/// errno for a write whose reader has hung up. Reachable only because SIGPIPE
/// is ignored; its default action would have killed the process first.
pub const EPIPE: i32 = 32;

/// errno returned by mkdir when the directory already exists; not an error for
/// a make-parents walk.
pub const EEXIST: i32 = 17;

/// errno for an argument the kernel rejects.
pub const EINVAL: i32 = 22;

/// Control-message buffer size, large enough for the fds passed either way.
const CMSG_BUF: usize = 256;

/// poll event bits.
pub const POLLIN: i16 = 0x001;
pub const POLLOUT: i16 = 0x004;

// flock operations
pub const LOCK_EX: i32 = 2; // Exclusive lock
pub const LOCK_NB: i32 = 4; // Non-blocking (OR with LOCK_EX)

/// Resolve a relative openat path against the current working directory.
pub const AT_FDCWD: i32 = -100;

/// newfstatat flag: stat the symlink itself rather than its target, so a
/// symlinked directory is not followed during the recursive scan.
pub const AT_SYMLINK_NOFOLLOW: i32 = 0x100;

// open flags
pub const O_RDONLY: i32 = 0;
pub const O_WRONLY: i32 = 1;
pub const O_RDWR: i32 = 2;
pub const O_CREAT: i32 = 0o100;
pub const O_TRUNC: i32 = 0o1000;
pub const O_DIRECTORY: i32 = 0o200000;
pub const O_NONBLOCK: i32 = 0o4000;

/// lseek whence: from the start of the file.
pub const SEEK_SET: i32 = 0;

/// Monotonic clock id for clock_gettime; unaffected by wall-clock jumps, so it
/// is the right source for blink and key-repeat deadlines.
pub const CLOCK_MONOTONIC: i32 = 1;

// socket address family and type for the Wayland connection.
pub const AF_UNIX: i32 = 1;
pub const SOCK_STREAM: i32 = 1;

/// socket type flag: close the socket on exec, so launched apps do not inherit
/// the compositor connection. Shares O_CLOEXEC's bit value.
pub const SOCK_CLOEXEC: i32 = O_CLOEXEC;

// fcntl commands for toggling non-blocking mode on the socket.
pub const F_GETFL: i32 = 3;
pub const F_SETFL: i32 = 4;

/// fcntl command for the descriptor flags, and the only flag there. io_uring
/// hands back a descriptor that openat's O_CLOEXEC cannot reach, so its
/// close-on-exec is set after the fact.
pub const F_SETFD: i32 = 2;
pub const FD_CLOEXEC: i32 = 1;

/// A Unix-domain socket address. sun_path holds the filesystem path, and the
/// passed addrlen covers only the family plus the used path bytes and its NUL.
#[repr(C)]
pub struct sockaddr_un {
    pub sun_family: u16,
    pub sun_path: [u8; 108],
}

// io_uring mmap region offsets passed to mmap as the file offset.
pub const IORING_OFF_SQ_RING: i64 = 0;
pub const IORING_OFF_CQ_RING: i64 = 0x0800_0000;
pub const IORING_OFF_SQES: i64 = 0x1000_0000;

/// io_uring_enter flag: also wait for and reap completions, not just submit.
pub const IORING_ENTER_GETEVENTS: u32 = 1;

/// Feature bit reported by io_uring_setup when the submission and completion
/// rings share one mapping, so only the SQE array needs a second mmap.
pub const IORING_FEAT_SINGLE_MMAP: u32 = 1;

/// Completion flag: more completions will follow for this submission, so a
/// multishot op stays armed. When it is clear, the op ended and must be
/// re-armed to keep firing.
pub const IORING_CQE_F_MORE: u32 = 1 << 1;

// io_uring operation codes used here.
pub const IORING_OP_NOP: u8 = 0;
pub const IORING_OP_POLL_ADD: u8 = 6;
pub const IORING_OP_TIMEOUT: u8 = 11;
pub const IORING_OP_OPENAT: u8 = 18;
pub const IORING_OP_CLOSE: u8 = 19;
pub const IORING_OP_READ: u8 = 22;

/// poll_add len bit requesting a multishot poll: the ring keeps reporting
/// readiness until the poll is cancelled, instead of firing once.
pub const IORING_POLL_ADD_MULTI: u32 = 1;

/// timeout flag requesting a recurring timer that re-arms after each fire.
pub const IORING_TIMEOUT_MULTISHOT: u32 = 1 << 6;

/// sqe flag: this entry links to the next, which only starts once this one
/// completes successfully. Used to chain openat then read then close.
pub const IOSQE_IO_LINK: u8 = 1 << 2;

/// C void type alias
pub type c_void = core::ffi::c_void;

/// off_t type for file offsets
pub type off_t = i64;

/// iovec structure for scatter/gather I/O
#[repr(C)]
#[derive(Clone, Copy)]
pub struct iovec {
    pub iov_base: *mut c_void,
    pub iov_len: usize,
}

/// msghdr structure for sendmsg/recvmsg
#[repr(C)]
pub struct msghdr {
    pub msg_name: *mut c_void,
    pub msg_namelen: u32,
    pub msg_iov: *mut iovec,
    pub msg_iovlen: usize,
    pub msg_control: *mut c_void,
    pub msg_controllen: usize,
    pub msg_flags: i32,
}

/// cmsghdr structure for control messages
#[repr(C)]
pub struct cmsghdr {
    pub cmsg_len: usize,
    pub cmsg_level: i32,
    pub cmsg_type: i32,
    // Data follows
}

/// pollfd structure for poll.
#[repr(C)]
pub struct pollfd {
    pub fd: RawFd,
    pub events: i16,
    pub revents: i16,
}

/// Time value for clock_gettime and io_uring timeouts.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct kernel_timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

/// Fixed head of a getdents64 record. The variable-length name follows the
/// struct in the buffer, NUL-terminated, and d_reclen is the stride to the
/// next record.
#[repr(C)]
pub struct linux_dirent64 {
    pub d_ino: u64,
    pub d_off: i64,
    pub d_reclen: u16,
    pub d_type: u8,
    // d_name: [u8; _] follows, NUL-terminated
}

/// A timespec embedded in struct stat.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct stat_timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

/// The x86_64 struct stat filled by newfstatat. Only st_mode (for the file
/// kind) and st_mtime (for the cache fingerprint) are read; the rest is laid
/// out to match the kernel ABI.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub __pad0: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atime: stat_timespec,
    pub st_mtime: stat_timespec,
    pub st_ctime: stat_timespec,
    pub __unused: [i64; 3],
}

/// File-type mask and directory bit within st_mode.
pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;

/// d_type value marking a regular file in a getdents64 record.
pub const DT_REG: u8 = 8;
/// d_type value marking a directory in a getdents64 record.
pub const DT_DIR: u8 = 4;
/// d_type value marking a symbolic link in a getdents64 record.
pub const DT_LNK: u8 = 10;
/// d_type value meaning the filesystem did not report the kind; the caller
/// must stat to find out.
pub const DT_UNKNOWN: u8 = 0;

/// Ring offsets returned by io_uring_setup, each a byte offset into the SQ
/// mmap at which that u32 field lives.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct io_sqring_offsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub flags: u32,
    pub dropped: u32,
    pub array: u32,
    pub resv1: u32,
    pub resv2: u64,
}

/// Ring offsets returned by io_uring_setup for the completion mmap.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct io_cqring_offsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub overflow: u32,
    pub cqes: u32,
    pub flags: u32,
    pub resv1: u32,
    pub resv2: u64,
}

/// Parameters and negotiated layout for io_uring_setup. The kernel fills the
/// feature bits and ring offsets on return.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct io_uring_params {
    pub sq_entries: u32,
    pub cq_entries: u32,
    pub flags: u32,
    pub sq_thread_cpu: u32,
    pub sq_thread_idle: u32,
    pub features: u32,
    pub wq_fd: u32,
    pub resv: [u32; 3],
    pub sq_off: io_sqring_offsets,
    pub cq_off: io_cqring_offsets,
}

/// A submission queue entry: one queued operation. The unions of the kernel
/// struct are flattened to their 64-byte layout; off, addr, and op_flags carry
/// different meanings per opcode.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct io_uring_sqe {
    pub opcode: u8,
    pub flags: u8,
    pub ioprio: u16,
    pub fd: i32,
    pub off: u64,
    pub addr: u64,
    pub len: u32,
    pub op_flags: u32,
    pub user_data: u64,
    pub buf_index: u16,
    pub personality: u16,
    pub splice_fd_in: i32,
    pub addr3: u64,
    pub __pad2: u64,
}

/// A completion queue entry: the result of one submitted operation. user_data
/// echoes the value set on the matching sqe; res is the operation's return
/// value (negative is a negated errno).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct io_uring_cqe {
    pub user_data: u64,
    pub res: i32,
    pub flags: u32,
}

// The kernel reads and writes these structs by their fixed ABI layout, so any
// drift in size is a memory-safety bug, not just a logic error. Pin the sizes
// at compile time.
const _: () = assert!(core::mem::size_of::<io_uring_sqe>() == 64);
const _: () = assert!(core::mem::size_of::<io_uring_cqe>() == 16);
const _: () = assert!(core::mem::size_of::<io_sqring_offsets>() == 40);
const _: () = assert!(core::mem::size_of::<io_cqring_offsets>() == 40);
const _: () = assert!(core::mem::size_of::<io_uring_params>() == 120);
const _: () = assert!(core::mem::size_of::<kernel_timespec>() == 16);

/// Perform a raw syscall with 0 arguments
#[inline(always)]
unsafe fn syscall0(nr: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        in("rax") nr,
        lateout("rax") ret,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack, preserves_flags)
    );
    ret
}

/// Perform a raw syscall with 1 argument
#[inline(always)]
unsafe fn syscall1(nr: usize, a1: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        in("rax") nr,
        in("rdi") a1,
        lateout("rax") ret,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack, preserves_flags)
    );
    ret
}

/// Perform a raw syscall with 2 arguments
#[inline(always)]
unsafe fn syscall2(nr: usize, a1: usize, a2: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        in("rax") nr,
        in("rdi") a1,
        in("rsi") a2,
        lateout("rax") ret,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack, preserves_flags)
    );
    ret
}

/// Perform a raw syscall with 3 arguments
#[inline(always)]
unsafe fn syscall3(nr: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        in("rax") nr,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        lateout("rax") ret,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack, preserves_flags)
    );
    ret
}

/// Perform a raw syscall with 4 arguments
#[inline(always)]
unsafe fn syscall4(nr: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        in("rax") nr,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r10") a4,
        lateout("rax") ret,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack, preserves_flags)
    );
    ret
}

/// Perform a raw syscall with 5 arguments
#[inline(always)]
unsafe fn syscall5(nr: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        in("rax") nr,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r10") a4,
        in("r8") a5,
        lateout("rax") ret,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack, preserves_flags)
    );
    ret
}

/// Perform a raw syscall with 6 arguments
#[inline(always)]
unsafe fn syscall6(
    nr: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        in("rax") nr,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r10") a4,
        in("r8") a5,
        in("r9") a6,
        lateout("rax") ret,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack, preserves_flags)
    );
    ret
}

/// Create an anonymous file with the given name. Returns a new fd or a negated
/// errno.
pub fn memfd_create(name: &CPath, flags: u32) -> RawFd {
    // SAFETY: CPath is a valid C string the kernel only reads.
    unsafe { syscall2(nr::MEMFD_CREATE, name.as_ptr() as usize, flags as usize) as RawFd }
}

/// Truncate a file to a specified length.
pub fn ftruncate(fd: RawFd, length: off_t) -> i32 {
    // SAFETY: no pointer crosses the boundary.
    unsafe { syscall2(nr::FTRUNCATE, fd as usize, length as usize) as i32 }
}

/// Apply or remove an advisory lock on an open file.
pub fn flock(fd: RawFd, operation: i32) -> i32 {
    // SAFETY: no pointer crosses the boundary.
    unsafe { syscall2(nr::FLOCK, fd as usize, operation as usize) as i32 }
}

/// Map files or devices into memory.
pub unsafe fn mmap(
    addr: *mut c_void,
    length: usize,
    prot: i32,
    flags: i32,
    fd: RawFd,
    offset: off_t,
) -> *mut c_void {
    syscall6(
        nr::MMAP,
        addr as usize,
        length,
        prot as usize,
        flags as usize,
        fd as usize,
        offset as usize,
    ) as *mut c_void
}

/// Whether an mmap return value indicates failure.
///
/// The raw mmap syscall does not use the MAP_FAILED (-1) sentinel: it reports
/// errors by returning the negated errno in the range -4095 to -1. On x86_64
/// every valid user-space mapping lives in the low half of the address space,
/// so a pointer in that range is always an error.
#[inline]
pub fn mmap_failed(ptr: *mut c_void) -> bool {
    let v = ptr as isize;
    (-4095..0).contains(&v)
}

/// Extract the errno from a failed mmap return value.
///
/// Only meaningful when mmap_failed returned true. Raw syscalls do not set
/// errno, so the value has to be derived from the result.
#[inline]
pub fn mmap_errno(ptr: *mut c_void) -> i32 {
    -(ptr as isize as i32)
}

/// Unmap a mapped region.
pub unsafe fn munmap(addr: *mut c_void, length: usize) -> i32 {
    syscall2(nr::MUNMAP, addr as usize, length) as i32
}

/// A read-only mapping of a file, unmapped when dropped.
pub struct Mapped {
    ptr: *mut c_void,
    len: usize,
}

impl Mapped {
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr and len are the mapping made in read_mapped, which is
        // alive for as long as self is, and PROT_READ makes the bytes readable.
        // The borrow ties the slice to self, so it cannot outlive the unmap.
        unsafe { core::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: ptr and len are exactly what mmap returned and was given, and
        // Drop runs once, so the region is unmapped exactly once.
        unsafe { munmap(self.ptr, self.len) };
    }
}

/// Map len bytes of a file read-only.
///
/// The keyboard keymap arrives as a file descriptor whose seek position is
/// shared with the compositor's own file-table entry, and not every compositor
/// rewinds it before passing it on. A mapping sidesteps the offset entirely and
/// costs no copy.
pub fn read_mapped(fd: RawFd, len: usize) -> Result<Mapped> {
    if len == 0 {
        return Err(Error::msg("cannot map an empty file"));
    }
    // SAFETY: a null hint lets the kernel choose the address, and the result is
    // checked with mmap_failed before it is used or stored. The mapping is
    // private and read-only, so nothing else can be affected through it.
    let ptr = unsafe { mmap(core::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0) };
    if mmap_failed(ptr) {
        return Err(Error::from_errno(mmap_errno(ptr)));
    }
    Ok(Mapped { ptr, len })
}

/// Send a message on a socket.
pub unsafe fn sendmsg(sockfd: RawFd, msg: *const msghdr, flags: i32) -> isize {
    syscall3(nr::SENDMSG, sockfd as usize, msg as usize, flags as usize)
}

/// Receive a message from a socket.
pub unsafe fn recvmsg(sockfd: RawFd, msg: *mut msghdr, flags: i32) -> isize {
    syscall3(nr::RECVMSG, sockfd as usize, msg as usize, flags as usize)
}

/// The kernel's sigaction, which is not glibc's: the field order here (handler,
/// flags, restorer, mask) is what rt_sigaction reads on x86_64.
#[repr(C)]
struct kernel_sigaction {
    sa_handler: usize,
    sa_flags: u64,
    sa_restorer: usize,
    /// The kernel's sigset_t, 64 bits on x86_64.
    sa_mask: u64,
}

pub const SIGPIPE: i32 = 13;
/// Signal dispositions, as the integer values the kernel reads for them.
pub const SIG_DFL: usize = 0;
pub const SIG_IGN: usize = 1;

/// Set a signal's disposition to SIG_DFL or SIG_IGN. Returns 0 or a negated
/// errno.
///
/// Only these two dispositions are offered. Installing a real handler would
/// need a restorer trampoline (x86_64 rejects a handler without SA_RESTORER at
/// delivery time), and neither disposition here is ever delivered.
pub fn signal_disposition(signum: i32, disposition: usize) -> i32 {
    let act = kernel_sigaction {
        sa_handler: disposition,
        sa_flags: 0,
        sa_restorer: 0,
        sa_mask: 0,
    };
    // SAFETY: act is a fully initialized kernel_sigaction laid out as the kernel
    // expects and lives across the call; the old-action pointer is null, which
    // the kernel reads as "do not report the previous disposition". The last
    // argument is the size of the kernel's sigset_t, which the syscall requires.
    unsafe {
        syscall4(
            nr::RT_SIGACTION,
            signum as usize,
            &act as *const kernel_sigaction as usize,
            0,
            core::mem::size_of::<u64>(),
        ) as i32
    }
}

/// Create a pipe, writing the read and write fds into pipefd. Returns 0 or a
/// negated errno.
pub fn pipe2(pipefd: &mut [i32; 2], flags: i32) -> i32 {
    // SAFETY: pipefd is a valid writable array of two ints the kernel fills.
    unsafe { syscall2(nr::PIPE2, pipefd.as_mut_ptr() as usize, flags as usize) as i32 }
}

/// Create a socket. Returns a new fd or a negated errno.
pub fn socket(domain: i32, ty: i32, protocol: i32) -> RawFd {
    // SAFETY: no pointer crosses the boundary.
    unsafe { syscall3(nr::SOCKET, domain as usize, ty as usize, protocol as usize) as RawFd }
}

/// Create a connected pair of sockets, writing both fds into sv. Returns 0 or a
/// negated errno. Used by the connection tests to stand in for a compositor.
pub fn socketpair(domain: i32, ty: i32, protocol: i32, sv: &mut [i32; 2]) -> i32 {
    // SAFETY: sv is a valid writable array of two ints the kernel fills.
    unsafe {
        syscall4(
            nr::SOCKETPAIR,
            domain as usize,
            ty as usize,
            protocol as usize,
            sv.as_mut_ptr() as usize,
        ) as i32
    }
}

/// Connect a socket to an address. addrlen bounds the kernel's read of addr.
/// Returns 0 or a negated errno.
pub fn connect(fd: RawFd, addr: &sockaddr_un, addrlen: u32) -> i32 {
    // SAFETY: addr is a valid sockaddr_un and addrlen is at most its size, so
    // the kernel reads within it.
    unsafe {
        syscall3(
            nr::CONNECT,
            fd as usize,
            addr as *const sockaddr_un as usize,
            addrlen as usize,
        ) as i32
    }
}

/// Manipulate a file descriptor (here, to toggle non-blocking mode). Returns a
/// command-specific value or a negated errno.
pub fn fcntl(fd: RawFd, cmd: i32, arg: i32) -> i32 {
    // SAFETY: arg is an integer for the commands used here; no pointer crosses
    // the boundary.
    unsafe { syscall3(nr::FCNTL, fd as usize, cmd as usize, arg as usize) as i32 }
}

/// Fork the process. Returns the child pid in the parent, 0 in the child, or a
/// negated errno.
pub unsafe fn fork() -> i32 {
    syscall0(nr::FORK) as i32
}

/// Duplicate oldfd onto newfd, closing newfd first. Returns newfd or a negated
/// errno.
pub fn dup2(oldfd: RawFd, newfd: RawFd) -> i32 {
    // SAFETY: no pointer crosses the boundary.
    unsafe { syscall2(nr::DUP2, oldfd as usize, newfd as usize) as i32 }
}

/// Set the process group of pid (0 = self) to pgid (0 = make it a new group
/// leader). Returns 0 or a negated errno.
pub fn setpgid(pid: i32, pgid: i32) -> i32 {
    // SAFETY: no pointer crosses the boundary.
    unsafe { syscall2(nr::SETPGID, pid as usize, pgid as usize) as i32 }
}

/// Exit the whole process immediately with the given status. Never returns.
pub fn exit_group(status: i32) -> ! {
    // SAFETY: exit_group terminates the process, so the syscall never returns
    // and the following hint is never reached.
    unsafe {
        syscall1(nr::EXIT_GROUP, status as usize);
        core::hint::unreachable_unchecked()
    }
}

/// Close a file descriptor.
pub unsafe fn close(fd: RawFd) -> i32 {
    syscall1(nr::CLOSE, fd as usize) as i32
}

/// Write a buffer to a file descriptor. Returns the bytes written or a negated
/// errno.
pub fn write_fd(fd: RawFd, buf: &[u8]) -> isize {
    // SAFETY: buf describes a valid readable region; the kernel reads at most
    // buf.len() bytes.
    unsafe { syscall3(nr::WRITE, fd as usize, buf.as_ptr() as usize, buf.len()) }
}

/// Read from a file descriptor into buf. Returns the bytes read, 0 at end of
/// file, or a negated errno.
pub fn read_fd(fd: RawFd, buf: &mut [u8]) -> isize {
    // SAFETY: buf describes a valid writable region; the kernel writes at most
    // buf.len() bytes.
    unsafe { syscall3(nr::READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) }
}

/// Wait for an event on a set of file descriptors, with a millisecond timeout.
/// Returns the number ready, 0 on timeout, or a negated errno.
pub fn poll(fds: &mut [pollfd], timeout_ms: i32) -> isize {
    // SAFETY: fds describes a valid array the kernel reads and updates in place.
    unsafe {
        syscall3(
            nr::POLL,
            fds.as_mut_ptr() as usize,
            fds.len(),
            timeout_ms as isize as usize,
        )
    }
}

/// Open a path relative to dirfd. Returns a new fd or a negated errno.
pub fn openat(dirfd: RawFd, path: &CPath, flags: i32, mode: u32) -> RawFd {
    // SAFETY: CPath is a valid C string the kernel only reads.
    unsafe {
        syscall4(
            nr::OPENAT,
            dirfd as usize,
            path.as_ptr() as usize,
            flags as usize,
            mode as usize,
        ) as RawFd
    }
}

/// Read directory entries from an open directory fd into a buffer of
/// linux_dirent64 records. Returns the bytes written, 0 at end of directory,
/// or a negated errno.
pub fn getdents64(fd: RawFd, dirp: &mut [u8]) -> isize {
    // SAFETY: dirp describes a valid writable region the kernel fills with
    // dirent records, at most dirp.len() bytes.
    unsafe {
        syscall3(
            nr::GETDENTS64,
            fd as usize,
            dirp.as_mut_ptr() as usize,
            dirp.len(),
        )
    }
}

/// Read the given clock into a fresh timespec and return it. The return code is
/// not surfaced; CLOCK_MONOTONIC does not fail in practice.
pub fn clock_gettime(clk_id: i32) -> kernel_timespec {
    let mut ts = kernel_timespec::default();
    // SAFETY: &mut ts is a valid writable timespec the kernel fills.
    unsafe {
        syscall2(
            nr::CLOCK_GETTIME,
            clk_id as usize,
            &mut ts as *mut kernel_timespec as usize,
        )
    };
    ts
}

/// Reposition an open file's offset. Returns the new offset or a negated errno.
pub fn lseek(fd: RawFd, offset: i64, whence: i32) -> i64 {
    // SAFETY: no pointer crosses the boundary.
    unsafe { syscall3(nr::LSEEK, fd as usize, offset as usize, whence as usize) as i64 }
}

/// Rename a file. Returns 0 or a negated errno.
pub fn rename(oldpath: &CPath, newpath: &CPath) -> i32 {
    // SAFETY: both CPaths are valid C strings the kernel only reads.
    unsafe {
        syscall2(
            nr::RENAME,
            oldpath.as_ptr() as usize,
            newpath.as_ptr() as usize,
        ) as i32
    }
}

/// Create a directory. Returns 0 or a negated errno (EEXIST if it already
/// exists).
pub fn mkdir(path: &CPath, mode: u32) -> i32 {
    // SAFETY: CPath is a valid C string the kernel only reads.
    unsafe { syscall2(nr::MKDIR, path.as_ptr() as usize, mode as usize) as i32 }
}

/// Stat a path relative to dirfd, or None on error.
pub fn newfstatat(dirfd: RawFd, path: &CPath, flags: i32) -> Option<stat> {
    // SAFETY: stat is a plain integer struct, so zeroed is a valid value the
    // kernel then fills; CPath is a valid C string the kernel only reads.
    let mut st: stat = unsafe { core::mem::zeroed() };
    let r = unsafe {
        syscall4(
            nr::NEWFSTATAT,
            dirfd as usize,
            path.as_ptr() as usize,
            &mut st as *mut stat as usize,
            flags as usize,
        ) as i32
    };
    if r < 0 {
        None
    } else {
        Some(st)
    }
}

const _: () = assert!(core::mem::size_of::<stat>() == 144);

/// Set up an io_uring instance with room for at least entries submissions.
/// Returns the ring fd and fills params with the negotiated layout, or a
/// negated errno.
pub fn io_uring_setup(entries: u32, params: &mut io_uring_params) -> RawFd {
    // SAFETY: params is a valid writable io_uring_params the kernel fills.
    unsafe {
        syscall2(
            nr::IO_URING_SETUP,
            entries as usize,
            params as *mut io_uring_params as usize,
        ) as RawFd
    }
}

/// Submit queued entries and optionally wait for completions. Returns the
/// number consumed from the submission queue or a negated errno.
pub fn io_uring_enter(fd: RawFd, to_submit: u32, min_complete: u32, flags: u32) -> isize {
    // SAFETY: no pointer crosses the boundary; the signal mask is null.
    unsafe {
        syscall6(
            nr::IO_URING_ENTER,
            fd as usize,
            to_submit as usize,
            min_complete as usize,
            flags as usize,
            0, // sig: no signal mask
            0, // sigsz
        )
    }
}

// CMSG macros implemented as functions

/// Alignment for control message data
const CMSG_ALIGN_SIZE: usize = core::mem::size_of::<usize>();

/// Align a value up to CMSG_ALIGN_SIZE
#[inline]
const fn cmsg_align(len: usize) -> usize {
    (len + CMSG_ALIGN_SIZE - 1) & !(CMSG_ALIGN_SIZE - 1)
}

/// Calculate the size of a control message with the given data length.
/// Equivalent to CMSG_SPACE macro.
#[inline]
pub const fn cmsg_space(data_len: usize) -> usize {
    cmsg_align(core::mem::size_of::<cmsghdr>()) + cmsg_align(data_len)
}

/// Calculate the value for cmsg_len field.
/// Equivalent to CMSG_LEN macro.
#[inline]
pub const fn cmsg_len(data_len: usize) -> usize {
    cmsg_align(core::mem::size_of::<cmsghdr>()) + data_len
}

/// Get pointer to the first cmsghdr in a msghdr.
/// Equivalent to CMSG_FIRSTHDR macro.
#[inline]
pub unsafe fn cmsg_firsthdr(msg: *const msghdr) -> *mut cmsghdr {
    if (*msg).msg_controllen >= core::mem::size_of::<cmsghdr>() {
        (*msg).msg_control as *mut cmsghdr
    } else {
        core::ptr::null_mut()
    }
}

/// Get pointer to the next cmsghdr.
/// Equivalent to CMSG_NXTHDR macro.
///
/// The bounds test is done on integers, not pointers. Forming the one-past
/// address of a candidate header and comparing it is undefined behaviour when
/// that address lands outside the control buffer, which is exactly the case this
/// has to detect.
#[inline]
pub unsafe fn cmsg_nxthdr(msg: *const msghdr, cmsg: *const cmsghdr) -> *mut cmsghdr {
    let header = core::mem::size_of::<cmsghdr>();
    // A record shorter than its own header is malformed; the kernel does not
    // produce one, and trusting it would run the walk off into the buffer.
    if (*cmsg).cmsg_len < header {
        return core::ptr::null_mut();
    }

    let base = (*msg).msg_control as usize;
    let len = (*msg).msg_controllen;
    let offset = (cmsg as usize) - base + cmsg_align((*cmsg).cmsg_len);

    // The next header must fit whole inside the control buffer.
    if offset.saturating_add(header) > len {
        return core::ptr::null_mut();
    }
    (base + offset) as *mut cmsghdr
}

/// Send data with file descriptors attached as SCM_RIGHTS ancillary data.
/// Returns the bytes sent, or a negated errno.
///
/// The fds ride with the first byte of the message, so a short send leaves the
/// caller to write the remainder as plain bytes.
pub fn send_with_fds(fd: RawFd, data: &[u8], fds: &[RawFd]) -> isize {
    let mut iov = iovec {
        iov_base: data.as_ptr() as *mut c_void,
        iov_len: data.len(),
    };
    let mut cmsg_buf = [0u8; CMSG_BUF];
    let control_len = cmsg_space(core::mem::size_of_val(fds));

    // SAFETY: msg is zeroed, then given one iovec covering data and a control
    // buffer of control_len bytes, both live for the call. control_len is what
    // cmsg_space computes for these fds and cmsg_buf is larger (checked below),
    // so the header and the fd array written through the cmsg pointers stay
    // inside cmsg_buf. MSG_NOSIGNAL keeps a hung-up peer from raising SIGPIPE.
    unsafe {
        if control_len > cmsg_buf.len() {
            return -(EINVAL as isize);
        }
        let mut msg: msghdr = core::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut c_void;
        msg.msg_controllen = control_len;

        let cmsg = cmsg_firsthdr(&msg);
        if !cmsg.is_null() {
            (*cmsg).cmsg_level = SOL_SOCKET;
            (*cmsg).cmsg_type = SCM_RIGHTS;
            (*cmsg).cmsg_len = cmsg_len(core::mem::size_of_val(fds));
            let fd_ptr = cmsg_data(cmsg) as *mut RawFd;
            for (i, &fd) in fds.iter().enumerate() {
                core::ptr::write(fd_ptr.add(i), fd);
            }
        }
        sendmsg(fd, &msg, MSG_NOSIGNAL)
    }
}

/// Receive into buf, appending any file descriptors passed as SCM_RIGHTS to fds
/// in arrival order. Returns the bytes read (0 means the peer closed), or a
/// negated errno.
///
/// Err when the kernel had to truncate the ancillary data, or when more fds
/// arrive than the queue holds. Both are fatal rather than skippable: fds are
/// matched to messages by arrival order, so one dropped fd misaligns every fd
/// after it.
pub fn recv_with_fds<const N: usize>(
    fd: RawFd,
    buf: &mut [u8],
    fds: &mut ArrayVec<Fd, N>,
) -> Result<isize> {
    let mut iov = iovec {
        iov_base: buf.as_mut_ptr() as *mut c_void,
        iov_len: buf.len(),
    };
    let mut cmsg_buf = [0u8; CMSG_BUF];

    // SAFETY: msg is zeroed, then given one iovec covering buf and the whole of
    // cmsg_buf as its control buffer, both live for the call. The kernel writes
    // no more than msg_controllen bytes of ancillary data, and the cmsg walk
    // below stays inside what it reports back.
    unsafe {
        let mut msg: msghdr = core::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut c_void;

        let n = loop {
            // recvmsg overwrites these, so reset them per attempt: each retry
            // has to offer the full ancillary buffer.
            msg.msg_controllen = cmsg_buf.len();
            msg.msg_flags = 0;
            let r = recvmsg(fd, &mut msg, 0);
            if r == -(EINTR as isize) {
                continue;
            }
            break r;
        };
        if n < 0 {
            return Ok(n);
        }
        if msg.msg_flags & MSG_CTRUNC != 0 {
            return Err(Error::msg(
                "received message truncated its passed file descriptors",
            ));
        }

        let mut cmsg = cmsg_firsthdr(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == SOL_SOCKET && (*cmsg).cmsg_type == SCM_RIGHTS {
                let header = cmsg_align(core::mem::size_of::<cmsghdr>());
                let data_len = (*cmsg).cmsg_len.saturating_sub(header);
                let count = data_len / core::mem::size_of::<RawFd>();
                let fd_ptr = cmsg_data(cmsg) as *const RawFd;
                for i in 0..count {
                    let passed = Fd::new(core::ptr::read(fd_ptr.add(i)));
                    if fds.push(passed).is_err() {
                        return Err(Error::msg(
                            "received more file descriptors than the queue holds",
                        ));
                    }
                }
            }
            cmsg = cmsg_nxthdr(&msg, cmsg);
        }
        Ok(n)
    }
}

/// Get pointer to the data portion of a cmsghdr.
/// Equivalent to CMSG_DATA macro.
#[inline]
pub unsafe fn cmsg_data(cmsg: *const cmsghdr) -> *mut u8 {
    (cmsg as *mut u8).add(cmsg_align(core::mem::size_of::<cmsghdr>()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignoring_sigpipe_turns_a_broken_pipe_write_into_epipe() {
        // The kernel takes the struct as laid out here for both dispositions.
        // SIG_DFL is what the forked child restores before exec, so it has to be
        // accepted too; it is set back to ignore immediately, since a default
        // SIGPIPE would kill this whole test process on the write below.
        assert_eq!(signal_disposition(SIGPIPE, SIG_DFL), 0);
        assert_eq!(signal_disposition(SIGPIPE, SIG_IGN), 0);

        let mut fds = [0i32; 2];
        assert_eq!(pipe2(&mut fds, O_CLOEXEC), 0);
        let (read_end, write_end) = (Fd::new(fds[0]), Fd::new(fds[1]));
        drop(read_end); // hang up the reader

        // Ignored, so the write reports the hangup instead of raising a signal.
        let n = write_fd(write_end.as_raw_fd(), b"data");
        assert_eq!(n, -(EPIPE as isize), "expected EPIPE, got {n}");
    }

    #[test]
    fn test_cmsg_space() {
        // CMSG_SPACE for one fd should be header + aligned fd size
        let space = cmsg_space(core::mem::size_of::<RawFd>());
        assert!(space >= core::mem::size_of::<cmsghdr>() + core::mem::size_of::<RawFd>());
    }

    #[test]
    fn test_cmsg_len() {
        let len = cmsg_len(core::mem::size_of::<RawFd>());
        assert!(len >= core::mem::size_of::<cmsghdr>() + core::mem::size_of::<RawFd>());
    }

    #[test]
    fn test_syscall_error_handling() {
        // Test that invalid syscall returns proper negative errno
        // Try to truncate an invalid fd (-1) - should return -EBADF (9)
        let result = ftruncate(-1, 100);
        assert!(result < 0, "ftruncate on invalid fd should return negative");
        assert_eq!(-result, 9, "should be EBADF (9)");

        // Verify we can create an io::Error from it
        let err = std::io::Error::from_raw_os_error(-result);
        assert_eq!(err.raw_os_error(), Some(9));
    }

    #[test]
    fn test_mmap_failed_detects_errno_range() {
        // The raw syscall reports failure as -errno in [-4095, -1].
        for errno in [1i32, 12, 22, 4095] {
            let ptr = (-(errno as isize)) as *mut c_void;
            assert!(mmap_failed(ptr), "errno {errno} should be a failure");
            assert_eq!(mmap_errno(ptr), errno);
        }
    }

    #[test]
    fn test_mmap_failed_accepts_valid_pointers() {
        // A plausible page-aligned user-space address is a success, not -errno.
        let ptr = 0x1000usize as *mut c_void;
        assert!(!mmap_failed(ptr));
        // -4096 is just outside the errno window and must not be misread.
        assert!(!mmap_failed((-4096isize) as *mut c_void));
    }

    #[test]
    fn test_memfd_create_success() {
        let name = CPath::new("test").expect("cpath");
        let fd = memfd_create(&name, MFD_CLOEXEC);
        assert!(fd >= 0, "memfd_create should succeed");
        unsafe { syscall1(nr::CLOSE, fd as usize) };
    }
}
