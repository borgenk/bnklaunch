//! Byte-scan primitives and the forward cursor the wire reader is built on.
//!
//! The scan is written branchlessly so the compiler autovectorises it (SSE2 on
//! x86-64) with no intrinsics and no dependency.

/// The byte index of the first needle in hay, or None. A byte-equality position
/// vectorises to a memchr-style scan.
pub fn find_byte(hay: &[u8], needle: u8) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

/// A bounds-checked forward cursor over a borrowed byte slice: the shared
/// mechanics behind the wire and cache readers. It only hands out sub-slices and
/// advances past them, never decoding integers itself, so each format layers its
/// own endianness and error messages on top.
///
/// That separation is load-bearing. The Wayland wire format is native-endian and
/// the cache file is little-endian, so the two decoders are interchangeable only
/// on a little-endian target. This program is exactly that (syscall.rs restricts
/// it to Linux x86_64), but keeping the integer decoding in each wrapper means
/// this cursor stays correct even if that ever changes.
pub struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// The next n bytes, advancing the cursor past them, or None when fewer than
    /// n bytes remain (the cursor is left unmoved). The caller turns None into
    /// its own format-specific error.
    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    /// How many bytes have been consumed so far.
    pub fn pos(&self) -> usize {
        self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_byte_matches_position() {
        assert_eq!(find_byte(b"abc\ndef", b'\n'), Some(3));
        assert_eq!(find_byte(b"\nfirst", b'\n'), Some(0));
        assert_eq!(find_byte(b"no newline", b'\n'), None);
        assert_eq!(find_byte(b"", b'\n'), None);
        // Match past a vector-width boundary is still found at the right index.
        let mut buf = vec![b'x'; 5000];
        buf[4097] = b'\n';
        assert_eq!(find_byte(&buf, b'\n'), Some(4097));
    }

    #[test]
    fn cursor_takes_in_bounds_and_tracks_position() {
        let mut c = Cursor::new(&[1, 2, 3, 4, 5]);
        assert_eq!(c.pos(), 0);
        assert_eq!(c.take(2), Some(&[1, 2][..]));
        assert_eq!(c.pos(), 2);
        assert_eq!(c.take(3), Some(&[3, 4, 5][..]));
        assert_eq!(c.pos(), 5);
        // Zero-length reads are always fine and stay put.
        assert_eq!(c.take(0), Some(&[][..]));
        assert_eq!(c.pos(), 5);
    }

    #[test]
    fn cursor_rejects_overrun_without_advancing() {
        let mut c = Cursor::new(&[1, 2, 3]);
        assert_eq!(c.take(2), Some(&[1, 2][..]));
        // One byte left, asking for two leaves the cursor unmoved.
        assert_eq!(c.take(2), None);
        assert_eq!(c.pos(), 2);
        // A length that would overflow the offset is rejected too.
        assert_eq!(c.take(usize::MAX), None);
        assert_eq!(c.pos(), 2);
    }
}
