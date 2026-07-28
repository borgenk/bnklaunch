//! The crate's error type, standing in for std::io::Error in a no_std build.
//!
//! Most fallible code only propagates errors with the question mark operator,
//! so this stays deliberately small: an error is either a failed syscall's
//! errno or a static message. Carrying the errno lets the event loop test for
//! the conditions it cares about, chiefly a non-blocking would-block.

use crate::platform::syscall::EAGAIN;

/// An OS error (a positive errno) or a static message. Exactly one is set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    errno: i32,
    msg: &'static str,
}

impl Error {
    /// An error from a failed syscall, given its positive errno.
    pub const fn from_errno(errno: i32) -> Self {
        Self { errno, msg: "" }
    }

    /// An error described by a static message, with no OS errno.
    pub const fn msg(msg: &'static str) -> Self {
        Self { errno: 0, msg }
    }

    /// Whether this is a would-block (EAGAIN): the signal to stop draining a
    /// non-blocking socket and park the event loop again.
    pub const fn would_block(&self) -> bool {
        self.errno == EAGAIN
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.errno != 0 {
            write!(f, "os error {}", self.errno)
        } else {
            f.write_str(self.msg)
        }
    }
}

/// Result carrying the crate's error.
pub type Result<T> = core::result::Result<T, Error>;

/// Write a formatted line to stderr without std, the no_std stand-in for
/// eprintln. The message is formatted into a fixed buffer (truncated past its
/// capacity) and a newline is appended.
///
/// Exported through the module rather than with macro_export, which would hoist
/// it to the crate root and leave platform code naming crate::elog: a reference
/// out of the layer, and one the boundary test rightly rejects.
macro_rules! elog {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let mut buf: $crate::platform::arena::ArrayString<256> =
            $crate::platform::arena::ArrayString::new();
        let _ = write!(buf, $($arg)*);
        let _ = buf.push('\n');
        $crate::platform::syscall::write_fd(2, buf.as_bytes());
    }};
}

pub(crate) use elog;
