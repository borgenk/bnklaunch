//! Glyph rasterization backed by the system FreeType (libfreetype.so.6).
//!
//! FreeType handles the hard parts of turning a TrueType or OpenType outline
//! into an anti-aliased coverage bitmap: hinting, scan conversion, and the
//! per-size metrics layout needs. This module is the thin FFI layer over it,
//! mirroring xkb.rs: a small extern "C" block, one struct that owns the C-side
//! handles, a Drop impl, and methods that return owned data.
//!
//! Only one font is ever loaded, so the Library and Face live in a single
//! struct rather than separate wrappers. That sidesteps the teardown ordering
//! (a face must be freed before the library it came from) and the lifetime
//! plumbing a split would require.
//!
//! FreeType exposes glyph data as struct fields reached from the Face pointer
//! (face->glyph->bitmap, face->size->metrics), not through accessors. The
//! repr(C) structs below mirror those layouts up to the last field this module
//! reads; FreeType allocates the full structs, this code only reads fields, so
//! declaring the prefix with matching primitive types is enough and repr(C)
//! reproduces the C padding. The fields touched here are old and load-bearing
//! for every FreeType consumer, so the offsets have been ABI-stable for years.

use core::ffi::{c_char, c_int, c_long, c_short, c_uint, c_ulong, c_ushort, c_void};
use core::ptr::{self, NonNull};

use crate::platform::arena::ArrayVec;
use crate::platform::error::{Error, Result};

/// Largest glyph coverage bitmap (width * rows). The UI font sizes are small,
/// so a glyph never approaches this; an oversized one renders blank.
pub const GLYPH_MAX: usize = 16 * 1024;

// ---------------------------------------------------------------------------
// FFI surface.
// ---------------------------------------------------------------------------

type FtError = c_int;
/// FT_Library: opaque, only ever held as a handle and freed.
type FtLibrary = *mut c_void;
/// FT_Face: a pointer to a struct whose fields this module reads.
type FtFace = *mut FtFaceRec;

/// Also rasterize to an 8-bit coverage bitmap, default gray render mode
/// (FT_LOAD_RENDER). FT_RENDER_MODE_NORMAL needs no extra argument.
const FT_LOAD_RENDER: i32 = 1 << 2;
/// Ignore embedded bitmap strikes so a glyph with an outline renders to the
/// 8-bit gray bitmap this module expects, rather than a mono or color strike
/// (FT_LOAD_NO_BITMAP).
const FT_LOAD_NO_BITMAP: i32 = 1 << 3;

/// FT_PIXEL_MODE_GRAY: one coverage byte per pixel, the only mode whose row
/// width in bytes equals its pixel width.
const FT_PIXEL_MODE_GRAY: u8 = 2;

#[link(name = "freetype")]
#[allow(non_snake_case)]
unsafe extern "C" {
    fn FT_Init_FreeType(alibrary: *mut FtLibrary) -> FtError;
    fn FT_Done_FreeType(library: FtLibrary) -> FtError;

    fn FT_New_Memory_Face(
        library: FtLibrary,
        file_base: *const u8,
        file_size: c_long,
        face_index: c_long,
        aface: *mut FtFace,
    ) -> FtError;
    fn FT_Done_Face(face: FtFace) -> FtError;

    fn FT_Set_Pixel_Sizes(face: FtFace, pixel_width: c_uint, pixel_height: c_uint) -> FtError;
    fn FT_Load_Char(face: FtFace, char_code: c_ulong, load_flags: i32) -> FtError;
}

// ---------------------------------------------------------------------------
// C struct layouts (read-only mirrors of the FreeType headers).
//
// Most fields are present only to place the ones this module reads at the
// right offset, hence allow(dead_code) on each.
// ---------------------------------------------------------------------------

/// FT_Generic: a client data pointer plus a finalizer. Unread; present so the
/// structs it is embedded in keep the right field offsets.
#[repr(C)]
#[allow(dead_code)]
struct FtGeneric {
    data: *mut c_void,
    finalizer: *mut c_void,
}

/// FT_Vector: advance is reported here in 26.6 fixed point (1/64 pixel).
#[repr(C)]
#[allow(dead_code)]
struct FtVector {
    x: c_long,
    y: c_long,
}

/// FT_Bitmap: the rendered glyph. buffer holds rows * |pitch| coverage bytes,
/// each 0..=255 under the default gray render mode. pitch is the byte stride
/// between rows and is signed (negative for an upward row flow).
#[repr(C)]
#[allow(dead_code)]
struct FtBitmap {
    rows: c_uint,
    width: c_uint,
    pitch: c_int,
    buffer: *const u8,
    num_grays: c_ushort,
    pixel_mode: u8,
    palette_mode: u8,
    palette: *mut c_void,
}

/// FT_Size_Metrics: per-size layout values. ascender and descender are 26.6
/// fixed point; descender is negative.
#[repr(C)]
#[allow(dead_code)]
struct FtSizeMetrics {
    x_ppem: c_ushort,
    y_ppem: c_ushort,
    x_scale: c_long,
    y_scale: c_long,
    ascender: c_long,
    descender: c_long,
    height: c_long,
    max_advance: c_long,
}

/// FT_GlyphSlotRec prefix up to bitmap_top, the last field this module reads.
/// metrics stands in for FT_Glyph_Metrics (eight FT_Pos); its value is unused
/// but its size places advance and bitmap correctly.
#[repr(C)]
#[allow(dead_code)]
struct FtGlyphSlotRec {
    library: *mut c_void,
    face: *mut c_void,
    next: *mut c_void,
    glyph_index: c_uint,
    generic: FtGeneric,
    metrics: [c_long; 8],
    linear_hori_advance: c_long,
    linear_vert_advance: c_long,
    advance: FtVector,
    format: c_int,
    bitmap: FtBitmap,
    bitmap_left: c_int,
    bitmap_top: c_int,
}

/// FT_SizeRec prefix up to metrics.
#[repr(C)]
#[allow(dead_code)]
struct FtSizeRec {
    face: *mut c_void,
    generic: FtGeneric,
    metrics: FtSizeMetrics,
}

/// FT_FaceRec prefix up to size. bbox stands in for FT_BBox (four FT_Pos).
/// The seven c_short fields after units_per_em are the ascender..underline
/// run; only glyph and size are read.
#[repr(C)]
#[allow(dead_code)]
struct FtFaceRec {
    num_faces: c_long,
    face_index: c_long,
    face_flags: c_long,
    style_flags: c_long,
    num_glyphs: c_long,
    family_name: *mut c_char,
    style_name: *mut c_char,
    num_fixed_sizes: c_int,
    available_sizes: *mut c_void,
    num_charmaps: c_int,
    charmaps: *mut c_void,
    generic: FtGeneric,
    bbox: [c_long; 4],
    units_per_em: c_ushort,
    ascender: c_short,
    descender: c_short,
    height: c_short,
    max_advance_width: c_short,
    max_advance_height: c_short,
    underline_position: c_short,
    underline_thickness: c_short,
    glyph: *mut FtGlyphSlotRec,
    size: *mut FtSizeRec,
}

// ---------------------------------------------------------------------------
// Owning wrapper.
// ---------------------------------------------------------------------------

/// A rasterized glyph, owned so no reference into FreeType memory escapes.
/// coverage is tightly packed top-down, width bytes per row, rows rows.
pub struct Glyph {
    /// Horizontal offset from the pen to the bitmap's left edge.
    pub left: i32,
    /// Vertical offset from the baseline up to the bitmap's top edge.
    pub top: i32,
    pub width: usize,
    pub rows: usize,
    /// Pen advance in pixels.
    pub advance: f32,
    pub coverage: ArrayVec<u8, GLYPH_MAX>,
}

impl Glyph {
    /// A glyph with no ink and no advance (a load failure or a missing glyph).
    fn empty() -> Self {
        Self {
            left: 0,
            top: 0,
            width: 0,
            rows: 0,
            advance: 0.0,
            coverage: ArrayVec::new(),
        }
    }
}

/// Owns the FreeType library, the face, and the font-file mapping the face
/// borrows.
///
/// FT_New_Memory_Face does not copy its input; it keeps a pointer into the
/// bytes for the life of the face. An mmap of the font file backs them: its
/// address is stable however the Face is moved, and it is unmapped only after
/// FT_Done_Face runs in Drop.
pub struct Face {
    library: NonNull<c_void>,
    face: NonNull<FtFaceRec>,
    map_ptr: *mut c_void,
    map_len: usize,
}

// SAFETY: Face uniquely owns its FreeType handles and is never shared across
// threads (the Font that holds it lives on the main thread). The raw pointers
// make it !Send by default, which is the behavior we want, so no unsafe impls.

impl Face {
    /// Build a face from an mmap of the font file. Takes ownership of the
    /// mapping (unmapped on drop); FreeType reads from it for the face's life.
    pub fn from_mmap(map_ptr: *mut c_void, map_len: usize) -> Result<Self> {
        let mut library: FtLibrary = ptr::null_mut();
        // SAFETY: alibrary points at a live local; FT_Init_FreeType writes the
        // new library handle through it and returns nonzero on failure.
        let err = unsafe { FT_Init_FreeType(&mut library) };
        if err != 0 {
            // SAFETY: we own the mapping and free it on this error path.
            unsafe { crate::platform::syscall::munmap(map_ptr, map_len) };
            return Err(Error::msg("FT_Init_FreeType failed"));
        }
        let library = match NonNull::new(library) {
            Some(l) => l,
            None => {
                // SAFETY: we own the mapping.
                unsafe { crate::platform::syscall::munmap(map_ptr, map_len) };
                return Err(Error::msg("FT_Init_FreeType returned null"));
            }
        };

        let mut face: FtFace = ptr::null_mut();
        // SAFETY: library is valid; the mapping describes the font bytes and
        // outlives the face (unmapped in Drop after FT_Done_Face); aface points
        // at a live local. FreeType keeps a pointer into the mapping.
        let err = unsafe {
            FT_New_Memory_Face(
                library.as_ptr(),
                map_ptr as *const u8,
                map_len as c_long,
                0,
                &mut face,
            )
        };
        if err != 0 {
            // SAFETY: library came from FT_Init_FreeType; the mapping is ours.
            unsafe {
                FT_Done_FreeType(library.as_ptr());
                crate::platform::syscall::munmap(map_ptr, map_len);
            }
            return Err(Error::msg("FT_New_Memory_Face failed"));
        }
        let face = match NonNull::new(face) {
            Some(face) => face,
            None => {
                // SAFETY: library is valid and not yet freed; mapping is ours.
                unsafe {
                    FT_Done_FreeType(library.as_ptr());
                    crate::platform::syscall::munmap(map_ptr, map_len);
                }
                return Err(Error::msg("FT_New_Memory_Face returned null"));
            }
        };

        Ok(Self {
            library,
            face,
            map_ptr,
            map_len,
        })
    }

    /// Select the pixel size FreeType renders and reports metrics at. The
    /// width argument of zero tells FreeType to match it to the height.
    pub fn set_pixel_size(&self, size: u32) -> Result<()> {
        // SAFETY: face is a valid FT_Face; the call only reads size arguments.
        let err = unsafe { FT_Set_Pixel_Sizes(self.face.as_ptr(), 0, size) };
        if err != 0 {
            return Err(Error::msg("FT_Set_Pixel_Sizes failed"));
        }
        Ok(())
    }

    /// Render a character at the current pixel size, copying the coverage out.
    /// A load failure or a glyphless character yields an empty Glyph.
    pub fn rasterize(&self, ch: char) -> Glyph {
        // SAFETY: face is valid; FT_Load_Char renders into the face's shared
        // glyph slot. No reference into that slot is held across this call.
        // NO_BITMAP forces the outline path so the result is 8-bit gray.
        let err = unsafe {
            FT_Load_Char(
                self.face.as_ptr(),
                ch as u32 as c_ulong,
                FT_LOAD_RENDER | FT_LOAD_NO_BITMAP,
            )
        };
        if err != 0 {
            return Glyph::empty();
        }
        // SAFETY: the load succeeded, so face->glyph points at the populated
        // slot. The borrow ends before any later FreeType call, so the slot is
        // not mutated underneath it. coverage is copied out before returning.
        let slot = unsafe { (*self.face.as_ptr()).glyph };
        let Some(slot) = NonNull::new(slot) else {
            return Glyph::empty();
        };
        let slot = unsafe { slot.as_ref() };
        let advance = slot.advance.x as f32 / 64.0;
        // The blit treats coverage as one byte per pixel, which holds only for
        // the gray render mode. A font with only embedded strikes (no outline)
        // can still yield a mono or color bitmap; skip its ink but keep the
        // advance so the glyph shows blank without disturbing layout.
        if slot.bitmap.pixel_mode != FT_PIXEL_MODE_GRAY {
            return Glyph {
                advance,
                ..Glyph::empty()
            };
        }
        Glyph {
            left: slot.bitmap_left,
            top: slot.bitmap_top,
            width: slot.bitmap.width as usize,
            rows: slot.bitmap.rows as usize,
            advance,
            coverage: copy_coverage(&slot.bitmap),
        }
    }

    /// Ascent and descent at the current pixel size, in pixels. Descent is
    /// negative, matching the FreeType convention.
    pub fn line_metrics(&self) -> (f32, f32) {
        // SAFETY: face is valid; face->size is non-null once a size is set,
        // which the public callers always do first. The metrics are copied out.
        let size = unsafe { (*self.face.as_ptr()).size };
        let Some(size) = NonNull::new(size) else {
            return (0.0, 0.0);
        };
        let metrics = unsafe { &size.as_ref().metrics };
        (
            metrics.ascender as f32 / 64.0,
            metrics.descender as f32 / 64.0,
        )
    }
}

impl Drop for Face {
    fn drop(&mut self) {
        // The face must be freed before the library it came from, and the
        // mapping the face reads from only after both. SAFETY: each handle came
        // from its matching FreeType new call and is freed exactly once; the
        // mapping is ours and unmapped once.
        unsafe {
            FT_Done_Face(self.face.as_ptr());
            FT_Done_FreeType(self.library.as_ptr());
            crate::platform::syscall::munmap(self.map_ptr, self.map_len);
        }
    }
}

/// Copy a FreeType bitmap into a tightly packed top-down coverage buffer of
/// width * rows bytes, dropping any inter-row padding. Row y starts at
/// buffer + y*pitch for either pitch sign, so the pointer walk handles an
/// upward row flow too. Assumes one coverage byte per pixel, so the caller
/// must confirm FT_PIXEL_MODE_GRAY first; other modes pack pixels differently
/// and would make the width-byte row read run past the row.
fn copy_coverage(bitmap: &FtBitmap) -> ArrayVec<u8, GLYPH_MAX> {
    let mut out: ArrayVec<u8, GLYPH_MAX> = ArrayVec::new();
    let width = bitmap.width as usize;
    let rows = bitmap.rows as usize;
    let total = rows * width;
    if width == 0 || rows == 0 || bitmap.buffer.is_null() || total > GLYPH_MAX {
        return out;
    }
    // Establish the length; every byte is overwritten by the row copies below.
    for _ in 0..total {
        let _ = out.push(0);
    }
    let pitch = bitmap.pitch as isize;
    for y in 0..rows {
        // SAFETY: FreeType guarantees |pitch| >= width bytes per row, so the
        // width-byte read starting at buffer + y*pitch stays within that row.
        // out is a distinct buffer, disjoint from the source.
        unsafe {
            let src = bitmap.buffer.offset(y as isize * pitch);
            ptr::copy_nonoverlapping(src, out[y * width..].as_mut_ptr(), width);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test-font.ttf");

    fn load_face() -> Face {
        // mmap the fixture the same way the real loader does, then hand the
        // mapping to the Face (which owns and unmaps it).
        let cp = crate::platform::fs::cpath(FIXTURE).expect("cpath");
        let st = crate::platform::syscall::newfstatat(crate::platform::syscall::AT_FDCWD, &cp, 0)
            .expect("stat fixture");
        let len = st.st_size as usize;
        let fd = crate::platform::syscall::openat(
            crate::platform::syscall::AT_FDCWD,
            &cp,
            crate::platform::syscall::O_RDONLY,
            0,
        );
        assert!(fd >= 0, "open fixture");
        let ptr = unsafe {
            crate::platform::syscall::mmap(
                core::ptr::null_mut(),
                len,
                crate::platform::syscall::PROT_READ,
                crate::platform::syscall::MAP_PRIVATE,
                fd,
                0,
            )
        };
        assert!(!crate::platform::syscall::mmap_failed(ptr), "mmap fixture");
        unsafe { crate::platform::syscall::close(fd) };
        Face::from_mmap(ptr, len).expect("build face from fixture")
    }

    #[test]
    fn loads_fixture() {
        let _face = load_face();
    }

    #[test]
    fn copy_coverage_packs_rows_dropping_pitch_padding() {
        // 2x2 glyph stored with a positive pitch of 3: a padding byte trails
        // each row and must not survive the copy.
        let buf = [1u8, 2, 9, 3, 4, 9];
        let bitmap = FtBitmap {
            rows: 2,
            width: 2,
            pitch: 3,
            buffer: buf.as_ptr(),
            num_grays: 256,
            pixel_mode: FT_PIXEL_MODE_GRAY,
            palette_mode: 0,
            palette: ptr::null_mut(),
        };
        assert_eq!(copy_coverage(&bitmap).as_slice(), &[1u8, 2, 3, 4]);
    }

    #[test]
    fn copy_coverage_follows_negative_pitch_upward() {
        // Negative pitch: buffer points at the top row and earlier rows lie at
        // lower addresses. Memory is [row1, row0]; buffer points at row0.
        let buf = [3u8, 4, 1, 2];
        let bitmap = FtBitmap {
            rows: 2,
            width: 2,
            pitch: -2,
            // SAFETY: in-bounds offset to the second row of a 4-byte array.
            buffer: unsafe { buf.as_ptr().add(2) },
            num_grays: 256,
            pixel_mode: FT_PIXEL_MODE_GRAY,
            palette_mode: 0,
            palette: ptr::null_mut(),
        };
        assert_eq!(copy_coverage(&bitmap).as_slice(), &[1u8, 2, 3, 4]);
    }

    #[test]
    fn rasterize_visible_glyph_has_ink() {
        let face = load_face();
        face.set_pixel_size(16).expect("set size");
        let glyph = face.rasterize('A');
        assert!(glyph.rows > 0, "A should have rows");
        assert!(glyph.width > 0, "A should have width");
        assert_eq!(glyph.coverage.len(), glyph.rows * glyph.width);
        assert!(
            glyph.coverage.iter().any(|&c| c > 0),
            "A should have at least one inked pixel"
        );
        assert!(glyph.advance > 0.0);
    }

    #[test]
    fn rasterize_space_is_blank_but_advances() {
        let face = load_face();
        face.set_pixel_size(16).expect("set size");
        let glyph = face.rasterize(' ');
        assert_eq!(glyph.rows, 0, "space has no ink");
        assert_eq!(glyph.width, 0, "space has no ink");
        assert!(glyph.coverage.is_empty());
        assert!(glyph.advance > 0.0, "space still advances the pen");
    }

    #[test]
    fn line_metrics_have_expected_signs() {
        let face = load_face();
        face.set_pixel_size(16).expect("set size");
        let (ascent, descent) = face.line_metrics();
        assert!(ascent > 0.0, "ascent should be positive");
        assert!(descent < 0.0, "descent should be negative");
    }

    #[test]
    fn advance_is_positive_for_letters() {
        // The pen advance comes back with the rasterized glyph, which is what
        // both the blit and the caret read it from.
        let face = load_face();
        face.set_pixel_size(16).expect("set size");
        assert!(face.rasterize('A').advance > 0.0);
        assert!(face.rasterize('m').advance > 0.0);
    }
}
