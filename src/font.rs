//! Font rendering on top of FreeType.
//!
//! Loads a system TrueType/OpenType font and renders anti-aliased text with
//! alpha blending onto the pixel buffer. Glyph rasterization and metrics come
//! from freetype.rs; this module owns font discovery, layout, and the blit.

use core::cell::RefCell;

use crate::platform::arena::ArrayVec;
use crate::platform::error::{Error, Result};
use crate::platform::freetype;
use crate::platform::fs::ScanPath;
use crate::platform::syscall::{self, Fd, AT_FDCWD, DT_DIR, DT_LNK, DT_REG, O_RDONLY};
use crate::platform::{env, fs};
use crate::shm;

/// Font sizes for UI elements.
pub const INPUT_SIZE: f32 = 20.0;
pub const NAME_SIZE: f32 = 16.0;
pub const SUBTITLE_SIZE: f32 = 14.0;

/// Slots in the glyph cache. The launcher's repertoire is the characters of a
/// few dozen application names at three sizes, so a direct-mapped table this
/// size collides rarely and a collision only costs a re-rasterization.
const GLYPH_SLOTS: usize = 512;

/// Coverage bytes held for all cached glyphs together. At the sizes drawn here
/// a glyph's ink is a few hundred bytes, so this holds the whole working set
/// several times over. When it does fill, glyphs are still drawn, just not
/// cached.
const INK_CAP: usize = 256 * 1024;

/// A glyph that has been rasterized once: its metrics, and where its coverage
/// sits in the ink arena.
#[derive(Clone, Copy)]
struct Cached {
    ch: char,
    /// The pixel size it was rasterized at. Part of the key: the same character
    /// at two sizes is two glyphs.
    px: u32,
    left: i32,
    top: i32,
    width: usize,
    rows: usize,
    advance: f32,
    ink_at: usize,
    ink_len: usize,
}

/// Rasterized glyphs, kept for the life of the font.
///
/// Rasterizing means decomposing an outline and filling scanlines, and every
/// frame drew the same handful of characters afresh. A launcher opens, shows a
/// few names, and closes, so its glyph repertoire is tiny and fixed: the cache
/// fills during the first frame and every frame after it is hits.
///
/// Measurement reads the same entries as drawing, which is the point of putting
/// the advance in the cache rather than beside it. If the caret asked FreeType
/// for an advance while the blit used a cached one, the two could disagree, and
/// the caret would drift away from the text it is supposed to sit in.
struct GlyphCache {
    slots: [Option<Cached>; GLYPH_SLOTS],
    ink: ArrayVec<u8, INK_CAP>,
}

impl GlyphCache {
    fn new() -> Self {
        Self {
            slots: [None; GLYPH_SLOTS],
            ink: ArrayVec::new(),
        }
    }

    /// Which slot a glyph belongs in. Characters used together (a run of ASCII)
    /// have to land in different slots, so the size is folded in with an odd
    /// multiplier rather than added.
    fn slot(ch: char, px: u32) -> usize {
        let key = (ch as u32)
            .wrapping_mul(31)
            .wrapping_add(px.wrapping_mul(2_654_435_761));
        (key as usize) % GLYPH_SLOTS
    }

    /// The cached glyph, rasterizing it first if this is its first sighting.
    /// None when the face cannot produce it.
    fn get(&mut self, face: &freetype::Face, ch: char, px: u32) -> Option<Cached> {
        let slot = Self::slot(ch, px);
        if let Some(hit) = self.slots[slot] {
            if hit.ch == ch && hit.px == px {
                return Some(hit);
            }
        }

        // A miss, or another glyph sitting in this slot. Rasterize, and take the
        // slot over: two characters that collide here are rare, and the loser is
        // simply rasterized again next time it is drawn.
        face.set_pixel_size(px).ok()?;
        let glyph = face.rasterize(ch);

        let ink_at = self.ink.len();
        if self.ink.extend_from_slice(&glyph.coverage).is_err() {
            // The arena is full. The glyph is still usable, it just cannot be
            // kept, so hand it back without caching it.
            self.ink.truncate(ink_at);
            // No ink means no blit, so it must claim no size either. The
            // advance is still right, so the line lays out as it should and the
            // glyph is merely blank.
            return Some(Cached {
                ch,
                px,
                left: glyph.left,
                top: glyph.top,
                width: 0,
                rows: 0,
                advance: glyph.advance,
                ink_at,
                ink_len: 0,
            });
        }

        let cached = Cached {
            ch,
            px,
            left: glyph.left,
            top: glyph.top,
            width: glyph.width,
            rows: glyph.rows,
            advance: glyph.advance,
            ink_at,
            ink_len: glyph.coverage.len(),
        };
        self.slots[slot] = Some(cached);
        Some(cached)
    }

    fn coverage(&self, g: &Cached) -> &[u8] {
        &self.ink[g.ink_at..g.ink_at + g.ink_len]
    }
}

/// Loaded font for text rendering.
pub struct Font {
    face: freetype::Face,
    /// Rasterized glyphs. Behind a cell because drawing takes the font by shared
    /// reference: filling the cache is not a change anyone can observe.
    cache: RefCell<GlyphCache>,
}

/// Preferred font filenames, most wanted first. A file's position here is its
/// rank, and the best-ranked one found anywhere is the one loaded.
const PREFERRED_FONTS: [&str; 12] = [
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

/// The fixed roots. Those that depend on the environment are added by font_roots.
const FONT_DIRS: [&str; 2] = ["/usr/share/fonts", "/usr/local/share/fonts"];

/// The host's font trees, bound into every Flatpak sandbox whatever its
/// permissions. The second is the user's own, so HOME is not walked beside it.
const HOST_FONT_DIRS: [&str; 2] = ["/run/host/fonts", "/run/host/user-fonts"];

/// The two system directories, plus either the user's own tree or the host's
/// two. The cases never both apply.
const MAX_FONT_ROOTS: usize = 4;

/// Paths kept per preferred name. The same name turns up under more than one
/// root, and distributions ship one font under two paths, so a name keeps a few
/// candidates: whether a file actually loads is not known until FreeType is
/// handed it, and the ones behind it have to still be there when it does not.
const PATHS_PER_NAME: usize = 3;

/// The paths found for each preferred name, indexed by that name's rank. A name
/// has its own slots, so no amount of one font crowds out another, and within a
/// slot the paths sit in the order the directories were walked.
type FontHits = [ArrayVec<ScanPath, PATHS_PER_NAME>; PREFERRED_FONTS.len()];

/// The trees to walk, highest priority first. Inside a Flatpak sandbox
/// /usr/share/fonts holds the runtime's own few fonts, so the host's go first
/// and the launcher renders in the session's face.
fn font_roots(in_flatpak: bool, home: Option<&str>) -> ArrayVec<ScanPath, MAX_FONT_ROOTS> {
    let mut roots: ArrayVec<ScanPath, MAX_FONT_ROOTS> = ArrayVec::new();
    let mut push = |dir: &str| {
        let mut path = ScanPath::new();
        if path.push_str(dir).is_ok() {
            let _ = roots.push(path);
        }
    };
    if in_flatpak {
        for dir in HOST_FONT_DIRS {
            push(dir);
        }
    }
    for dir in FONT_DIRS {
        push(dir);
    }
    // Inside a sandbox the user's tree is /run/host/user-fonts, walked above.
    if !in_flatpak {
        if let Some(user_fonts) = home.and_then(|h| fs::join(h, ".local/share/fonts")) {
            let _ = roots.push(user_fonts);
        }
    }
    roots
}

impl Font {
    /// Load the best available system font.
    pub fn load() -> Result<Self> {
        // Each directory tree is walked once, checking every file against the
        // whole preference list. A font tree runs to hundreds of files across
        // dozens of subdirectories, so walking it per name is the expensive way
        // to ask the same question.
        //
        // The walk does not stop at the first top choice. Whether a file is a
        // font this can render is only known once FreeType has parsed it and
        // every UI size has been set on it, which happens below; a corrupt or
        // bitmap-only DejaVuSans.ttf must leave the fallbacks still collected.
        let mut hits: FontHits = core::array::from_fn(|_| ArrayVec::new());
        for dir in font_roots(env::in_flatpak(), env::var("HOME")).iter() {
            if fs::is_dir(dir) {
                collect_fonts(dir, &mut hits);
            }
        }

        // Preference order, and within a name the order the roots were walked.
        // A file that does not load falls through to the next candidate, so one
        // broken font cannot leave the launcher with none.
        for slot in hits.iter() {
            for path in slot.iter() {
                if let Ok(font) = Self::from_path(path) {
                    return Ok(font);
                }
            }
        }

        Err(Error::msg(
            "no suitable system font found (install dejavu, noto, or liberation fonts)",
        ))
    }

    pub(crate) fn from_path(path: &str) -> Result<Self> {
        let (ptr, len) = mmap_file(path)?;
        // Face takes ownership of the mapping and unmaps it on drop.
        let face = freetype::Face::from_mmap(ptr, len)?;
        let font = Self {
            face,
            cache: RefCell::new(GlyphCache::new()),
        };
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
        let px = pixel_size(size);
        // from_path validated every UI size, so this cannot fail for the sizes
        // the caller passes; the same holds at the other set_pixel_size sites.
        let _ = self.face.set_pixel_size(px);
        let (ascent, _descent) = self.face.line_metrics();

        let baseline_y = y as f32 + ascent;
        let mut cursor_x = x as f32;
        let max_x = (x + max_width) as f32;
        let clip_x = max_x as i32;

        let mut cache = self.cache.borrow_mut();
        for ch in text.chars() {
            let Some(glyph) = cache.get(&self.face, ch, px) else {
                continue;
            };

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
                cache.coverage(&glyph),
                glyph.width,
                glyph.rows,
                color,
            );

            cursor_x += glyph.advance;
        }

        (cursor_x - x as f32) as u32
    }

    /// The pen advance of a character, from the same cache the blit draws from.
    fn advance(&self, ch: char, px: u32) -> f32 {
        self.cache
            .borrow_mut()
            .get(&self.face, ch, px)
            .map(|g| g.advance)
            .unwrap_or(0.0)
    }

    /// Visual text height (ascent to descent) for a given font size.
    pub fn text_height(&self, size: f32) -> f32 {
        let _ = self.face.set_pixel_size(pixel_size(size));
        let (ascent, descent) = self.face.line_metrics();
        ascent - descent
    }

    /// Pixel x position of the caret at a given char offset.
    pub fn x_at_char_offset(&self, text: &str, char_offset: usize, size: f32) -> f32 {
        let px = pixel_size(size);
        text.chars()
            .take(char_offset)
            .map(|ch| self.advance(ch, px))
            .sum()
    }

    /// Find the char offset closest to a pixel x position (for mouse hit-testing).
    pub fn char_offset_at_x(&self, text: &str, x: f32, size: f32) -> usize {
        let px = pixel_size(size);
        let mut acc = 0.0f32;
        for (i, ch) in text.chars().enumerate() {
            let advance = self.advance(ch, px);
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
    // The coverage is one byte per pixel, row-major and tightly packed. A glyph
    // whose bitmap is short of that is not drawable; the cache only ever hands
    // out a full one, so this is a guard, not a case.
    if glyph_width == 0 || glyph_height == 0 || bitmap.len() < glyph_width * glyph_height {
        return;
    }

    // Most of a glyph is fully covered; an opaque color needs no blend there.
    let opaque = color >> 24 == 0xFF;

    // Clip once for the whole glyph rather than testing every pixel against
    // four bounds. What is left is the rectangle of the glyph that lands inside
    // the buffer and inside the text box, and every pixel in it is in range.
    let right = (buf_width as i32).min(clip_x);
    let col_start = (-gx).max(0) as usize;
    let col_end = ((right - gx).max(0) as usize).min(glyph_width);
    let row_start = (-gy).max(0) as usize;
    let row_end = ((buf_height as i32 - gy).max(0) as usize).min(glyph_height);
    if col_start >= col_end || row_start >= row_end {
        return;
    }

    for row in row_start..row_end {
        let coverage_row = &bitmap[row * glyph_width + col_start..row * glyph_width + col_end];
        let line = (gy + row as i32) as usize * buf_width as usize;
        let left = line + (gx + col_start as i32) as usize;

        for (i, &coverage) in coverage_row.iter().enumerate() {
            if coverage == 0 {
                continue;
            }
            let offset = left + i;
            pixels[offset] = if coverage == u8::MAX && opaque {
                color
            } else {
                shm::blend(pixels[offset], color, coverage)
            };
        }
    }
}

/// Walk a directory tree, recording every preferred name it holds against that
/// name's rank. A symlinked directory is not descended into, which breaks any
/// symlink cycle. A name whose slots are already full keeps the paths it has,
/// which are the ones found earliest and so in the highest-priority root.
fn collect_fonts(dir: &str, hits: &mut FontHits) {
    let Ok(mut rd) = fs::ReadDir::open(dir) else {
        return;
    };
    let _ = rd.for_each(|name, d_type| {
        let Some(path) = fs::join(dir, name) else {
            return;
        };
        let is_directory = match d_type {
            DT_DIR => true,
            DT_REG | DT_LNK => false,
            _ => fs::is_dir_nofollow(&path),
        };
        if is_directory {
            collect_fonts(&path, hits);
        } else if let Some(rank) = PREFERRED_FONTS.iter().position(|f| *f == name) {
            let _ = hits[rank].push(path);
        }
    });
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

    /// The preference list is a ranking, not a search order. One walk of the
    /// tree checks every file against the whole list, so the top choice wins
    /// however deep it sits and a lesser name at the root does not shadow it.
    #[test]
    fn font_discovery_ranks_by_preference_not_by_depth() {
        let base = format!(
            "{}/bnk_fonts_{}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(format!("{base}/sys/vendor/opentype")).expect("dirs");
        std::fs::create_dir_all(format!("{base}/user")).expect("dirs");
        let last = PREFERRED_FONTS[PREFERRED_FONTS.len() - 1];
        // The top choice sits two levels down in the first root; the last choice
        // has a root of its own, walked second. Splitting them across roots is
        // what makes the second assertion independent of the order getdents
        // happens to return names in. Only the filenames matter to the walk, so
        // these need no font bytes.
        std::fs::write(
            format!("{base}/sys/vendor/opentype/{}", PREFERRED_FONTS[0]),
            b"",
        )
        .expect("write first choice");
        std::fs::write(format!("{base}/user/{last}"), b"").expect("write last choice");

        let mut hits: FontHits = core::array::from_fn(|_| ArrayVec::new());
        collect_fonts(&format!("{base}/sys"), &mut hits);
        collect_fonts(&format!("{base}/user"), &mut hits);
        assert!(
            hits[0]
                .first()
                .is_some_and(|p| p.ends_with(PREFERRED_FONTS[0])),
            "the top choice should be found two directories down"
        );
        // Finding the top choice must not end the walk: the lesser name in the
        // next root is still recorded, because whether the top choice actually
        // loads is not known until FreeType has been handed it.
        assert!(
            hits[PREFERRED_FONTS.len() - 1]
                .first()
                .is_some_and(|p| p.ends_with(last)),
            "a later root should still be collected behind the top choice"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A broken top choice must not cost the launcher its font. The first
    /// candidate here is a file that is not a font at all; loading has to fall
    /// through to the real one behind it.
    #[test]
    fn a_top_choice_that_does_not_load_falls_through_to_the_next() {
        let base = format!(
            "{}/bnk_fonts_broken_{}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(format!("{base}/sys")).expect("dir");
        std::fs::create_dir_all(format!("{base}/user")).expect("dir");
        // Rank 0 is bytes FreeType will refuse, in the root walked first; rank 1
        // is the real fixture, in the root walked second. A scan that stopped at
        // the top choice would never reach the second root at all.
        std::fs::write(format!("{base}/sys/{}", PREFERRED_FONTS[0]), b"not a font")
            .expect("write broken");
        std::fs::copy(FIXTURE, format!("{base}/user/{}", PREFERRED_FONTS[1]))
            .expect("copy fixture");

        let mut hits: FontHits = core::array::from_fn(|_| ArrayVec::new());
        collect_fonts(&format!("{base}/sys"), &mut hits);
        collect_fonts(&format!("{base}/user"), &mut hits);
        assert_eq!(hits[0].len(), 1, "the broken top choice was collected");
        assert_eq!(hits[1].len(), 1, "and so was the one behind it");

        // The same order load() tries them in.
        let mut loaded = None;
        for slot in hits.iter() {
            for path in slot.iter() {
                if let Ok(font) = Font::from_path(path) {
                    loaded = Some(font);
                    break;
                }
            }
        }
        assert!(
            loaded.is_some(),
            "a broken first candidate must not sink the load"
        );

        let _ = std::fs::remove_dir_all(&base);
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
        let width = font.render_text(&mut pixels, w, h, 2, 2, "Hi", 0xFFFF_FFFF, w, NAME_SIZE);
        assert!(width > 0, "rendered width should be positive");
        assert!(
            pixels.iter().any(|&p| p != 0),
            "rendering should leave at least one blended pixel"
        );
    }

    /// Text is drawn with the same premultiplied compositing as everything else,
    /// so a color carrying alpha lands over what is under it rather than
    /// replacing it, and a color with no alpha at all covers nothing.
    #[test]
    fn render_text_composites_a_translucent_color() {
        let font = load_fixture_font();
        let (w, h) = (200u32, 40u32);
        let black = 0xFF00_0000;

        let mut pixels = vec![black; (w * h) as usize];
        font.render_text(&mut pixels, w, h, 2, 2, "Hi", 0x8080_8080, w, NAME_SIZE);
        assert!(
            pixels.iter().any(|&p| p != black),
            "half-alpha white should still mark the buffer"
        );
        // Half alpha over black tops out at half gray. A pixel brighter than
        // that would mean the color had been stamped rather than composited.
        assert!(
            pixels.iter().all(|&p| p >> 16 & 0xFF <= 0x80),
            "no channel should exceed the color's own alpha"
        );

        let mut pixels = vec![black; (w * h) as usize];
        font.render_text(&mut pixels, w, h, 2, 2, "Hi", 0x0000_0000, w, NAME_SIZE);
        assert!(
            pixels.iter().all(|&p| p == black),
            "a color with no alpha should leave the buffer alone"
        );
    }

    /// A glyph can land partly outside the buffer: the last one in a run reaches
    /// past the right edge, and a tall one at y=0 reaches above the top. The
    /// clipping is worked out once per glyph now, so an off-by-one there writes
    /// out of bounds rather than merely drawing wrong.
    #[test]
    fn a_glyph_hanging_off_the_edge_is_clipped_not_dropped() {
        let font = load_fixture_font();
        let (w, h) = (40u32, 24u32);

        // Text far wider than the buffer, drawn from the last few columns, so
        // every glyph but the first is entirely outside it.
        let mut pixels = vec![0u32; (w * h) as usize];
        let width = font.render_text(
            &mut pixels,
            w,
            h,
            w - 4,
            2,
            "WWWWWWWW",
            0xFFFF_FFFF,
            w * 4,
            NAME_SIZE,
        );
        assert!(width > 0);

        // Drawn at the very top, where a glyph's ink rises above the baseline
        // and out of the buffer.
        let mut pixels = vec![0u32; (w * h) as usize];
        font.render_text(&mut pixels, w, h, 0, 0, "Wg", 0xFFFF_FFFF, w, NAME_SIZE);

        // And with the text box ending before the buffer does, which is the case
        // the input field draws: the glyph fits the buffer but not the box.
        let mut pixels = vec![0u32; (w * h) as usize];
        font.render_text(&mut pixels, w, h, 0, 2, "WW", 0xFFFF_FFFF, 6, NAME_SIZE);
    }

    #[test]
    fn render_text_respects_max_width() {
        let font = load_fixture_font();
        let (w, h) = (200u32, 40u32);
        let mut full = vec![0u32; (w * h) as usize];
        let mut clipped = vec![0u32; (w * h) as usize];

        let full_w = font.render_text(&mut full, w, h, 0, 2, "WWWWWW", 0xFFFF_FFFF, w, NAME_SIZE);
        // A max_width well under the full extent must cut the run short.
        let narrow = full_w / 2;
        let clipped_w = font.render_text(
            &mut clipped,
            w,
            h,
            0,
            2,
            "WWWWWW",
            0xFFFF_FFFF,
            narrow,
            NAME_SIZE,
        );
        assert!(clipped_w <= narrow, "clipped width stays within max_width");
        assert!(clipped_w < full_w, "clipping should shorten the run");
    }

    fn roots(in_flatpak: bool, home: Option<&str>) -> Vec<String> {
        font_roots(in_flatpak, home)
            .iter()
            .map(|r| r.as_str().to_string())
            .collect()
    }

    #[test]
    fn font_roots_end_with_the_users_own_tree() {
        assert_eq!(
            roots(false, Some("/home/u")),
            [
                "/usr/share/fonts",
                "/usr/local/share/fonts",
                "/home/u/.local/share/fonts"
            ]
        );
    }

    #[test]
    fn font_roots_without_home_are_the_system_ones() {
        assert_eq!(
            roots(false, None),
            ["/usr/share/fonts", "/usr/local/share/fonts"]
        );
    }

    #[test]
    fn flatpak_font_roots_put_the_hosts_first_and_walk_home_once() {
        assert_eq!(
            roots(true, Some("/home/u")),
            [
                "/run/host/fonts",
                "/run/host/user-fonts",
                "/usr/share/fonts",
                "/usr/local/share/fonts"
            ]
        );
    }
}
