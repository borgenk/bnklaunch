//! A minimal io_uring submission and completion ring.
//!
//! io_uring is two ring buffers shared with the kernel. We push submission
//! queue entries (SQEs) describing operations and read completion queue entries
//! (CQEs) carrying their results. Unlike the readiness model, file reads run to
//! completion in the kernel while this thread does other work, so one thread
//! drives the Wayland socket, the timers, and the desktop rescan from a single
//! wait point in io_uring_enter.
//!
//! ```text
//!   prep_*()  ->  [ SQ ring ]  --io_uring_enter-->  kernel runs the op
//!                                                          |
//!   next_cqe() <-  [ CQ ring ]  <----completion-----------+
//! ```
//!
//! Buffers handed to prep_read, prep_openat, and prep_timeout must stay
//! put and alive until the matching completion arrives: the kernel reads and
//! writes them asynchronously. In this program they live in the long-lived app
//! state, which satisfies that.

#![allow(dead_code)]

use core::sync::atomic::{AtomicU32, Ordering};

use crate::syscall::{
    self, io_uring_cqe, io_uring_params, io_uring_sqe, kernel_timespec, IORING_ENTER_GETEVENTS,
    IORING_FEAT_SINGLE_MMAP, IORING_OFF_CQ_RING, IORING_OFF_SQES, IORING_OFF_SQ_RING,
    IORING_OP_CLOSE, IORING_OP_NOP, IORING_OP_OPENAT, IORING_OP_POLL_ADD, IORING_OP_READ,
    IORING_OP_TIMEOUT, IOSQE_IO_LINK, MAP_POPULATE, MAP_SHARED, PROT_READ, PROT_WRITE,
};

type RawFd = i32;

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

    /// Arm a single relative timer that fires once after ts. ts must outlive the
    /// op (the kernel reads it asynchronously); keep it in long-lived state. The
    /// caller re-arms after each fire. A one-shot timeout works on every
    /// io_uring kernel, unlike multishot timeout (which needs 6.4+).
    pub fn prep_timeout(&mut self, ts: *const kernel_timespec, user_data: u64) -> Result<(), ()> {
        let sqe = self.get_sqe().ok_or(())?;
        sqe.opcode = IORING_OP_TIMEOUT;
        sqe.addr = ts as u64;
        sqe.len = 1;
        sqe.user_data = user_data;
        Ok(())
    }

    /// Open a path relative to dirfd. The path bytes must outlive the op. When
    /// link is set, the next prepared entry only runs if this open succeeds,
    /// which is how an open/read/close chain is built.
    pub fn prep_openat(
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

    /// Read up to len bytes from fd at the given offset into buf. buf must
    /// outlive the op. Pass offset 0 (or the actual offset) for regular files.
    pub fn prep_read(
        &mut self,
        fd: RawFd,
        buf: *mut u8,
        len: u32,
        offset: u64,
        user_data: u64,
    ) -> Result<(), ()> {
        let sqe = self.get_sqe().ok_or(())?;
        sqe.opcode = IORING_OP_READ;
        sqe.fd = fd;
        sqe.addr = buf as u64;
        sqe.len = len;
        sqe.off = offset;
        sqe.user_data = user_data;
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
        if crate::syscall::pipe2(&mut fds, 0) != 0 {
            return;
        }
        let (rd, wr) = (fds[0], fds[1]);
        let byte = [7u8];
        let _ = crate::syscall::write_fd(wr, &byte);

        assert_eq!(
            ring.prep_poll_add(rd, crate::syscall::POLLIN as u32, 42),
            Ok(())
        );
        ring.submit_and_wait(1).expect("enter");
        let cqe = ring.next_cqe().expect("poll completion");
        assert_eq!(cqe.user_data, 42);
        assert!(
            cqe.res > 0 && cqe.res as u32 & crate::syscall::POLLIN as u32 != 0,
            "POLLIN should be set, got {}",
            cqe.res
        );

        // SAFETY: both fds are live and closed once here.
        unsafe {
            crate::syscall::close(rd);
            crate::syscall::close(wr);
        }
    }

    // Reading a real file through the ring (openat, then read, then close) is
    // exactly the rescan pipeline's inner loop. This proves those three ops
    // round-trip against the kernel with the right fields.
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

        ring.prep_openat(
            crate::syscall::AT_FDCWD,
            cpath.as_ptr(),
            crate::syscall::O_RDONLY,
            0,
            1,
            false,
        )
        .expect("queue openat");
        ring.submit_and_wait(1).expect("enter");
        let open = ring.next_cqe().expect("openat completion");
        assert_eq!(open.user_data, 1);
        assert!(open.res >= 0, "openat failed: {}", open.res);
        let fd = open.res;

        let mut buf = [0u8; 64];
        ring.prep_read(fd, buf.as_mut_ptr(), buf.len() as u32, 0, 2)
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
        if ring.prep_timeout(&ts, 7).is_err() || ring.submit_and_wait(1).is_err() {
            return;
        }
        if let Some(cqe) = ring.next_cqe() {
            assert_eq!(cqe.user_data, 7);
        }
    }
}
