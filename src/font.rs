//! Font rendering on top of FreeType.
//!
//! Loads a system TrueType/OpenType font and renders anti-aliased text with
//! alpha blending onto the pixel buffer. Glyph rasterization and metrics come
//! from freetype.rs; this module owns font discovery, layout, and the blit.

use crate::platform::error::{Error, Result};
use crate::platform::freetype;
use crate::platform::syscall::{self, Fd, AT_FDCWD, DT_DIR, DT_LNK, DT_REG, O_RDONLY};
use crate::platform::{env, fs};

/// A scanned filesystem path.
type ScanPath = crate::platform::arena::ArrayString<{ fs::PATH_CAP }>;

/// Join two path segments with a single separator, or None if it does not fit.
fn join(base: &str, child: &str) -> Option<ScanPath> {
    let mut p = ScanPath::new();
    p.push_str(base).ok()?;
    if !base.ends_with('/') {
        p.push('/').ok()?;
    }
    p.push_str(child).ok()?;
    Some(p)
}

/// Font sizes for UI elements.
pub const INPUT_SIZE: f32 = 20.0;
pub const NAME_SIZE: f32 = 16.0;
pub const SUBTITLE_SIZE: f32 = 14.0;

/// Loaded font for text rendering.
pub struct Font {
    face: freetype::Face,
}

/// Preferred font filenames, searched in order.
const PREFERRED_FONTS: &[&str] = &[
    "DejaVuSans.ttf",
    "NotoSans-Regular.ttf",
    "LiberationSans-Regular.ttf",
    "Inter-Regular.ttf",
    "Inter-Regular.otf",
    "Cantarell-Regular.otf",
    "Cantarell-VF.otf",
    "Ubuntu-R.ttf",
    "Roboto-Regular.ttf",
    "SourceSans3-Regular.otf",
    "SourceSansPro-Regular.otf",
    "SourceSansPro-Regular.ttf",
];

/// Directories to search for fonts.
const FONT_DIRS: &[&str] = &["/usr/share/fonts", "/usr/local/share/fonts"];

impl Font {
    /// Load the best available system font.
    pub fn load() -> Result<Self> {
        for name in PREFERRED_FONTS {
            for dir in FONT_DIRS {
                if !fs::is_dir(dir) {
                    continue;
                }
                if let Some(path) = find_font_file(dir, name) {
                    if let Ok(font) = Self::from_path(&path) {
                        return Ok(font);
                    }
                }
            }
        }

        // Check user font directory.
        if let Some(user_fonts) = env::var("HOME").and_then(|h| join(h, ".local/share/fonts")) {
            if fs::is_dir(&user_fonts) {
                for name in PREFERRED_FONTS {
                    if let Some(path) = find_font_file(&user_fonts, name) {
                        if let Ok(font) = Self::from_path(&path) {
                            return Ok(font);
                        }
                    }
                }
            }
        }

        Err(Error::msg(
            "no suitable system font found (install dejavu, noto, or liberation fonts)",
        ))
    }

    fn from_path(path: &str) -> Result<Self> {
        let (ptr, len) = mmap_file(path)?;
        // Face takes ownership of the mapping and unmaps it on drop.
        let face = freetype::Face::from_mmap(ptr, len)?;
        let font = Self { face };
        // Confirm the face can be set to every UI size now. The layout and
        // render paths set the size but cannot surface a failure, so a face
        // that rejects one of these sizes (a bitmap strike font missing it) is
        // skipped here rather than later rendering at a stale, wrong size.
        for size in [INPUT_SIZE, NAME_SIZE, SUBTITLE_SIZE] {
            font.face.set_pixel_size(pixel_size(size))?;
        }
        Ok(font)
    }

    /// Render text onto a pixel buffer with alpha blending.
    /// y is the top of the text line. Returns the rendered width in pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn render_text(
        &self,
        pixels: &mut [u32],
        buf_width: u32,
        buf_height: u32,
        x: u32,
        y: u32,
        text: &str,
        color: u32,
        max_width: u32,
        size: f32,
    ) -> u32 {
        // from_path validated every UI size, so this cannot fail for the sizes
        // the caller passes; the same holds at the other set_pixel_size sites.
        let _ = self.face.set_pixel_size(pixel_size(size));
        let (ascent, _descent) = self.face.line_metrics();

        let baseline_y = y as f32 + ascent;
        let mut cursor_x = x as f32;
        let max_x = (x + max_width) as f32;
        let clip_x = max_x as i32;

        for ch in text.chars() {
            let glyph = self.face.rasterize(ch);

            if cursor_x + glyph.advance > max_x {
                break;
            }

            let gx = cursor_x as i32 + glyph.left;
            let gy = baseline_y as i32 - glyph.top;

            // Clip to the text box: a glyph can fit by advance yet have ink
            // (italics, overhangs) reaching past it, which would bleed out.
            blit_coverage(
                pixels,
                buf_width,
                buf_height,
                clip_x,
                gx,
                gy,
                &glyph.coverage,
                glyph.width,
                glyph.rows,
                color,
            );

            cursor_x += glyph.advance;
        }

        (cursor_x - x as f32) as u32
    }

    /// Visual text height (ascent to descent) for a given font size.
    pub fn text_height(&self, size: f32) -> f32 {
        let _ = self.face.set_pixel_size(pixel_size(size));
        let (ascent, descent) = self.face.line_metrics();
        ascent - descent
    }

    /// Pixel x position of the cursor at a given char offset.
    pub fn x_at_char_offset(&self, text: &str, char_offset: usize, size: f32) -> f32 {
        let _ = self.face.set_pixel_size(pixel_size(size));
        text.chars()
            .take(char_offset)
            .map(|ch| self.face.advance(ch))
            .sum()
    }

    /// Find the char offset closest to a pixel x position (for mouse hit-testing).
    pub fn char_offset_at_x(&self, text: &str, x: f32, size: f32) -> usize {
        let _ = self.face.set_pixel_size(pixel_size(size));
        let mut acc = 0.0f32;
        for (i, ch) in text.chars().enumerate() {
            let advance = self.face.advance(ch);
            if x < acc + advance / 2.0 {
                return i;
            }
            acc += advance;
        }
        text.chars().count()
    }
}

/// Map a UI font size to a FreeType pixel size. The sizes here are whole
/// numbers, so the half-up cast is an exact rounding; clamp to at least 1.
fn pixel_size(size: f32) -> u32 {
    let rounded = (size + 0.5) as u32;
    rounded.max(1)
}

/// Blit a coverage bitmap onto the pixel buffer with alpha blending.
#[allow(clippy::too_many_arguments)]
fn blit_coverage(
    pixels: &mut [u32],
    buf_width: u32,
    buf_height: u32,
    clip_x: i32,
    gx: i32,
    gy: i32,
    bitmap: &[u8],
    glyph_width: usize,
    glyph_height: usize,
    color: u32,
) {
    for row in 0..glyph_height {
        let py = gy + row as i32;
        if py < 0 || py >= buf_height as i32 {
            continue;
        }

        for col in 0..glyph_width {
            let Some(&coverage) = bitmap.get(row * glyph_width + col) else {
                continue;
            };
            if coverage == 0 {
                continue;
            }

            let px = gx + col as i32;
            if px < 0 || px >= buf_width as i32 || px >= clip_x {
                continue;
            }

            let offset = (py as u32 * buf_width + px as u32) as usize;
            if offset < pixels.len() {
                pixels[offset] = blend(pixels[offset], color, coverage);
            }
        }
    }
}

/// Alpha-blend foreground color over background using glyph coverage.
fn blend(bg: u32, fg: u32, coverage: u8) -> u32 {
    let a = coverage as u32;
    let inv = 255 - a;

    let r = ((fg >> 16 & 0xFF) * a + (bg >> 16 & 0xFF) * inv) / 255;
    let g = ((fg >> 8 & 0xFF) * a + (bg >> 8 & 0xFF) * inv) / 255;
    let b = ((fg & 0xFF) * a + (bg & 0xFF) * inv) / 255;
    let out_a = a + ((bg >> 24 & 0xFF) * inv) / 255;

    (out_a << 24) | (r << 16) | (g << 8) | b
}

/// Recursively search a directory for a font file by name. A symlinked
/// directory is not descended into, which breaks any symlink cycle.
fn find_font_file(dir: &str, target_name: &str) -> Option<ScanPath> {
    let mut rd = fs::ReadDir::open(dir).ok()?;
    let mut found: Option<ScanPath> = None;
    let _ = rd.for_each(|name, d_type| {
        if found.is_some() {
            return;
        }
        let Some(path) = join(dir, name) else {
            return;
        };
        let is_directory = match d_type {
            DT_DIR => true,
            DT_REG | DT_LNK => false,
            _ => fs::is_dir_nofollow(&path),
        };
        if is_directory {
            found = find_font_file(&path, target_name);
        } else if name == target_name {
            found = Some(path);
        }
    });
    found
}

/// Map a font file read-only and return the mapping pointer and length. Font
/// files are large and FreeType reads from the bytes for the Face's life, so a
/// mapping (stable address) is the right backing rather than a moved buffer.
fn mmap_file(path: &str) -> Result<(*mut core::ffi::c_void, usize)> {
    let cp = fs::cpath(path)?;
    let st = syscall::newfstatat(AT_FDCWD, &cp, 0)
        .ok_or_else(|| Error::msg("failed to stat font file"))?;
    let len = st.st_size as usize;
    if len == 0 {
        return Err(Error::msg("empty font file"));
    }

    let fd = syscall::openat(AT_FDCWD, &cp, O_RDONLY, 0);
    if fd < 0 {
        return Err(Error::from_errno(-fd));
    }
    let fd = Fd::new(fd);
    // SAFETY: a read-only private mapping of the whole file; fd is open.
    let ptr = unsafe {
        syscall::mmap(
            core::ptr::null_mut(),
            len,
            syscall::PROT_READ,
            syscall::MAP_PRIVATE,
            fd.as_raw_fd(),
            0,
        )
    };
    if syscall::mmap_failed(ptr) {
        return Err(Error::from_errno(syscall::mmap_errno(ptr)));
    }
    // The fd closes when fd drops; the mapping keeps its pages.
    Ok((ptr, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test-font.ttf");

    fn load_fixture_font() -> Font {
        Font::from_path(FIXTURE).expect("load fixture font")
    }

    #[test]
    fn text_height_is_positive() {
        let font = load_fixture_font();
        assert!(font.text_height(INPUT_SIZE) > 0.0);
    }

    #[test]
    fn cursor_x_grows_with_offset() {
        let font = load_fixture_font();
        let text = "Settings";
        let mut last = -1.0;
        for offset in 0..=text.chars().count() {
            let x = font.x_at_char_offset(text, offset, INPUT_SIZE);
            assert!(x > last, "x at offset {offset} should grow ({x} <= {last})");
            last = x;
        }
    }

    #[test]
    fn char_offset_round_trips_to_ends() {
        let font = load_fixture_font();
        let text = "Files";
        let count = text.chars().count();
        assert_eq!(font.char_offset_at_x(text, -5.0, INPUT_SIZE), 0);
        let full = font.x_at_char_offset(text, count, INPUT_SIZE);
        assert_eq!(font.char_offset_at_x(text, full + 50.0, INPUT_SIZE), count);
    }

    #[test]
    fn render_text_blends_pixels_and_reports_width() {
        let font = load_fixture_font();
        let (w, h) = (200u32, 40u32);
        let mut pixels = vec![0u32; (w * h) as usize];
        let width = font.render_text(&mut pixels, w, h, 2, 2, "Hi", 0x00FF_FFFF, w, NAME_SIZE);
        assert!(width > 0, "rendered width should be positive");
        assert!(
            pixels.iter().any(|&p| p != 0),
            "rendering should leave at least one blended pixel"
        );
    }

    #[test]
    fn render_text_respects_max_width() {
        let font = load_fixture_font();
        let (w, h) = (200u32, 40u32);
        let mut full = vec![0u32; (w * h) as usize];
        let mut clipped = vec![0u32; (w * h) as usize];

        let full_w = font.render_text(&mut full, w, h, 0, 2, "WWWWWW", 0x00FF_FFFF, w, NAME_SIZE);
        // A max_width well under the full extent must cut the run short.
        let narrow = full_w / 2;
        let clipped_w = font.render_text(
            &mut clipped,
            w,
            h,
            0,
            2,
            "WWWWWW",
            0x00FF_FFFF,
            narrow,
            NAME_SIZE,
        );
        assert!(clipped_w <= narrow, "clipped width stays within max_width");
        assert!(clipped_w < full_w, "clipping should shorten the run");
    }
}
