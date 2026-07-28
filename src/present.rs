//! Surface and buffer lifecycle: create the layer surface, hold the frames the
//! renderer draws into, and hand each one to the compositor.
//!
//! The launcher draws in software into shared memory, so a frame is a memfd
//! mapped on both sides. The compositor keeps reading a buffer after the commit
//! that showed it, until it says wl_buffer.release. Two frames are kept and
//! drawn into in turn: draw into the one the compositor is not holding, show it,
//! and the other comes free while this one is on screen.
//!
//! Painting over the buffer being scanned out is what a single frame would mean,
//! and the compositor is within its rights to display the half-painted result.

use crate::platform::arena::ArrayVec;
use crate::platform::conn::Connection;
use crate::platform::error::{Error, Result};
use crate::platform::protocol::{self as proto, KeyboardInteractivity, Layer, ShmFormat};
use crate::platform::wire::Arg;
use crate::shm::PixelBuffer;

/// Frames drawn into in turn. Two is the minimum that lets the renderer touch
/// one while the compositor holds the other, and a launcher has no use for a
/// deeper queue.
const FRAMES: usize = 2;

/// Most buffers retired (awaiting release) at once. A resize retires one, and
/// the compositor releases it a frame later, so this is far above what a burst
/// of resizes can leave outstanding.
const MAX_RETIRED: usize = 8;

/// Upper bound on a surface dimension accepted from a configure event. A buggy
/// or hostile compositor could otherwise echo an enormous size; the buffer math
/// is checked, but capping here keeps a usable surface instead of an error.
pub const MAX_SURFACE_DIM: u32 = 16384;

/// The layer surface the launcher paints on.
pub struct Surface {
    pub id: u32,
    pub layer_surface_id: u32,
    pub configured: bool,
    pub width: u32,
    pub height: u32,
}

/// One shared-memory frame, with the protocol objects that expose it.
struct Frame {
    pixels: PixelBuffer,
    pool_id: u32,
    buffer_id: u32,
    /// True from the commit that shows this frame until the compositor releases
    /// it. Drawing into it in the meantime paints over what is on screen.
    held: bool,
}

/// A frame retired by a resize, kept mapped until the compositor releases it.
/// Unmapping sooner would pull memory out from under a compositor that is still
/// scanning it out.
struct Retired {
    pool_id: u32,
    buffer_id: u32,
    _pixels: PixelBuffer,
}

/// The surface, its frames, and the frames waiting to die.
#[derive(Default)]
pub struct Present {
    pub surface: Option<Surface>,
    frames: ArrayVec<Frame, FRAMES>,
    /// The frame the renderer draws into next.
    current: usize,
    retired: ArrayVec<Retired, MAX_RETIRED>,
}

impl Present {
    pub fn new() -> Self {
        Self::default()
    }

    /// The surface's size, or None before it is created.
    fn size(&self) -> Option<(u32, u32)> {
        self.surface.as_ref().map(|s| (s.width, s.height))
    }

    /// Create the layer surface: a wl_surface promoted to an overlay that sits
    /// above ordinary windows, anchored near the top of the screen.
    #[allow(clippy::too_many_arguments)]
    pub fn create_surface(
        &mut self,
        conn: &mut Connection,
        next_id: &mut impl FnMut() -> u32,
        compositor_id: u32,
        layer_shell_id: u32,
        interactivity: KeyboardInteractivity,
        width: u32,
        height: u32,
    ) -> Result<()> {
        let surface_id = next_id();
        conn.request(
            compositor_id,
            proto::wl_compositor::CREATE_SURFACE,
            &[Arg::NewId(surface_id)],
        )?;

        let layer_surface_id = next_id();
        conn.request(
            layer_shell_id,
            proto::zwlr_layer_shell_v1::GET_LAYER_SURFACE,
            &[
                Arg::NewId(layer_surface_id),
                Arg::Object(surface_id),
                // A null output (0) lets the compositor choose.
                Arg::Object(0),
                Arg::Uint(Layer::Overlay as u32),
                Arg::Str("bnklaunch"),
            ],
        )?;
        conn.request(
            layer_surface_id,
            proto::zwlr_layer_surface_v1::SET_SIZE,
            &[Arg::Uint(width), Arg::Uint(height)],
        )?;
        // Anchored to the top edge and then pushed down by the margin, so the
        // launcher sits in the upper third rather than dead centre.
        conn.request(
            layer_surface_id,
            proto::zwlr_layer_surface_v1::SET_ANCHOR,
            &[Arg::Uint(proto::anchor::TOP)],
        )?;
        conn.request(
            layer_surface_id,
            proto::zwlr_layer_surface_v1::SET_MARGIN,
            &[Arg::Int(350), Arg::Int(0), Arg::Int(0), Arg::Int(0)],
        )?;
        // -1 ignores the exclusive zones panels reserve, so the overlay lies
        // over them rather than below.
        conn.request(
            layer_surface_id,
            proto::zwlr_layer_surface_v1::SET_EXCLUSIVE_ZONE,
            &[Arg::Int(-1)],
        )?;
        conn.request(
            layer_surface_id,
            proto::zwlr_layer_surface_v1::SET_KEYBOARD_INTERACTIVITY,
            &[Arg::Uint(interactivity as u32)],
        )?;
        // The compositor answers this commit with a configure carrying the size
        // to draw at; the frames are allocated then.
        conn.request(surface_id, proto::wl_surface::COMMIT, &[])?;

        self.surface = Some(Surface {
            id: surface_id,
            layer_surface_id,
            configured: false,
            width,
            height,
        });
        conn.flush()
    }

    /// Allocate the frames for the current surface size.
    pub fn create_frames(
        &mut self,
        conn: &mut Connection,
        next_id: &mut impl FnMut() -> u32,
        shm_id: u32,
    ) -> Result<()> {
        let (width, height) = self.size().ok_or_else(|| Error::msg("no surface"))?;
        self.frames.clear();
        self.current = 0;
        for _ in 0..FRAMES {
            let frame = Self::create_frame(conn, next_id, shm_id, width, height)?;
            if self.frames.push(frame).is_err() {
                return Err(Error::msg("too many frames"));
            }
        }
        conn.flush()
    }

    fn create_frame(
        conn: &mut Connection,
        next_id: &mut impl FnMut() -> u32,
        shm_id: u32,
        width: u32,
        height: u32,
    ) -> Result<Frame> {
        let pixels = PixelBuffer::new(width, height)?;
        let size = i32::try_from(pixels.size())
            .map_err(|_| Error::msg("buffer too large for a wl_shm pool"))?;

        let pool_id = next_id();
        conn.request_with_fd(
            shm_id,
            proto::wl_shm::CREATE_POOL,
            &[Arg::NewId(pool_id), Arg::Int(size)],
            &[pixels.fd()],
        )?;

        let buffer_id = next_id();
        conn.request(
            pool_id,
            proto::wl_shm_pool::CREATE_BUFFER,
            &[
                Arg::NewId(buffer_id),
                Arg::Int(0), // offset into the pool
                Arg::Int(width as i32),
                Arg::Int(height as i32),
                Arg::Int(pixels.stride as i32),
                Arg::Uint(ShmFormat::Argb8888 as u32),
            ],
        )?;

        Ok(Frame {
            pixels,
            pool_id,
            buffer_id,
            held: false,
        })
    }

    /// Whether the next frame to draw into is free. False means the compositor
    /// is still holding both, and the caller has to dispatch until it lets one
    /// go.
    pub fn ready(&self) -> bool {
        match self.frames.get(self.current) {
            Some(frame) => !frame.held,
            None => false,
        }
    }

    /// The buffer id of the frame the renderer draws into next, which is the id
    /// the compositor names when it releases that frame.
    #[cfg(test)]
    pub(crate) fn current_buffer_id(&self) -> Option<u32> {
        self.frames.get(self.current).map(|f| f.buffer_id)
    }

    /// The pixels of the frame being drawn into, if it is free to draw into.
    pub fn pixels(&mut self) -> Option<&mut PixelBuffer> {
        let current = self.current;
        self.frames
            .get_mut(current)
            .filter(|f| !f.held)
            .map(|f| &mut f.pixels)
    }

    /// Show the frame just drawn, and turn to the other one for the next.
    pub fn commit(&mut self, conn: &mut Connection) -> Result<()> {
        let (surface_id, width, height) = {
            let surface = self
                .surface
                .as_ref()
                .ok_or_else(|| Error::msg("no surface"))?;
            (surface.id, surface.width as i32, surface.height as i32)
        };
        let current = self.current;
        let buffer_id = self
            .frames
            .get(current)
            .filter(|f| !f.held)
            .map(|f| f.buffer_id)
            .ok_or_else(|| Error::msg("no frame to commit"))?;

        conn.request(
            surface_id,
            proto::wl_surface::ATTACH,
            &[Arg::Object(buffer_id), Arg::Int(0), Arg::Int(0)],
        )?;
        conn.request(
            surface_id,
            proto::wl_surface::DAMAGE,
            &[Arg::Int(0), Arg::Int(0), Arg::Int(width), Arg::Int(height)],
        )?;
        conn.request(surface_id, proto::wl_surface::COMMIT, &[])?;
        conn.flush()?;

        if let Some(frame) = self.frames.get_mut(current) {
            frame.held = true;
        }
        self.current = (current + 1) % self.frames.len().max(1);
        Ok(())
    }

    /// A wl_buffer.release names a frame the compositor has finished with. An id
    /// naming neither a live frame nor a retired one is not ours, and there is
    /// nothing to do about it.
    pub fn release(&mut self, conn: &mut Connection, buffer_id: u32) -> Result<()> {
        if let Some(frame) = self.frames.iter_mut().find(|f| f.buffer_id == buffer_id) {
            frame.held = false;
            return Ok(());
        }

        // A retired frame's release is the last thing that has to happen before
        // its memory can go: the compositor is done scanning it out.
        if let Some(idx) = self.retired.iter().position(|r| r.buffer_id == buffer_id) {
            if let Some(retired) = self.retired.swap_remove(idx) {
                conn.request(retired.buffer_id, proto::wl_buffer::DESTROY, &[])?;
                conn.request(retired.pool_id, proto::wl_shm_pool::DESTROY, &[])?;
                conn.flush()?;
                // retired._pixels drops here: munmap plus close of the memfd.
            }
        }
        Ok(())
    }

    /// Retire every frame and allocate a fresh set at the new size. The old ones
    /// stay mapped until their release arrives.
    pub fn resize_frames(
        &mut self,
        conn: &mut Connection,
        next_id: &mut impl FnMut() -> u32,
        shm_id: u32,
    ) -> Result<()> {
        while let Some(frame) = self.frames.pop() {
            let retired = Retired {
                pool_id: frame.pool_id,
                buffer_id: frame.buffer_id,
                _pixels: frame.pixels,
            };
            // A frame the compositor never held can be destroyed at once; only a
            // held one has to wait for its release.
            if !frame.held {
                conn.request(retired.buffer_id, proto::wl_buffer::DESTROY, &[])?;
                conn.request(retired.pool_id, proto::wl_shm_pool::DESTROY, &[])?;
                continue;
            }
            if self.retired.push(retired).is_err() {
                // The bound is far above what a burst of resizes can produce.
                // Dropping one here would orphan both protocol objects, so say
                // so rather than leak them quietly.
                return Err(Error::msg("too many buffers awaiting release"));
            }
        }
        self.create_frames(conn, next_id, shm_id)
    }
}
