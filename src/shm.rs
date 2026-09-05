//! Shared memory buffer management.
//!
//! This module handles creating memory-mapped buffers for rendering.
//! Uses memfd_create for anonymous file creation and mmap for mapping.
//! x86_64 Linux only.

use core::ptr::NonNull;

use crate::platform::error::{Error, Result};
use crate::platform::syscall::{self as sys, Fd, RawFd};

/// A memory-mapped buffer for pixel data.
pub struct PixelBuffer {
    /// File descriptor for the shared memory
    fd: Fd,
    /// Pointer to mapped memory
    ptr: NonNull<u8>,
    /// Size in bytes
    size: usize,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Stride (bytes per row)
    pub stride: u32,
}

impl PixelBuffer {
    /// Create a new pixel buffer with the given dimensions.
    /// Uses ARGB8888 format (4 bytes per pixel).
    pub fn new(width: u32, height: u32) -> Result<Self> {
        // Dimensions originate from the compositor's configure event, so the
        // size math is on untrusted input. Overflow checks are off in release;
        // a wrapped size would make mmap map fewer bytes than the renderer
        // later writes (out of bounds), so do the math with checked arithmetic
        // and surface any overflow as a clean error.
        let overflow = || Error::msg("buffer dimensions too large");
        let stride = width.checked_mul(4).ok_or_else(overflow)?; // ARGB8888
        let size = (stride as usize)
            .checked_mul(height as usize)
            .ok_or_else(overflow)?;
        let len = sys::off_t::try_from(size).map_err(|_| overflow())?;

        // Create anonymous file
        let fd = memfd_create("bnklaunch_buffer")?;

        // Set file size
        let result = sys::ftruncate(fd.as_raw_fd(), len);
        if result < 0 {
            return Err(Error::from_errno(-result));
        }

        // Memory map the file
        let ptr = unsafe {
            sys::mmap(
                core::ptr::null_mut(),
                size,
                sys::PROT_READ | sys::PROT_WRITE,
                sys::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };

        if sys::mmap_failed(ptr) {
            return Err(Error::from_errno(sys::mmap_errno(ptr)));
        }

        let ptr = NonNull::new(ptr as *mut u8).ok_or_else(|| Error::msg("mmap returned null"))?;

        Ok(Self {
            fd,
            ptr,
            size,
            width,
            height,
            stride,
        })
    }

    /// Get the file descriptor for passing to wl_shm.
    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Get the buffer size in bytes.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Get a mutable slice of the pixel data as u32 (ARGB values).
    pub fn pixels_u32(&mut self) -> &mut [u32] {
        let len = self.size / 4;
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr() as *mut u32, len) }
    }

    /// Stamp one colour over the whole buffer, replacing the frame it held.
    pub fn fill(&mut self, color: u32) {
        self.pixels_u32().fill(color);
    }

    /// The rows of a rectangle, clipped to the buffer. Empty when it starts
    /// past an edge, which is where the caret ends up behind long enough text.
    fn rows(&mut self, x: u32, y: u32, w: u32, h: u32) -> impl Iterator<Item = &mut [u32]> {
        let width = self.width as usize;
        let x_end = (x + w).min(self.width);
        let y_end = (y + h).min(self.height);
        let empty = x >= x_end || y >= y_end;
        let (first, last) = if empty {
            (0, 0)
        } else {
            (y as usize * width, y_end as usize * width)
        };
        let (x, x_end) = (x as usize, x_end as usize);

        // The rows the rectangle spans, then its columns out of each.
        self.pixels_u32()[first..last]
            .chunks_mut(width)
            .map(move |row| &mut row[x..x_end])
    }

    /// Draw a filled rectangle. An opaque colour replaces what is there; one
    /// carrying alpha composites over it.
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: u32) {
        let alpha = color >> 24;
        if alpha == 0 {
            return;
        }
        for row in self.rows(x, y, w, h) {
            if alpha == 0xFF {
                row.fill(color);
            } else {
                for pixel in row.iter_mut() {
                    *pixel = blend(*pixel, color, u8::MAX);
                }
            }
        }
    }

    /// Render text at the given position with a loaded font.
    /// Returns the width of the rendered text in pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text(
        &mut self,
        x: u32,
        y: u32,
        text: &str,
        color: u32,
        max_width: u32,
        font: &crate::font::Font,
        size: f32,
    ) -> u32 {
        let width = self.width;
        let height = self.height;
        font.render_text(
            self.pixels_u32(),
            width,
            height,
            x,
            y,
            text,
            color,
            max_width,
            size,
        )
    }
}

impl Drop for PixelBuffer {
    fn drop(&mut self) {
        unsafe {
            let _ = sys::munmap(self.ptr.as_ptr() as *mut sys::c_void, self.size);
        }
    }
}

/// Composite src over dst, both premultiplied ARGB8888, with src scaled by
/// coverage first: 255 for a solid fill, a glyph's coverage byte for text.
pub fn blend(dst: u32, src: u32, coverage: u8) -> u32 {
    let cov = coverage as u32;
    let inv = 255 - ((src >> 24 & 0xFF) * cov) / 255;

    let a = ((src >> 24 & 0xFF) * cov + (dst >> 24 & 0xFF) * inv) / 255;
    let r = ((src >> 16 & 0xFF) * cov + (dst >> 16 & 0xFF) * inv) / 255;
    let g = ((src >> 8 & 0xFF) * cov + (dst >> 8 & 0xFF) * inv) / 255;
    let b = ((src & 0xFF) * cov + (dst & 0xFF) * inv) / 255;

    (a << 24) | (r << 16) | (g << 8) | b
}

/// Create an anonymous file using memfd_create. The name is a fixed program
/// constant, checked into a CPath for the syscall.
fn memfd_create(name: &str) -> Result<Fd> {
    let cname = sys::CPath::new(name).ok_or_else(|| Error::msg("memfd name too long"))?;
    let fd = sys::memfd_create(&cname, sys::MFD_CLOEXEC);
    if fd < 0 {
        return Err(Error::from_errno(-fd));
    }
    Ok(Fd::new(fd))
}

#[cfg(test)]
mod tests {
    use super::*;

    // PixelBuffer has no Debug impl, so the Result is matched, not unwrapped.
    fn expect_overflow(width: u32, height: u32) {
        match PixelBuffer::new(width, height) {
            Ok(_) => panic!("expected an overflow error for {width}x{height}"),
            // The dimension math fails before any syscall is attempted, so it is
            // this error and not an errno from ftruncate or mmap.
            Err(e) => assert_eq!(e, Error::msg("buffer dimensions too large")),
        }
    }

    #[test]
    fn new_rejects_stride_overflow() {
        // width * 4 overflows u32 long before any syscall is attempted.
        expect_overflow(u32::MAX, 1);
    }

    #[test]
    fn new_rejects_size_overflow() {
        // stride fits in u32 but stride * height exceeds off_t::MAX.
        expect_overflow(1_000_000_000, 3_000_000_000);
    }

    const BLACK: u32 = 0xFF00_0000;
    const WHITE: u32 = 0xFFFF_FFFF;
    /// White at half alpha, premultiplied: every channel is scaled, not just
    /// the alpha.
    const HALF_WHITE: u32 = 0x8080_8080;

    #[test]
    fn blend_leaves_an_opaque_source_alone() {
        // Full coverage of an opaque colour is the colour, whatever is under it.
        assert_eq!(blend(BLACK, WHITE, 255), WHITE);
        assert_eq!(blend(WHITE, BLACK, 255), BLACK);
    }

    #[test]
    fn blend_covering_nothing_changes_nothing() {
        // Two ways to cover nothing: no coverage, or no alpha to cover with.
        assert_eq!(blend(BLACK, WHITE, 0), BLACK);
        assert_eq!(blend(BLACK, 0x0000_0000, 255), BLACK);
        assert_eq!(blend(BLACK, 0x0000_0000, 128), BLACK);
    }

    #[test]
    fn blend_composites_a_translucent_source() {
        // Half white over black is half grey, and the result is still opaque:
        // the destination's own alpha survives what is laid over it.
        assert_eq!(blend(BLACK, HALF_WHITE, 255), 0xFF80_8080);

        // The same colour over nothing keeps its own alpha and no more, which is
        // what makes a translucent window's rows as see-through as its body.
        assert_eq!(blend(0x0000_0000, HALF_WHITE, 255), HALF_WHITE);
    }

    #[test]
    fn blend_never_leaves_a_channel_above_the_alpha() {
        // Premultiplied is an invariant, not a convention: a channel brighter
        // than its own alpha is a colour the compositor cannot read. Walk a
        // spread of sources, destinations and coverages and hold the line.
        for &dst in &[0x0000_0000, 0x8040_2010, BLACK, WHITE, HALF_WHITE] {
            for &src in &[0x0000_0000, 0x4020_1008, BLACK, WHITE, HALF_WHITE] {
                for coverage in [0u8, 1, 63, 128, 200, 254, 255] {
                    let out = blend(dst, src, coverage);
                    let a = out >> 24 & 0xFF;
                    for shift in [16, 8, 0] {
                        let c = out >> shift & 0xFF;
                        assert!(
                            c <= a,
                            "channel {c} above alpha {a} blending {src:08x} over \
                             {dst:08x} at coverage {coverage}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn fill_replaces_what_the_buffer_held() {
        // A frame starts by filling over the frame before last, so a
        // translucent background has to replace those pixels, not land on them.
        let mut buf = PixelBuffer::new(4, 2).expect("buffer");
        buf.fill(WHITE);
        buf.fill(HALF_WHITE);
        assert!(buf.pixels_u32().iter().all(|&p| p == HALF_WHITE));
    }

    #[test]
    fn fill_rect_stamps_an_opaque_colour_and_composites_a_translucent_one() {
        let mut buf = PixelBuffer::new(4, 2).expect("buffer");
        buf.fill(BLACK);

        buf.fill_rect(0, 0, 2, 1, WHITE);
        assert_eq!(buf.pixels_u32()[0], WHITE);

        buf.fill_rect(2, 0, 2, 1, HALF_WHITE);
        assert_eq!(buf.pixels_u32()[2], 0xFF80_8080);

        // The row below was never drawn into.
        assert_eq!(buf.pixels_u32()[4], BLACK);
    }

    #[test]
    fn fill_rect_stops_at_the_buffer_edges() {
        let mut buf = PixelBuffer::new(2, 2).expect("buffer");
        buf.fill(BLACK);
        // Wider and taller than the buffer, which is what a rect anchored near
        // the right edge of the window is.
        buf.fill_rect(1, 1, 10, 10, WHITE);
        assert_eq!(buf.pixels_u32(), &[BLACK, BLACK, BLACK, WHITE]);
        // Starting past the edge altogether, which is where the caret ends up
        // behind long enough text, draws nothing.
        buf.fill_rect(5, 0, 2, 1, WHITE);
        assert_eq!(buf.pixels_u32(), &[BLACK, BLACK, BLACK, WHITE]);
    }
}
