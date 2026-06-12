//! Wayland wire protocol encoding and decoding.
//!
//! The Wayland protocol uses a binary format over Unix sockets. Each message has:
//! - object_id (u32): Target object
//! - size_opcode (u32): Size in upper 16 bits, opcode in lower 16 bits
//! - payload: Variable length, padded to 4-byte alignment

use crate::arena::{ArrayString, ArrayVec};
use crate::error::{Error, Result};

/// Capacity for a string argument read off the wire (interface names, mime
/// types). These are short; longer strings are rejected.
pub const WIRE_STR_CAP: usize = 256;

/// Header size in bytes (object_id + size_opcode)
pub const HEADER_SIZE: usize = 8;

/// Maximum message size (64KB)
pub const MAX_MESSAGE_SIZE: usize = 65536;

/// Capacity of an outgoing message buffer. The launcher's requests are small (a
/// few args and short strings) and never approach the wire maximum.
pub const BUILD_CAP: usize = 1024;

/// A buffer for building outgoing Wayland messages.
#[derive(Debug)]
pub struct MessageBuilder {
    buf: ArrayVec<u8, BUILD_CAP>,
    opcode: u16,
    /// Set when an argument did not fit, so finish rejects the message rather
    /// than sending a silently truncated one.
    overflowed: bool,
}

impl MessageBuilder {
    /// Create a new message targeting the given object with the given opcode.
    pub fn new(object_id: u32, opcode: u16) -> Self {
        let mut buf: ArrayVec<u8, BUILD_CAP> = ArrayVec::new();
        // Reserve space for header, will fill in size later
        let _ = buf.extend_from_slice(&object_id.to_ne_bytes());
        let _ = buf.extend_from_slice(&[0u8; 4]); // placeholder for size_opcode
        Self {
            buf,
            opcode,
            overflowed: false,
        }
    }

    /// Append bytes, tracking an overflow.
    fn put_bytes(&mut self, bytes: &[u8]) {
        if self.buf.extend_from_slice(bytes).is_err() {
            self.overflowed = true;
        }
    }

    /// Write a u32 argument.
    pub fn put_u32(&mut self, val: u32) {
        self.put_bytes(&val.to_ne_bytes());
    }

    /// Write an i32 argument.
    pub fn put_i32(&mut self, val: i32) {
        self.put_bytes(&val.to_ne_bytes());
    }

    /// Write an object ID argument (same as u32 but semantically different).
    pub fn put_object(&mut self, id: u32) {
        self.put_u32(id);
    }

    /// Write a new_id argument (ID being created by this request).
    pub fn put_new_id(&mut self, id: u32) {
        self.put_u32(id);
    }

    /// Write a string argument (length-prefixed, null-terminated, padded).
    pub fn put_string(&mut self, s: &str) {
        let bytes = s.as_bytes();
        // Length includes null terminator
        let len = bytes.len() + 1;
        self.put_u32(len as u32);
        self.put_bytes(bytes);
        self.put_bytes(&[0]); // null terminator
                              // Pad to 4-byte alignment
        let padding = (4 - (len % 4)) % 4;
        for _ in 0..padding {
            self.put_bytes(&[0]);
        }
    }

    /// Finalize the message, filling in the size field. Rejects a message whose
    /// arguments overflowed the buffer (an oversized request) rather than
    /// sending a truncated, corrupt one.
    pub fn finish(mut self) -> Result<ArrayVec<u8, BUILD_CAP>> {
        if self.overflowed {
            return Err(Error::msg("message exceeds buffer capacity"));
        }
        let size =
            u16::try_from(self.buf.len()).map_err(|_| Error::msg("message exceeds 65535 bytes"))?;
        let size_opcode = ((size as u32) << 16) | (self.opcode as u32);
        self.buf[4..8].copy_from_slice(&size_opcode.to_ne_bytes());
        Ok(self.buf)
    }
}

/// Parser for incoming Wayland messages.
#[derive(Debug)]
pub struct MessageParser<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> MessageParser<'a> {
    /// Create a parser for the given message payload (after header).
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Read a u32 argument.
    pub fn get_u32(&mut self) -> Result<u32> {
        let bytes = self
            .data
            .get(self.pos..self.pos + 4)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .ok_or_else(|| Error::msg("not enough data for u32"))?;
        self.pos += 4;
        Ok(u32::from_ne_bytes(bytes))
    }

    /// Read an i32 argument.
    pub fn get_i32(&mut self) -> Result<i32> {
        let bytes = self
            .data
            .get(self.pos..self.pos + 4)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .ok_or_else(|| Error::msg("not enough data for i32"))?;
        self.pos += 4;
        Ok(i32::from_ne_bytes(bytes))
    }

    /// Read a wl_fixed_t argument (24.8 signed fixed-point) as f64.
    pub fn get_fixed(&mut self) -> Result<f64> {
        let raw = self.get_i32()?;
        Ok(raw as f64 / 256.0)
    }

    /// Read a string argument.
    pub fn get_string(&mut self) -> Result<ArrayString<WIRE_STR_CAP>> {
        let len = self.get_u32()? as usize;
        let mut out: ArrayString<WIRE_STR_CAP> = ArrayString::new();
        if len == 0 {
            return Ok(out);
        }
        if self.pos + len > self.data.len() {
            return Err(Error::msg("not enough data for string"));
        }
        // The wire format null-terminates strings inside the declared length.
        if self.data[self.pos + len - 1] != 0 {
            return Err(Error::msg("string not null-terminated"));
        }
        // Exclude null terminator.
        let s = core::str::from_utf8(&self.data[self.pos..self.pos + len - 1])
            .map_err(|_| Error::msg("invalid utf-8 in string"))?;
        out.push_str(s).map_err(|_| Error::msg("string too long"))?;
        // Skip string data + padding
        let padded_len = (len + 3) & !3;
        self.pos += padded_len;
        Ok(out)
    }
}

/// Parse a message header from raw bytes.
/// Returns (object_id, opcode, message_size, payload_offset).
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

    #[test]
    fn test_message_roundtrip() {
        let mut builder = MessageBuilder::new(1, 1);
        builder.put_u32(42);
        builder.put_string("hello");
        builder.put_i32(-100);
        let msg = builder.finish().unwrap();

        let (obj_id, opcode, size) = parse_header(&msg).unwrap();
        assert_eq!(obj_id, 1);
        assert_eq!(opcode, 1);
        assert_eq!(size, msg.len());

        let mut parser = MessageParser::new(&msg[HEADER_SIZE..]);
        assert_eq!(parser.get_u32().unwrap(), 42);
        assert_eq!(parser.get_string().unwrap(), "hello");
        assert_eq!(parser.get_i32().unwrap(), -100);
    }

    // === String padding tests ===

    #[test]
    fn string_padding_1_byte() {
        // "abc" = 3 chars + 1 null = 4 bytes, no padding needed
        let mut builder = MessageBuilder::new(1, 0);
        builder.put_string("abc");
        let msg = builder.finish().unwrap();

        // Header (8) + length (4) + "abc\0" (4) = 16
        assert_eq!(msg.len(), 16);
    }

    #[test]
    fn string_padding_2_bytes() {
        // "ab" = 2 chars + 1 null = 3 bytes, needs 1 byte padding
        let mut builder = MessageBuilder::new(1, 0);
        builder.put_string("ab");
        let msg = builder.finish().unwrap();

        // Header (8) + length (4) + "ab\0" + 1 padding = 16
        assert_eq!(msg.len(), 16);
    }

    #[test]
    fn string_padding_3_bytes() {
        // "a" = 1 char + 1 null = 2 bytes, needs 2 bytes padding
        let mut builder = MessageBuilder::new(1, 0);
        builder.put_string("a");
        let msg = builder.finish().unwrap();

        // Header (8) + length (4) + "a\0" + 2 padding = 16
        assert_eq!(msg.len(), 16);
    }

    #[test]
    fn string_padding_longer() {
        // "hello" = 5 chars + 1 null = 6 bytes, needs 2 bytes padding
        let mut builder = MessageBuilder::new(1, 0);
        builder.put_string("hello");
        let msg = builder.finish().unwrap();

        // Header (8) + length (4) + "hello\0" (6) + 2 padding = 20
        assert_eq!(msg.len(), 20);

        let mut parser = MessageParser::new(&msg[HEADER_SIZE..]);
        assert_eq!(parser.get_string().unwrap(), "hello");
    }

    #[test]
    fn multiple_strings_roundtrip() {
        let mut builder = MessageBuilder::new(1, 0);
        builder.put_string("first");
        builder.put_string("second");
        builder.put_string("x");
        let msg = builder.finish().unwrap();

        let mut parser = MessageParser::new(&msg[HEADER_SIZE..]);
        assert_eq!(parser.get_string().unwrap(), "first");
        assert_eq!(parser.get_string().unwrap(), "second");
        assert_eq!(parser.get_string().unwrap(), "x");
    }

    // === Header parsing ===

    #[test]
    fn parse_header_extracts_fields() {
        let mut builder = MessageBuilder::new(42, 7);
        builder.put_u32(0);
        let msg = builder.finish().unwrap();

        let (obj_id, opcode, size) = parse_header(&msg).unwrap();
        assert_eq!(obj_id, 42);
        assert_eq!(opcode, 7);
        assert_eq!(size, 12); // 8 header + 4 payload
    }

    #[test]
    fn parse_header_too_short() {
        let data = [0u8; 4]; // Only 4 bytes, need 8
        let result = parse_header(&data);
        assert!(result.is_err());
    }

    #[test]
    fn parse_header_rejects_size_below_header() {
        // Valid 8-byte buffer, but the announced size (4) is below HEADER_SIZE.
        let mut data = Vec::new();
        data.extend_from_slice(&1u32.to_ne_bytes()); // object_id
        let size_opcode = 4u32 << 16; // size = 4, opcode = 0
        data.extend_from_slice(&size_opcode.to_ne_bytes());
        assert!(parse_header(&data).is_err());
    }

    // === Parser error handling ===

    #[test]
    fn parser_u32_eof() {
        let data = [0u8; 2]; // Only 2 bytes
        let mut parser = MessageParser::new(&data);
        assert!(parser.get_u32().is_err());
    }

    #[test]
    fn parser_i32_eof() {
        let data = [0u8; 2];
        let mut parser = MessageParser::new(&data);
        assert!(parser.get_i32().is_err());
    }

    #[test]
    fn parser_string_length_exceeds_data() {
        // Length says 100 bytes but only 4 available
        let mut data = vec![];
        data.extend_from_slice(&100u32.to_ne_bytes());
        data.extend_from_slice(b"abc\0");

        let mut parser = MessageParser::new(&data);
        assert!(parser.get_string().is_err());
    }

    #[test]
    fn parser_empty_string() {
        // Length of 0 should return empty string
        let data = 0u32.to_ne_bytes();
        let mut parser = MessageParser::new(&data);
        assert_eq!(parser.get_string().unwrap(), "");
    }

    // === Edge cases ===

    #[test]
    fn large_object_id() {
        let builder = MessageBuilder::new(u32::MAX, 0);
        let msg = builder.finish().unwrap();

        let (obj_id, _, _) = parse_header(&msg).unwrap();
        assert_eq!(obj_id, u32::MAX);
    }

    #[test]
    fn max_opcode() {
        let builder = MessageBuilder::new(1, u16::MAX);
        let msg = builder.finish().unwrap();

        let (_, opcode, _) = parse_header(&msg).unwrap();
        assert_eq!(opcode, u16::MAX);
    }

    #[test]
    fn finish_rejects_oversized_message() {
        // An argument that overflows the builder must error on finish rather
        // than send a truncated, corrupt message.
        let mut builder = MessageBuilder::new(1, 0);
        builder.put_string(&"x".repeat(70_000));
        assert!(builder.finish().is_err());
    }

    #[test]
    fn negative_i32_roundtrip() {
        let mut builder = MessageBuilder::new(1, 0);
        builder.put_i32(i32::MIN);
        builder.put_i32(-1);
        let msg = builder.finish().unwrap();

        let mut parser = MessageParser::new(&msg[HEADER_SIZE..]);
        assert_eq!(parser.get_i32().unwrap(), i32::MIN);
        assert_eq!(parser.get_i32().unwrap(), -1);
    }
}
