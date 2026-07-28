//! A minimal io_uring submission and completion ring.
//!
//! io_uring is two ring buffers shared with the kernel. Submission queue entries
//! (SQEs) describe operations; completion queue entries (CQEs) carry their
//! results. One io_uring_enter submits a batch and waits for its completions, so
//! a single thread can have many operations outstanding at once.
//!
//! ```text
//!   prep_*()  ->  [ SQ ring ]  --io_uring_enter-->  kernel runs the op
//!                                                          |
//!   next_cqe() <-  [ CQ ring ]  <----completion-----------+
//! ```
//!
//! Buffers handed to prep_read, prep_openat, and prep_timeout must stay put and
//! alive until the matching completion arrives: the kernel reads and writes them
//! asynchronously, after the call that submitted them has returned.
//!
//! # What this is for, and what it is not for
//!
//! The reason to have it at all is that readiness polling cannot help with
//! files. A regular file is always "ready", so poll and epoll have nothing to
//! say about one, and a read that misses the page cache simply blocks the thread
//! that made it. The only ways to overlap many such reads are a thread pool,
//! which is what async runtimes quietly use for file I/O, or io_uring. In a
//! program with no allocator, no executor, and no threads, io_uring is the only
//! door. That is a real capability, and read_files is it.
//!
//! It does not follow that using it here is faster, and for the desktop scan it
//! measurably is not. Reading the couple of hundred desktop files through the
//! ring collapses about eight hundred syscalls into a dozen submissions, and is
//! six to fifteen times *slower* than reading them one at a time. Two reasons,
//! and the second is the one worth remembering:
//!
//! - The syscalls were never the cost. An open and a read on a warm page cache
//!   are a few hundred nanoseconds of transition around a kernel-side path walk
//!   that io_uring has to do as well. Removing the transition removes the
//!   smaller half.
//! - The first file operation a ring performs costs 12 to 20 milliseconds. An
//!   operation that would block is handed to a kernel worker pool, and standing
//!   that pool up is expensive. A database amortizes it over millions of
//!   operations and never thinks about it again. A launcher starts, scans once,
//!   shows a window, and exits: it pays that in full, on the cold start, and it
//!   is ten times the scan it was meant to accelerate.
//!
//! So the scan reads its files one at a time (see desktop::read_entries), and
//! `make scan-bench` times both against each other so the claim stays checkable.
//!
//! The event loop still waits here, and that costs nothing: a poll and a timeout
//! never block, so neither is ever handed to a worker, and no pool is ever stood
//! up. Ring::new itself is 5 us. One wait point serves the socket and the clock
//! together, and it does it with no thread and no executor behind it, which is
//! the whole point of the thing.

#![allow(dead_code)]

use core::sync::atomic::{AtomicU32, Ordering};

use crate::platform::arena::ArrayVec;
use crate::platform::syscall::{
    self, io_uring_cqe, io_uring_params, io_uring_sqe, kernel_timespec, CPath, RawFd, AT_FDCWD,
    IORING_ENTER_GETEVENTS, IORING_FEAT_SINGLE_MMAP, IORING_OFF_CQ_RING, IORING_OFF_SQES,
    IORING_OFF_SQ_RING, IORING_OP_CLOSE, IORING_OP_NOP, IORING_OP_OPENAT, IORING_OP_POLL_ADD,
    IORING_OP_READ, IORING_OP_TIMEOUT, IOSQE_IO_LINK, MAP_POPULATE, MAP_SHARED, O_RDONLY,
    PROT_READ, PROT_WRITE,
};

/// An io_uring instance with its mapped rings.
pub struct Ring {
    ring_fd: RawFd,

    // Submission ring, mapped from the kernel.
    sq_ptr: *mut u8,
    sq_map_len: usize,
    sq_khead: *const AtomicU32,
    sq_ktail: *const AtomicU32,
    sq_mask: u32,
    sq_entries: u32,
    sq_array: *mut u32,
    sqes: *mut io_uring_sqe,
    sqes_map_len: usize,

    // Our running tail, published to the kernel only at submit time. The count
    // of unconsumed SQEs is recomputed from tail minus the kernel head at each
    // submit rather than tracked, so a short submit strands no entries.
    sq_tail_local: u32,

    // Completion ring. cq_ptr aliases sq_ptr when the kernel reports a single
    // mapping, in which case cq_map_len stays zero so Drop unmaps it once.
    cq_ptr: *mut u8,
    cq_map_len: usize,
    cq_khead: *const AtomicU32,
    cq_ktail: *const AtomicU32,
    cq_mask: u32,
    cqes: *const io_uring_cqe,
}

impl Ring {
    /// Create a ring sized for at least `entries` in-flight submissions.
    /// Returns the errno on failure (positive).
    pub fn new(entries: u32) -> Result<Ring, i32> {
        let mut params = io_uring_params::default();
        let fd = syscall::io_uring_setup(entries, &mut params);
        if fd < 0 {
            return Err(-fd);
        }

        // io_uring_setup takes no open flags, so close-on-exec is set here.
        // Without it the ring fd rides through fork/exec into every launched
        // application, which keeps the ring's mappings pinned for as long as
        // that application runs.
        let r = syscall::fcntl(fd, syscall::F_SETFD, syscall::FD_CLOEXEC);
        if r < 0 {
            // SAFETY: fd is the ring just created, closed once on this error
            // path before any mapping exists or the Ring is built.
            unsafe { syscall::close(fd) };
            return Err(-r);
        }

        let sq_ring_bytes =
            params.sq_off.array as usize + params.sq_entries as usize * size_of::<u32>();
        let cq_ring_bytes =
            params.cq_off.cqes as usize + params.cq_entries as usize * size_of::<io_uring_cqe>();
        let single_mmap = params.features & IORING_FEAT_SINGLE_MMAP != 0;

        // With a single mapping the SQ map must cover both rings.
        let sq_map_len = if single_mmap {
            sq_ring_bytes.max(cq_ring_bytes)
        } else {
            sq_ring_bytes
        };

        // SAFETY: a null hint lets the kernel choose the address; the length
        // and ring fd come from the setup call above.
        let sq_ptr = unsafe {
            syscall::mmap(
                core::ptr::null_mut(),
                sq_map_len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_POPULATE,
                fd,
                IORING_OFF_SQ_RING,
            )
        };
        if syscall::mmap_failed(sq_ptr) {
            let e = syscall::mmap_errno(sq_ptr);
            // SAFETY: fd is the open ring from setup.
            unsafe { syscall::close(fd) };
            return Err(e);
        }
        let sq_ptr = sq_ptr as *mut u8;

        let (cq_ptr, cq_map_len) = if single_mmap {
            (sq_ptr, 0)
        } else {
            // SAFETY: same ring fd, separate completion-ring offset.
            let p = unsafe {
                syscall::mmap(
                    core::ptr::null_mut(),
                    cq_ring_bytes,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED | MAP_POPULATE,
                    fd,
                    IORING_OFF_CQ_RING,
                )
            };
            if syscall::mmap_failed(p) {
                let e = syscall::mmap_errno(p);
                // SAFETY: unmap the SQ ring we just mapped, then close the fd.
                unsafe {
                    syscall::munmap(sq_ptr as *mut _, sq_map_len);
                    syscall::close(fd);
                }
                return Err(e);
            }
            (p as *mut u8, cq_ring_bytes)
        };

        let sqes_map_len = params.sq_entries as usize * size_of::<io_uring_sqe>();
        // SAFETY: the SQE array lives at its own ring offset.
        let sqes = unsafe {
            syscall::mmap(
                core::ptr::null_mut(),
                sqes_map_len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_POPULATE,
                fd,
                IORING_OFF_SQES,
            )
        };
        if syscall::mmap_failed(sqes) {
            let e = syscall::mmap_errno(sqes);
            // SAFETY: tear down both ring mappings (or the one shared map) and
            // the fd before bailing.
            unsafe {
                syscall::munmap(sq_ptr as *mut _, sq_map_len);
                if cq_map_len != 0 {
                    syscall::munmap(cq_ptr as *mut _, cq_map_len);
                }
                syscall::close(fd);
            }
            return Err(e);
        }
        let sqes = sqes as *mut io_uring_sqe;

        // SAFETY: every offset is within the mapping the kernel just sized for
        // us; the head/tail words are plain u32 the kernel accesses atomically,
        // so we view them through AtomicU32.
        unsafe {
            let so = &params.sq_off;
            let co = &params.cq_off;
            let sq_mask = *(sq_ptr.add(so.ring_mask as usize) as *const u32);
            let cq_mask = *(cq_ptr.add(co.ring_mask as usize) as *const u32);
            let ring = Ring {
                ring_fd: fd,
                sq_ptr,
                sq_map_len,
                sq_khead: sq_ptr.add(so.head as usize) as *const AtomicU32,
                sq_ktail: sq_ptr.add(so.tail as usize) as *const AtomicU32,
                sq_mask,
                sq_entries: params.sq_entries,
                sq_array: sq_ptr.add(so.array as usize) as *mut u32,
                sqes,
                sqes_map_len,
                sq_tail_local: (*(sq_ptr.add(so.tail as usize) as *const AtomicU32))
                    .load(Ordering::Relaxed),
                cq_ptr,
                cq_map_len,
                cq_khead: cq_ptr.add(co.head as usize) as *const AtomicU32,
                cq_ktail: cq_ptr.add(co.tail as usize) as *const AtomicU32,
                cq_mask,
                cqes: cq_ptr.add(co.cqes as usize) as *const io_uring_cqe,
            };
            Ok(ring)
        }
    }

    /// Claim the next free submission slot, zeroed. None when the submission
    /// queue is full (too many ops in flight); the caller submits and retries.
    fn get_sqe(&mut self) -> Option<&mut io_uring_sqe> {
        // SAFETY: head is a kernel-updated atomic in the mapped ring.
        let head = unsafe { (*self.sq_khead).load(Ordering::Acquire) };
        if self.sq_tail_local.wrapping_sub(head) >= self.sq_entries {
            return None;
        }
        let idx = (self.sq_tail_local & self.sq_mask) as usize;
        // SAFETY: idx is masked into [0, sq_entries), in bounds for both the
        // SQE array and the index array.
        unsafe {
            let sqe = &mut *self.sqes.add(idx);
            *sqe = io_uring_sqe::default();
            *self.sq_array.add(idx) = idx as u32;
            self.sq_tail_local = self.sq_tail_local.wrapping_add(1);
            Some(sqe)
        }
    }

    /// Publish prepared entries and, when wait_nr > 0, block until at least
    /// that many completions are available. Returns the count the kernel
    /// consumed from the submission queue. Retries EINTR, the spurious wakeup a
    /// job-control stop/resume or ptrace attach delivers while parked in the
    /// wait, so neither suspends the launcher.
    pub fn submit_and_wait(&mut self, wait_nr: u32) -> Result<u32, i32> {
        // SAFETY: release the prepared tail so the kernel sees the new SQEs.
        unsafe { (*self.sq_ktail).store(self.sq_tail_local, Ordering::Release) };
        let flags = if wait_nr > 0 {
            IORING_ENTER_GETEVENTS
        } else {
            0
        };
        loop {
            // Submit whatever the kernel has not yet consumed, derived from tail
            // minus head. A short submit or an EINTR retry recomputes this, so
            // the newest SQEs are never left stranded behind older ones.
            // SAFETY: head is a kernel-updated atomic in the mapped ring.
            let head = unsafe { (*self.sq_khead).load(Ordering::Acquire) };
            let to_submit = self.sq_tail_local.wrapping_sub(head);
            let ret = syscall::io_uring_enter(self.ring_fd, to_submit, wait_nr, flags);
            if ret < 0 {
                let e = -ret as i32;
                if e == syscall::EINTR {
                    continue;
                }
                return Err(e);
            }
            return Ok(ret as u32);
        }
    }

    /// Pop one completion, advancing the ring head so its slot is reusable.
    /// Returns a copy so the caller may submit fresh work while iterating.
    pub fn next_cqe(&mut self) -> Option<io_uring_cqe> {
        // SAFETY: tail is kernel-written; the Acquire pairs with the kernel's
        // release so the cqe contents below are visible.
        let tail = unsafe { (*self.cq_ktail).load(Ordering::Acquire) };
        let head = unsafe { (*self.cq_khead).load(Ordering::Relaxed) };
        if head == tail {
            return None;
        }
        let idx = (head & self.cq_mask) as usize;
        // SAFETY: idx is masked in bounds; the cqe is fully written by the
        // kernel before it advanced tail.
        let cqe = unsafe { *self.cqes.add(idx) };
        // SAFETY: publishing the new head frees the slot for the kernel.
        unsafe { (*self.cq_khead).store(head.wrapping_add(1), Ordering::Release) };
        Some(cqe)
    }

    /// Queue a no-op completion. Used as a setup smoke test.
    pub fn prep_nop(&mut self, user_data: u64) -> Result<(), ()> {
        let sqe = self.get_sqe().ok_or(())?;
        sqe.opcode = IORING_OP_NOP;
        sqe.user_data = user_data;
        Ok(())
    }

    /// Watch a file descriptor for readiness once. poll_mask is a set of poll
    /// event bits, e.g. POLLIN. The caller re-arms after each completion. A
    /// one-shot poll works on every io_uring kernel, unlike multishot poll.
    pub fn prep_poll_add(&mut self, fd: RawFd, poll_mask: u32, user_data: u64) -> Result<(), ()> {
        let sqe = self.get_sqe().ok_or(())?;
        sqe.opcode = IORING_OP_POLL_ADD;
        sqe.fd = fd;
        sqe.op_flags = poll_mask;
        sqe.user_data = user_data;
        Ok(())
    }

    /// Arm a single relative timer that fires once after ts. The caller re-arms
    /// after each fire. A one-shot timeout works on every io_uring kernel,
    /// unlike multishot timeout (which needs 6.4+).
    ///
    /// # Safety
    ///
    /// ts is handed to the kernel, which reads it after submit and outside this
    /// call. It must stay valid and unmoved until the op completes, so it has to
    /// live somewhere longer-lived than the frame that submits it.
    pub unsafe fn prep_timeout(
        &mut self,
        ts: *const kernel_timespec,
        user_data: u64,
    ) -> Result<(), ()> {
        let sqe = self.get_sqe().ok_or(())?;
        sqe.opcode = IORING_OP_TIMEOUT;
        sqe.addr = ts as u64;
        sqe.len = 1;
        sqe.user_data = user_data;
        Ok(())
    }

    /// Open a path relative to dirfd. When link is set, the next prepared entry
    /// only runs if this open succeeds, which is how an open/read/close chain is
    /// built.
    ///
    /// # Safety
    ///
    /// The kernel reads the NUL-terminated path after submit and outside this
    /// call, so the bytes must stay valid and unmoved until the op completes.
    pub unsafe fn prep_openat(
        &mut self,
        dirfd: RawFd,
        path: *const u8,
        flags: i32,
        mode: u32,
        user_data: u64,
        link: bool,
    ) -> Result<(), ()> {
        let sqe = self.get_sqe().ok_or(())?;
        sqe.opcode = IORING_OP_OPENAT;
        sqe.fd = dirfd;
        sqe.addr = path as u64;
        sqe.len = mode;
        sqe.op_flags = flags as u32;
        sqe.user_data = user_data;
        if link {
            sqe.flags |= IOSQE_IO_LINK;
        }
        Ok(())
    }

    /// Read up to len bytes from fd at the given offset into buf. Pass offset 0
    /// (or the actual offset) for regular files.
    ///
    /// # Safety
    ///
    /// The kernel writes through buf after submit and outside this call. It must
    /// point at len writable bytes that stay valid and unmoved until the op
    /// completes: a stack buffer that goes out of scope first is memory
    /// corruption the caller cannot see.
    pub unsafe fn prep_read(
        &mut self,
        fd: RawFd,
        buf: *mut u8,
        len: u32,
        offset: u64,
        user_data: u64,
        link: bool,
    ) -> Result<(), ()> {
        let sqe = self.get_sqe().ok_or(())?;
        sqe.opcode = IORING_OP_READ;
        sqe.fd = fd;
        sqe.addr = buf as u64;
        sqe.len = len;
        sqe.off = offset;
        sqe.user_data = user_data;
        if link {
            sqe.flags |= IOSQE_IO_LINK;
        }
        Ok(())
    }

    /// Close a file descriptor through the ring.
    pub fn prep_close(&mut self, fd: RawFd, user_data: u64) -> Result<(), ()> {
        let sqe = self.get_sqe().ok_or(())?;
        sqe.opcode = IORING_OP_CLOSE;
        sqe.fd = fd;
        sqe.user_data = user_data;
        Ok(())
    }
}

/// Read the first `stride` bytes of many files, all at once.
///
/// Slot i of `bufs` is `bufs[i * stride..][..stride]`, and `lens[i]` says how
/// many bytes landed in it. A file that could not be opened or read gets zero,
/// which the caller reads as "skip this one".
///
/// This is what the ring is here for, and the one thing nothing else on Linux
/// does. A regular file is always "ready", so poll and epoll have nothing to
/// tell you about one: a read that misses the page cache simply blocks the
/// thread that made it. The only ways to overlap many such reads are a thread
/// pool, which is what async runtimes quietly use for file I/O, or this. Here
/// the whole batch goes to the kernel in one call and comes back in one call,
/// with no threads and no executor.
///
/// The batch must fit the ring: it takes one submission per file to open, then
/// two per opened file to read and close, so a ring of N entries handles N/2
/// files at a time. The caller chunks.
///
/// # Safety
///
/// The paths and the buffers are borrowed for the whole call, and both waits
/// below block until every operation the kernel was given has reported back. So
/// nothing the kernel holds a pointer to can go away underneath it, which is the
/// obligation the prep functions carry.
pub fn read_files(
    ring: &mut Ring,
    paths: &[CPath],
    bufs: &mut [u8],
    stride: usize,
    lens: &mut [usize],
) -> Result<(), ()> {
    if paths.len() > lens.len() || paths.len() * stride > bufs.len() {
        return Err(());
    }
    for len in lens.iter_mut() {
        *len = 0;
    }
    if paths.is_empty() {
        return Ok(());
    }

    // Open everything. The kernel does the path walks; we make one call.
    for (i, path) in paths.iter().enumerate() {
        // SAFETY: paths outlives the wait below, so the kernel reads a live
        // NUL-terminated path.
        unsafe { ring.prep_openat(AT_FDCWD, path.as_ptr(), O_RDONLY, 0, i as u64, false)? };
    }
    ring.submit_and_wait(paths.len() as u32).map_err(|_| ())?;

    let mut fds: ArrayVec<RawFd, MAX_BATCH> = ArrayVec::new();
    for _ in 0..paths.len() {
        let _ = fds.push(-1);
    }
    while let Some(cqe) = ring.next_cqe() {
        let i = cqe.user_data as usize;
        if let Some(slot) = fds.get_mut(i) {
            *slot = cqe.res; // negative is a negated errno: the open failed
        }
    }

    // Read each open file and close it. The two are linked, because io_uring
    // orders nothing between unlinked operations: an independent close could run
    // before its own read and pull the descriptor out from under it.
    let mut queued = 0u32;
    for (i, &fd) in fds.iter().enumerate() {
        if fd < 0 {
            continue;
        }
        let slot = &mut bufs[i * stride..(i + 1) * stride];
        // SAFETY: bufs outlives the wait below, and each slot is a distinct,
        // non-overlapping run of `stride` writable bytes, which is exactly the
        // length handed to the kernel.
        unsafe { ring.prep_read(fd, slot.as_mut_ptr(), stride as u32, 0, i as u64, true)? };
        ring.prep_close(fd, CLOSE_TAG)?;
        queued += 2;
    }
    if queued == 0 {
        return Ok(());
    }
    ring.submit_and_wait(queued).map_err(|_| ())?;

    while let Some(cqe) = ring.next_cqe() {
        if cqe.user_data == CLOSE_TAG {
            continue;
        }
        let i = cqe.user_data as usize;
        if cqe.res > 0 {
            if let Some(len) = lens.get_mut(i) {
                *len = cqe.res as usize;
            }
        }
    }
    Ok(())
}

/// user_data for the closes, which have no result worth reading. Real slots are
/// indices, so no index can collide with it.
const CLOSE_TAG: u64 = u64::MAX;

/// Most files one read_files call handles. The ring is sized for twice this,
/// since an opened file costs a read and a close.
pub const MAX_BATCH: usize = 64;

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: each pointer and length is one this Ring mapped in new and has
        // not unmapped; cq is unmapped only when it had its own mapping.
        unsafe {
            syscall::munmap(self.sqes as *mut _, self.sqes_map_len);
            if self.cq_map_len != 0 {
                syscall::munmap(self.cq_ptr as *mut _, self.cq_map_len);
            }
            syscall::munmap(self.sq_ptr as *mut _, self.sq_map_len);
            syscall::close(self.ring_fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Submitting a NOP and reaping its completion exercises the full setup,
    // submit, and reap path against the real kernel. Sandboxes that forbid
    // io_uring (seccomp) make setup fail; treat that as "not available here"
    // rather than a test failure, since it is an environment limit.
    #[test]
    fn nop_roundtrips_through_the_ring() {
        let mut ring = match Ring::new(8) {
            Ok(r) => r,
            Err(_) => return,
        };
        assert_eq!(ring.prep_nop(0xabcd), Ok(()));
        let submitted = ring.submit_and_wait(1).expect("enter");
        assert_eq!(submitted, 1);
        let cqe = ring.next_cqe().expect("one completion");
        assert_eq!(cqe.user_data, 0xabcd);
        assert_eq!(cqe.res, 0);
        assert!(ring.next_cqe().is_none());
    }

    // A poll on a pipe with data waiting reports readiness, which is exactly how
    // the event loop learns the Wayland socket has events.
    #[test]
    fn poll_reports_a_readable_pipe() {
        let mut ring = match Ring::new(8) {
            Ok(r) => r,
            Err(_) => return,
        };
        let mut fds = [0i32; 2];
        if crate::platform::syscall::pipe2(&mut fds, 0) != 0 {
            return;
        }
        let (rd, wr) = (fds[0], fds[1]);
        let byte = [7u8];
        let _ = crate::platform::syscall::write_fd(wr, &byte);

        assert_eq!(
            ring.prep_poll_add(rd, crate::platform::syscall::POLLIN as u32, 42),
            Ok(())
        );
        ring.submit_and_wait(1).expect("enter");
        let cqe = ring.next_cqe().expect("poll completion");
        assert_eq!(cqe.user_data, 42);
        assert!(
            cqe.res > 0 && cqe.res as u32 & crate::platform::syscall::POLLIN as u32 != 0,
            "POLLIN should be set, got {}",
            cqe.res
        );

        // SAFETY: both fds are live and closed once here.
        unsafe {
            crate::platform::syscall::close(rd);
            crate::platform::syscall::close(wr);
        }
    }

    // openat, then read, then close, against a real file: the three operations
    // read_files is built out of. This proves each round-trips through the ring
    // with the right fields, one at a time, before read_files batches them.
    #[test]
    fn openat_read_close_roundtrips_a_file() {
        let mut ring = match Ring::new(8) {
            Ok(r) => r,
            Err(_) => return,
        };

        let path = std::env::temp_dir().join(format!("bnk_uring_{}", std::process::id()));
        std::fs::write(&path, b"hello uring").expect("write temp");
        // openat reads the path as a NUL-terminated C string.
        let mut cpath = path
            .clone()
            .into_os_string()
            .into_string()
            .expect("utf8 path");
        cpath.push('\0');

        // SAFETY: cpath lives until the end of this test, past the completion
        // of the open op below.
        unsafe {
            ring.prep_openat(
                crate::platform::syscall::AT_FDCWD,
                cpath.as_ptr(),
                crate::platform::syscall::O_RDONLY,
                0,
                1,
                false,
            )
        }
        .expect("queue openat");
        ring.submit_and_wait(1).expect("enter");
        let open = ring.next_cqe().expect("openat completion");
        assert_eq!(open.user_data, 1);
        assert!(open.res >= 0, "openat failed: {}", open.res);
        let fd = open.res;

        let mut buf = [0u8; 64];
        // SAFETY: buf outlives the read op, which is waited on below before the
        // buffer is read or dropped.
        unsafe { ring.prep_read(fd, buf.as_mut_ptr(), buf.len() as u32, 0, 2, false) }
            .expect("queue read");
        ring.submit_and_wait(1).expect("enter");
        let read = ring.next_cqe().expect("read completion");
        assert_eq!(read.user_data, 2);
        assert!(read.res > 0, "read failed: {}", read.res);
        assert_eq!(&buf[..read.res as usize], b"hello uring");

        ring.prep_close(fd, 3).expect("queue close");
        ring.submit_and_wait(1).expect("enter");
        assert_eq!(ring.next_cqe().expect("close completion").user_data, 3);

        let _ = std::fs::remove_file(&path);
    }

    /// read_files against real files: every one comes back with its own bytes in
    /// its own slot, and a path that does not exist reports zero rather than
    /// taking the batch down with it.
    #[test]
    fn read_files_fills_a_slot_per_file() {
        let mut ring = match Ring::new(16) {
            Ok(r) => r,
            // A sandbox or a seccomp policy can refuse io_uring outright, which
            // is why every caller keeps a path that does not need it.
            Err(_) => return,
        };

        let dir = std::env::temp_dir().join(format!("bnk_readfiles_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let bodies = ["first file", "second file, a bit longer", ""];
        let mut cpaths: ArrayVec<CPath, 4> = ArrayVec::new();
        for (i, body) in bodies.iter().enumerate() {
            let path = dir.join(format!("f{i}"));
            std::fs::write(&path, body).expect("write");
            let cp = CPath::new(path.to_str().expect("utf8")).expect("cpath");
            let _ = cpaths.push(cp);
        }
        // A path that is not there: it must report zero, not derail the rest.
        let missing = dir.join("nope");
        let _ = cpaths.push(CPath::new(missing.to_str().expect("utf8")).expect("cpath"));

        const SLOT: usize = 64;
        let mut bufs = [0u8; SLOT * 4];
        let mut lens = [0usize; 4];
        read_files(&mut ring, &cpaths, &mut bufs, SLOT, &mut lens).expect("read_files");

        for (i, body) in bodies.iter().enumerate() {
            assert_eq!(lens[i], body.len(), "length of file {i}");
            assert_eq!(
                &bufs[i * SLOT..i * SLOT + body.len()],
                body.as_bytes(),
                "contents of file {i}"
            );
        }
        assert_eq!(lens[3], 0, "a missing file reads as nothing");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // A short one-shot timeout posts a completion tagged with its user_data,
    // the wakeup the loop uses for key repeat and the caret blink. Tolerant of
    // kernels that reject the op so it never hangs or flakes.
    #[test]
    fn timeout_posts_a_completion() {
        let mut ring = match Ring::new(8) {
            Ok(r) => r,
            Err(_) => return,
        };
        let ts = kernel_timespec {
            tv_sec: 0,
            tv_nsec: 2_000_000,
        };
        // SAFETY: ts outlives the timeout op, which is waited on below.
        if unsafe { ring.prep_timeout(&ts, 7) }.is_err() || ring.submit_and_wait(1).is_err() {
            return;
        }
        if let Some(cqe) = ring.next_cqe() {
            assert_eq!(cqe.user_data, 7);
        }
    }
}
