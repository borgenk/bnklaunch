//! Wayland wire-format encoding and decoding.
//!
//! A message is: u32 object_id, then u32 (size << 16 | opcode) where size is the
//! whole message length in bytes including this 8-byte header, then the
//! arguments. Everything is host byte order (little-endian on this target), and
//! every argument is padded to a 4-byte boundary. File descriptors are not in
//! the byte stream; they travel as SCM_RIGHTS ancillary data (see conn).

use crate::platform::arena::ArrayVec;
use crate::platform::bytes::Cursor;
use crate::platform::error::{Error, Result};

/// Header size in bytes (object_id + size_opcode).
pub const HEADER_SIZE: usize = 8;

/// Largest message the wire format can express: the size field is 16 bits.
pub const MAX_MESSAGE_SIZE: usize = 65536;

/// Body capacity of one received message. The events the launcher reads are
/// small: the keymap arrives as an fd, not inline.
pub const MSG_BODY_CAP: usize = 16 * 1024;

/// A fully received event: its target object, opcode, and the argument bytes
/// after the header. Owned, so the connection's input buffer can advance
/// independently of a message still being handled.
#[derive(Debug)]
pub struct Message {
    pub object: u32,
    pub opcode: u16,
    pub body: ArrayVec<u8, MSG_BODY_CAP>,
}

impl Message {
    /// A reader over this message's argument bytes.
    pub fn reader(&self) -> Reader<'_> {
        Reader::new(&self.body)
    }
}

/// One request argument. The Bind variant is the generic new-id used only by
/// wl_registry.bind, where the interface is not known statically and is
/// therefore sent inline as interface-string + version + id.
pub enum Arg<'a> {
    Int(i32),
    Uint(u32),
    Object(u32),
    NewId(u32),
    Str(&'a str),
    Bind {
        interface: &'a str,
        version: u32,
        new_id: u32,
    },
}

/// Append an encoded request to buf.
///
/// Err when the request does not fit, and the sink is left holding exactly the
/// whole requests it held before. Rolling back matters: the sink is a stream the
/// compositor reads in order, and a half-written request in front of the next
/// one is a parse error it drops the connection over. Better to refuse the
/// request and keep the stream well formed.
pub fn encode<const N: usize>(
    buf: &mut ArrayVec<u8, N>,
    object: u32,
    opcode: u16,
    args: &[Arg],
) -> Result<()> {
    let start = buf.len();
    let r = encode_into(buf, object, opcode, args, start);
    if r.is_err() {
        buf.truncate(start);
    }
    r
}

fn encode_into<const N: usize>(
    buf: &mut ArrayVec<u8, N>,
    object: u32,
    opcode: u16,
    args: &[Arg],
    start: usize,
) -> Result<()> {
    put(buf, &object.to_ne_bytes())?;
    put(buf, &[0u8; 4])?; // size|opcode, patched once the length is known
    for arg in args {
        match arg {
            Arg::Int(v) => put(buf, &v.to_ne_bytes())?,
            Arg::Uint(v) | Arg::Object(v) | Arg::NewId(v) => put(buf, &v.to_ne_bytes())?,
            Arg::Str(s) => put_str(buf, s)?,
            Arg::Bind {
                interface,
                version,
                new_id,
            } => {
                put_str(buf, interface)?;
                put(buf, &version.to_ne_bytes())?;
                put(buf, &new_id.to_ne_bytes())?;
            }
        }
    }

    // The size lives in the header's high 16 bits, so a request has to fit 16
    // bits. Nothing here builds one near that, but a size that wrapped would
    // silently truncate the length into the opcode bits.
    let size = u16::try_from(buf.len() - start)
        .map_err(|_| Error::msg("wayland request too large to encode"))?;
    let word = ((size as u32) << 16) | u32::from(opcode);
    buf[start + 4..start + 8].copy_from_slice(&word.to_ne_bytes());
    Ok(())
}

fn put<const N: usize>(buf: &mut ArrayVec<u8, N>, bytes: &[u8]) -> Result<()> {
    buf.extend_from_slice(bytes)
        .map_err(|_| Error::msg("wayland send buffer full"))
}

/// Encode a string: length (including the trailing NUL), then the bytes, the
/// NUL, and zero padding up to a 4-byte boundary.
fn put_str<const N: usize>(buf: &mut ArrayVec<u8, N>, s: &str) -> Result<()> {
    let len = s.len() + 1;
    put(buf, &(len as u32).to_ne_bytes())?;
    put(buf, s.as_bytes())?;
    put(buf, &[0])?;
    let pad = (4 - (len % 4)) % 4;
    put(buf, &[0u8; 3][..pad])
}

/// Sequential reader over an event's argument bytes. Wraps the shared
/// bounds-checked cursor, decoding each argument in native byte order (the wire
/// format is host-endian).
///
/// Strings and arrays come back as slices into the message, so reading one costs
/// no copy and imposes no length limit. The strings the compositor relays are
/// chosen by other clients (a clipboard MIME type, say), and a cap here would
/// turn an exotic one into a dead session.
pub struct Reader<'a> {
    cursor: Cursor<'a>,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            cursor: Cursor::new(data),
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        self.cursor
            .take(n)
            .ok_or_else(|| Error::msg("truncated wayland message"))
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn i32(&mut self) -> Result<i32> {
        let b = self.take(4)?;
        Ok(i32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a wl_fixed: a signed 24.8 fixed-point number, as pixels. Used for
    /// pointer coordinates.
    pub fn fixed(&mut self) -> Result<f64> {
        Ok(self.i32()? as f64 / 256.0)
    }

    /// Read a string argument, dropping its trailing NUL and its padding.
    pub fn string(&mut self) -> Result<&'a str> {
        let len = self.u32()? as usize;
        if len == 0 {
            return Ok("");
        }
        let bytes = self.take(len)?;
        let pad = (4 - (len % 4)) % 4;
        self.take(pad)?;
        if bytes[len - 1] != 0 {
            return Err(Error::msg("wayland string is not NUL-terminated"));
        }
        core::str::from_utf8(&bytes[..len - 1])
            .map_err(|_| Error::msg("invalid utf-8 in wayland string"))
    }
}

/// Parse a message header: the target object, the opcode, and the whole message
/// size in bytes, header included.
pub fn parse_header(data: &[u8]) -> Result<(u32, u16, usize)> {
    if data.len() < HEADER_SIZE {
        return Err(Error::msg("not enough data for header"));
    }
    // The length check above guarantees these eight bytes are present.
    let object_id = u32::from_ne_bytes([data[0], data[1], data[2], data[3]]);
    let size_opcode = u32::from_ne_bytes([data[4], data[5], data[6], data[7]]);
    let opcode = (size_opcode & 0xFFFF) as u16;
    let size = (size_opcode >> 16) as usize;
    // Reject a size below the header; the payload slice assumes at least
    // HEADER_SIZE bytes.
    if size < HEADER_SIZE {
        return Err(Error::msg("message size smaller than header"));
    }
    Ok((object_id, opcode, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink roomy enough for any request these tests build.
    type Buf = ArrayVec<u8, 1024>;

    fn encoded(object: u32, opcode: u16, args: &[Arg]) -> Buf {
        let mut buf: Buf = ArrayVec::new();
        encode(&mut buf, object, opcode, args).expect("encode");
        buf
    }

    #[test]
    fn header_carries_size_and_opcode() {
        let buf = encoded(5, 2, &[Arg::Uint(0xdead_beef), Arg::Int(-1)]);
        assert_eq!(buf.len(), 8 + 4 + 4);
        let (object, opcode, size) = parse_header(&buf).expect("header");
        assert_eq!(object, 5);
        assert_eq!(opcode, 2);
        assert_eq!(size, 16);
    }

    #[test]
    fn roundtrips_a_mixed_argument_list() {
        let buf = encoded(1, 1, &[Arg::Uint(42), Arg::Str("hello"), Arg::Int(-100)]);
        let (object, opcode, size) = parse_header(&buf).expect("header");
        assert_eq!((object, opcode, size), (1, 1, buf.len()));

        let mut r = Reader::new(&buf[HEADER_SIZE..]);
        assert_eq!(r.u32().expect("uint"), 42);
        assert_eq!(r.string().expect("str"), "hello");
        assert_eq!(r.i32().expect("int"), -100);
    }

    #[test]
    fn bind_encodes_interface_version_and_id() {
        let buf = encoded(
            2,
            0,
            &[
                Arg::Uint(3),
                Arg::Bind {
                    interface: "zwlr_layer_shell_v1",
                    version: 1,
                    new_id: 9,
                },
            ],
        );
        let mut r = Reader::new(&buf[HEADER_SIZE..]);
        assert_eq!(r.u32().expect("name"), 3);
        assert_eq!(r.string().expect("interface"), "zwlr_layer_shell_v1");
        assert_eq!(r.u32().expect("version"), 1);
        assert_eq!(r.u32().expect("new id"), 9);
    }

    /// Every string is padded out to a 4-byte boundary and its length prefix
    /// counts the NUL, so these all encode to the same size.
    #[test]
    fn strings_pad_to_a_four_byte_boundary() {
        for (s, want_len) in [("abc", 16), ("ab", 16), ("a", 16), ("hello", 20)] {
            let buf = encoded(1, 0, &[Arg::Str(s)]);
            assert_eq!(buf.len(), want_len, "encoded length of {s:?}");
            let mut r = Reader::new(&buf[HEADER_SIZE..]);
            assert_eq!(r.string().expect("str"), s);
        }
    }

    #[test]
    fn multiple_strings_roundtrip() {
        let buf = encoded(
            1,
            0,
            &[Arg::Str("first"), Arg::Str("second"), Arg::Str("x")],
        );
        let mut r = Reader::new(&buf[HEADER_SIZE..]);
        assert_eq!(r.string().expect("first"), "first");
        assert_eq!(r.string().expect("second"), "second");
        assert_eq!(r.string().expect("x"), "x");
    }

    #[test]
    fn reads_a_string_far_longer_than_any_fixed_buffer() {
        // The reader borrows the message, so a string carries no cap of its own.
        // The strings the compositor relays are chosen by other clients: one
        // exotic MIME type from a clipboard owner must not be able to end the
        // session. The sink here is sized for the message; only the read side is
        // under test.
        let long = "x".repeat(4096);
        let mut buf: ArrayVec<u8, 8192> = ArrayVec::new();
        encode(&mut buf, 1, 0, &[Arg::Str(&long)]).expect("encode");
        let mut r = Reader::new(&buf[HEADER_SIZE..]);
        assert_eq!(r.string().expect("str"), long);
    }

    #[test]
    fn fixed_decodes_signed_24_8() {
        // 256 (0x100) is 1.0, -256 is -1.0, 128 is 0.5.
        let mut buf: Buf = ArrayVec::new();
        for raw in [256i32, -256, 128] {
            let _ = buf.extend_from_slice(&raw.to_ne_bytes());
        }
        let mut r = Reader::new(&buf);
        assert_eq!(r.fixed().expect("fixed"), 1.0);
        assert_eq!(r.fixed().expect("fixed"), -1.0);
        assert_eq!(r.fixed().expect("fixed"), 0.5);
    }

    #[test]
    fn negative_ints_roundtrip() {
        let buf = encoded(1, 0, &[Arg::Int(i32::MIN), Arg::Int(-1)]);
        let mut r = Reader::new(&buf[HEADER_SIZE..]);
        assert_eq!(r.i32().expect("int"), i32::MIN);
        assert_eq!(r.i32().expect("int"), -1);
    }

    #[test]
    fn object_id_and_opcode_span_their_full_range() {
        let buf = encoded(u32::MAX, u16::MAX, &[]);
        let (object, opcode, _) = parse_header(&buf).expect("header");
        assert_eq!(object, u32::MAX);
        assert_eq!(opcode, u16::MAX);
    }

    #[test]
    fn reader_rejects_truncation() {
        let mut r = Reader::new(&[0u8, 0, 0]);
        assert!(r.u32().is_err());
        assert!(Reader::new(&[]).string().is_err());

        // A length running past the message is refused, not trusted.
        let mut data: Buf = ArrayVec::new();
        let _ = data.extend_from_slice(&100u32.to_ne_bytes());
        let _ = data.extend_from_slice(b"abc\0");
        assert!(Reader::new(&data).string().is_err());
    }

    #[test]
    fn reader_rejects_a_string_without_its_nul() {
        let mut data: Buf = ArrayVec::new();
        let _ = data.extend_from_slice(&4u32.to_ne_bytes());
        let _ = data.extend_from_slice(b"abcd"); // no terminator inside the length
        assert!(Reader::new(&data).string().is_err());
    }

    #[test]
    fn empty_string_reads_as_empty() {
        let data = 0u32.to_ne_bytes();
        assert_eq!(Reader::new(&data).string().expect("str"), "");
    }

    #[test]
    fn parse_header_rejects_a_short_buffer_and_an_undersized_size() {
        assert!(parse_header(&[0u8; 4]).is_err());

        // Well-formed eight bytes, but the announced size is below the header.
        let mut data: Buf = ArrayVec::new();
        let _ = data.extend_from_slice(&1u32.to_ne_bytes());
        let _ = data.extend_from_slice(&(4u32 << 16).to_ne_bytes());
        assert!(parse_header(&data).is_err());
    }

    #[test]
    fn encode_rejects_a_request_that_does_not_fit_and_keeps_the_buffer_whole() {
        let mut buf: ArrayVec<u8, 32> = ArrayVec::new();
        encode(&mut buf, 1, 0, &[Arg::Uint(7)]).expect("first request fits");
        let whole = buf.len();

        // The string overruns the sink. The partial write is rolled back, so
        // what stays buffered is exactly the requests that did fit, and the
        // stream the compositor eventually reads is still well formed.
        assert!(encode(&mut buf, 1, 0, &[Arg::Str(&"x".repeat(64))]).is_err());
        assert_eq!(buf.len(), whole);
        let (object, opcode, size) = parse_header(&buf).expect("header");
        assert_eq!((object, opcode, size), (1, 0, whole));
    }
}
