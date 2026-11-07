//! One launcher at a time, and the hotkey closing it when pressed again.
//!
//! The launcher that runs holds a lock in the runtime dir and listens on a
//! socket beside it. A second press of the hotkey starts a second instance,
//! which finds the lock taken, connects to that socket and exits. The connection
//! carries no data: arriving is the whole message, and the open launcher closes
//! when its listener turns readable. Every instance of a Flatpak shares the
//! sandbox's runtime dir, so both work there too.

use crate::platform::arena::ArrayString;
use crate::platform::env;
use crate::platform::error::{elog, Error, Result};
use crate::platform::fs;
use crate::platform::syscall::{
    self, Fd, UnixAddr, AF_UNIX, ENOENT, SOCK_CLOEXEC, SOCK_NONBLOCK, SOCK_STREAM,
};

/// What claim found.
pub(crate) enum Claim {
    /// This process is the launcher.
    First(Instance),
    /// Another launcher holds the lock. It has been asked to close, when its
    /// socket took the connection.
    Second,
}

/// The running launcher's hold on the lock, and the socket a second instance
/// connects to. Both close when this drops.
pub(crate) struct Instance {
    _lock: Fd,
    listener: Option<Fd>,
}

impl Instance {
    /// The listening socket, readable once a second instance has connected.
    /// None when it could not be set up; the hotkey then only opens.
    pub(crate) fn close_requests(&self) -> Option<&Fd> {
        self.listener.as_ref()
    }
}

/// Take the lock in the runtime dir, or ask the launcher holding it to close.
pub(crate) fn claim() -> Result<Claim> {
    claim_in(env::var("XDG_RUNTIME_DIR").unwrap_or("/tmp"))
}

/// claim, with the lock and the socket in dir.
pub(crate) fn claim_in(dir: &str) -> Result<Claim> {
    let lock_path = path_in(dir, "bnklaunch.lock")?;
    let socket_path = path_in(dir, "bnklaunch.sock")?;
    match try_acquire_lock(&lock_path)? {
        Lock::Held(lock) => {
            // A launcher that cannot listen still runs: refusing to start would
            // leave a hotkey that silently does nothing.
            let listener = match listen(&socket_path) {
                Ok(fd) => Some(fd),
                Err(e) => {
                    elog!("bnklaunch: the hotkey cannot close this launcher: {e}");
                    None
                }
            };
            Ok(Claim::First(Instance {
                _lock: lock,
                listener,
            }))
        }
        Lock::Busy => {
            ask_to_close(&socket_path);
            Ok(Claim::Second)
        }
    }
}

fn path_in(dir: &str, name: &str) -> Result<ArrayString<{ fs::PATH_CAP }>> {
    let mut path: ArrayString<{ fs::PATH_CAP }> = ArrayString::new();
    path.push_str(dir)
        .and_then(|_| path.push('/'))
        .and_then(|_| path.push_str(name))
        .map_err(|_| Error::msg("runtime path too long"))?;
    Ok(path)
}

/// The single-instance lock, as try_acquire_lock found it.
enum Lock {
    /// This process holds it until the fd is dropped (and closed).
    Held(Fd),
    /// Another instance holds it.
    Busy,
}

/// Take the lock at path without waiting.
fn try_acquire_lock(path: &str) -> Result<Lock> {
    let cp = fs::cpath(path)?;

    // O_CLOEXEC so a launched app does not inherit the lock fd and hold the
    // single-instance flock for its whole lifetime.
    let fd = syscall::openat(
        syscall::AT_FDCWD,
        &cp,
        syscall::O_RDWR | syscall::O_CREAT | syscall::O_CLOEXEC,
        0o644,
    );
    if fd < 0 {
        return Err(Error::from_errno(-fd));
    }
    let fd = Fd::new(fd);

    let result = syscall::flock(fd.as_raw_fd(), syscall::LOCK_EX | syscall::LOCK_NB);
    match result {
        0 => Ok(Lock::Held(fd)),
        // EWOULDBLOCK, which Linux defines as EAGAIN.
        r if r == -syscall::EAGAIN => Ok(Lock::Busy),
        r => Err(Error::from_errno(-r)),
    }
}

/// Listen on the socket at path. Only the lock holder gets here, so a socket
/// file already at path was left by a launcher that has exited, and is removed
/// to free the name. The listener never accepts: the launcher closes on the
/// first connection, and the kernel drops the queue with it.
fn listen(path: &str) -> Result<Fd> {
    let addr = UnixAddr::new(path).ok_or_else(|| Error::msg("socket path too long"))?;
    let r = syscall::unlink(&fs::cpath(path)?);
    if r < 0 && r != -ENOENT {
        return Err(Error::from_errno(-r));
    }

    // SOCK_CLOEXEC so a launched app does not inherit the listener and keep
    // the name answering after the launcher has gone.
    let fd = syscall::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if fd < 0 {
        return Err(Error::from_errno(-fd));
    }
    let fd = Fd::new(fd);
    let r = syscall::bind(fd.as_raw_fd(), &addr);
    if r < 0 {
        return Err(Error::from_errno(-r));
    }
    let r = syscall::listen(fd.as_raw_fd(), 1);
    if r < 0 {
        return Err(Error::from_errno(-r));
    }
    Ok(fd)
}

/// Connect to the open launcher's socket, and hang up. The connect is
/// non-blocking: a full queue means a launcher already closing, and fails at
/// once. A failure leaves the open launcher as it is, and is not reported: that
/// launcher may still be starting up, or be a build without the socket.
fn ask_to_close(path: &str) {
    let Some(addr) = UnixAddr::new(path) else {
        return;
    };
    let fd = syscall::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
    if fd < 0 {
        return;
    }
    let fd = Fd::new(fd);
    let _ = syscall::connect(fd.as_raw_fd(), &addr);
}

#[cfg(test)]
mod tests {
    use crate::instance::*;
    use crate::platform::syscall::{pollfd, POLLIN};

    fn temp_dir(name: &str) -> String {
        let dir = format!(
            "{}/bnk_{name}_{}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    /// Whether the launcher has been asked to close, without waiting.
    fn asked_to_close(instance: &Instance) -> bool {
        let fd = instance.close_requests().expect("listener");
        let mut pfd = pollfd {
            fd: fd.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        };
        syscall::poll(core::slice::from_mut(&mut pfd), 0) == 1 && pfd.revents & POLLIN != 0
    }

    /// A second instance finds the lock busy and leaves quietly. A lock that
    /// cannot be taken at all is an error the caller reports, not a busy lock:
    /// otherwise the launcher exits silently and the hotkey does nothing.
    #[test]
    fn the_lock_tells_another_instance_apart_from_a_failure() {
        let dir = temp_dir("lock");
        let path = format!("{dir}/bnklaunch.lock");

        let first = try_acquire_lock(&path).expect("first");
        assert!(matches!(first, Lock::Held(_)));
        // flock belongs to the open file, so a second open in this process
        // contends with the first exactly as another instance would.
        assert!(matches!(try_acquire_lock(&path), Ok(Lock::Busy)));
        drop(first);
        assert!(matches!(try_acquire_lock(&path), Ok(Lock::Held(_))));

        let unreachable = format!("{dir}/missing/bnklaunch.lock");
        assert!(try_acquire_lock(&unreachable).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The second press asks the open launcher to close, including after an
    /// earlier launcher exited and left its socket file behind, which is how
    /// every launcher but the first finds the runtime dir.
    #[test]
    fn a_second_instance_asks_the_first_to_close() {
        let dir = temp_dir("inst");

        let Ok(Claim::First(earlier)) = claim_in(&dir) else {
            panic!("the first claim did not take the lock");
        };
        drop(earlier);
        assert!(std::path::Path::new(&format!("{dir}/bnklaunch.sock")).exists());

        let Ok(Claim::First(open)) = claim_in(&dir) else {
            panic!("the claim after an exited launcher did not take the lock");
        };
        assert!(!asked_to_close(&open), "nobody has pressed the hotkey yet");
        assert!(matches!(claim_in(&dir), Ok(Claim::Second)));
        assert!(asked_to_close(&open));

        drop(open);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
