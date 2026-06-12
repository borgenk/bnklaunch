//! Shared memory buffer management.
//!
//! This module handles creating memory-mapped buffers for rendering.
//! Uses memfd_create for anonymous file creation and mmap for mapping.
//! x86_64 Linux only.

use core::ptr::NonNull;

use crate::error::{Error, Result};
use crate::syscall::{self as sys, Fd, RawFd};

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

    /// Fill the entire buffer with a single color.
    pub fn fill(&mut self, color: u32) {
        for pixel in self.pixels_u32() {
            *pixel = color;
        }
    }

    /// Draw a filled rectangle.
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: u32) {
        let width = self.width;
        let x_end = (x + w).min(width);
        let y_end = (y + h).min(self.height);

        let pixels = self.pixels_u32();
        for py in y..y_end {
            for px in x..x_end {
                let offset = (py * width + px) as usize;
                pixels[offset] = color;
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

// SAFETY: The buffer is not accessed from multiple threads simultaneously
unsafe impl Send for PixelBuffer {}

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

    // PixelBuffer has no Debug impl, so the test matches on the Result by hand.
    fn expect_overflow(width: u32, height: u32) {
        match PixelBuffer::new(width, height) {
            Ok(_) => panic!("expected an overflow error for {width}x{height}"),
            // The dimension math fails before any syscall, so this is a
            // message error rather than an errno.
            Err(e) => assert_eq!(e.errno(), 0),
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
}
