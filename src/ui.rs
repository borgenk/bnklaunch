//! Software rendering of the launcher: theme, layout, and the draw routines
//! that composite the input field and result rows into the pixel buffer.

use crate::app::{normalize_url, parse_action, AppState, InputAction};
use crate::desktop::{self, DesktopEntry};
use crate::launch::URL_CAP;
use crate::platform::arena;
use crate::{font, shm};

/// A color as the buffer holds it: premultiplied ARGB8888, the color channels
/// already scaled by the alpha, so compositing is a multiply and an add.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Color(u32);

impl Color {
    /// A color from straight channels, as the config file spells one.
    pub(crate) const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        // Rounded to nearest, so an opaque color comes out exactly as written.
        let a = a as u32;
        let r = (r as u32 * a + 127) / 255;
        let g = (g as u32 * a + 127) / 255;
        let b = (b as u32 * a + 127) / 255;
        Self((a << 24) | (r << 16) | (g << 8) | b)
    }

    /// The packed word, for the buffer.
    pub(crate) const fn argb(self) -> u32 {
        self.0
    }
}

/// How the window looks. A dark palette by default: one background shade the
/// desktop shows a little through, and the selected row is the only part that
/// lifts off it. A color carrying alpha composites over what is under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Theme {
    /// The window's fill; its alpha is the window's own transparency.
    pub background: Color,
    /// Input text and result titles.
    pub text: Color,
    /// The second line of a result row.
    pub subtitle: Color,
    /// The wash marking the selected result row.
    pub highlight: Color,
    /// The blinking caret in the input field.
    pub caret: Color,
    /// Behind selected input text.
    pub selection: Color,
}

impl Theme {
    pub(crate) const DEFAULT: Self = Self {
        background: Color::rgba(0x1f, 0x21, 0x24, 0xf0),
        text: Color::rgba(0xec, 0xec, 0xec, 0xff),
        subtitle: Color::rgba(0x88, 0x88, 0x88, 0xff),
        highlight: Color::rgba(0xff, 0xff, 0xff, 0x0d),
        caret: Color::rgba(0x6f, 0xd3, 0xef, 0xff),
        selection: Color::rgba(0x2f, 0x7b, 0xa6, 0xff),
    };
}

impl Default for Theme {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Width of the launcher window in pixels.
pub(crate) const WINDOW_WIDTH: u32 = 800;

/// Result rows drawn under the input. The window grows and shrinks with the
/// number of rows, up to this many.
pub(crate) const MAX_RESULTS: usize = 5;

/// Input field height
const INPUT_BOX_H: u32 = 54;

/// X position where text starts: the inner padding, in from the window's left
/// edge.
pub(crate) const TEXT_X: u32 = 15;

/// Right edge of every text run, as a width from TEXT_X. The trailing gutter is
/// the wider of the two, so a name that fills the row stops well short of the
/// window edge.
pub(crate) const TEXT_W: u32 = WINDOW_WIDTH - TEXT_X - 115;

/// Y position where results start, flush under the input field
const RESULTS_START_Y: u32 = INPUT_BOX_H;

/// Height of each result item
const RESULT_HEIGHT: u32 = 50;

/// Window height for a number of result rows: the input field plus a row each,
/// up to MAX_RESULTS.
pub(crate) fn calculate_height(num_results: usize) -> u32 {
    let rows = num_results.min(MAX_RESULTS) as u32;
    INPUT_BOX_H + rows * RESULT_HEIGHT
}

/// The result row a pointer position falls in, given how many rows are drawn.
/// None for a position above the list, past the last row, or outside the window
/// horizontally. The same arithmetic draw_ui lays the rows out with, read
/// backwards, so the two cannot disagree about where a row is.
pub(crate) fn row_at(x: f64, y: f64, num_results: usize) -> Option<usize> {
    let rows = num_results.min(MAX_RESULTS);
    if x < 0.0 || x >= WINDOW_WIDTH as f64 {
        return None;
    }
    let offset = y - RESULTS_START_Y as f64;
    if offset < 0.0 {
        return None;
    }
    let row = (offset / RESULT_HEIGHT as f64) as usize;
    (row < rows).then_some(row)
}

/// Whether a pointer position is in the input field.
pub(crate) fn input_box_contains(x: f64, y: f64) -> bool {
    y >= 0.0 && y < INPUT_BOX_H as f64 && x >= TEXT_X as f64 && x < WINDOW_WIDTH as f64
}

/// What the caret and the selection look like this frame.
pub(crate) struct Caret {
    /// Char offset of the caret in the input text.
    pub offset: usize,
    /// The selected range, as char offsets.
    pub selection: Option<(usize, usize)>,
    /// The caret is solid for half its blink period and hidden for the other.
    pub visible: bool,
}

/// Draw the whole launcher into a pixel buffer.
///
/// It takes the buffer and the caret rather than the client, so drawing a frame
/// needs no compositor: the same call renders into an offscreen buffer.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_ui(
    pixels: &mut shm::PixelBuffer,
    input_text: &str,
    state: &AppState,
    results: &[&DesktopEntry],
    font: &font::Font,
    search_enabled: bool,
    caret: &Caret,
    theme: &Theme,
) {
    let cursor = caret.offset;
    let selection = caret.selection;
    let cursor_visible = caret.visible;

    // A single flat surface; the selected row is the only part that washes.
    pixels.fill(theme.background.argb());

    // Vertically center text in the input box
    let text_h = font.text_height(font::INPUT_SIZE);
    let text_y = ((INPUT_BOX_H as f32 - text_h) / 2.0) as u32;
    let cursor_h = text_h as u32;

    // Selection highlight (drawn before text so text appears on top)
    if let Some((sel_start, sel_end)) = selection {
        let start_px = font.x_at_char_offset(input_text, sel_start, font::INPUT_SIZE);
        let end_px = font.x_at_char_offset(input_text, sel_end, font::INPUT_SIZE);
        let sel_width = (end_px - start_px) as u32;
        if sel_width > 0 {
            pixels.fill_rect(
                TEXT_X + start_px as u32,
                text_y,
                sel_width,
                cursor_h,
                theme.selection.argb(),
            );
        }
    }

    // Input text
    if !input_text.is_empty() {
        pixels.draw_text(
            TEXT_X,
            text_y,
            input_text,
            theme.text.argb(),
            TEXT_W,
            font,
            font::INPUT_SIZE,
        );
    }

    // Cursor (hidden on the blink-off phase)
    if cursor_visible {
        let cursor_px = font.x_at_char_offset(input_text, cursor, font::INPUT_SIZE);
        pixels.fill_rect(
            TEXT_X + cursor_px as u32,
            text_y,
            2,
            cursor_h,
            theme.caret.argb(),
        );
    }

    // Under the input goes either a prefix-action preview (s: / u:), which takes
    // precedence, or the app results.
    match parse_action(input_text, search_enabled) {
        InputAction::Search(q) if !q.is_empty() => {
            draw_preview_row(pixels, "Web search", q, font, theme);
        }
        InputAction::OpenUrl(u) if !u.is_empty() => {
            let mut url: arena::ArrayString<URL_CAP> = arena::ArrayString::new();
            normalize_url(u, &mut url);
            draw_preview_row(pixels, "Open URL", &url, font, theme);
        }
        _ => draw_results(pixels, state, results, font, theme),
    }
}

/// The color of the selected row: the wash over the background, worked out once.
/// Every pixel under the row is the background the frame started with, so
/// blending across the row lands on this same color.
fn wash(theme: &Theme) -> u32 {
    shm::blend(theme.background.argb(), theme.highlight.argb(), u8::MAX)
}

/// Draw the result rows. They butt up against each other with no gaps; only the
/// selected row gets a wash so it stands out from the body.
fn draw_results(
    pixels: &mut shm::PixelBuffer,
    state: &AppState,
    results: &[&DesktopEntry],
    font: &font::Font,
    theme: &Theme,
) {
    for (i, entry) in results.iter().take(MAX_RESULTS).enumerate() {
        let y = RESULTS_START_Y + (i as u32 * RESULT_HEIGHT);

        if i == state.selected {
            pixels.stamp_rect(0, y, WINDOW_WIDTH, RESULT_HEIGHT, wash(theme));
        }

        // The subtitle previews the resolved command with field codes dropped.
        let mut subtitle: arena::ArrayString<{ desktop::SUBTITLE_CAP }> = arena::ArrayString::new();
        desktop::exec_subtitle(&entry.exec, &mut subtitle);
        draw_row(pixels, y, &entry.name, &subtitle, font, theme);
    }
}

/// Draw a result row: title over subtitle, the two lines vertically centered as
/// a block in the row starting at y. Both lines stop at TEXT_W, so a long name
/// or command line cannot run off the row.
fn draw_row(
    pixels: &mut shm::PixelBuffer,
    y: u32,
    title: &str,
    subtitle: &str,
    font: &font::Font,
    theme: &Theme,
) {
    let title_h = font.text_height(font::NAME_SIZE);
    let subtitle_h = font.text_height(font::SUBTITLE_SIZE);
    let gap = 2.0;
    let block_h = title_h + gap + subtitle_h;
    let top = y as f32 + (RESULT_HEIGHT as f32 - block_h) / 2.0;

    pixels.draw_text(
        TEXT_X,
        top as u32,
        title,
        theme.text.argb(),
        TEXT_W,
        font,
        font::NAME_SIZE,
    );
    pixels.draw_text(
        TEXT_X,
        (top + title_h + gap) as u32,
        subtitle,
        theme.subtitle.argb(),
        TEXT_W,
        font,
        font::SUBTITLE_SIZE,
    );
}

/// Draw a single preview row for prefix actions (s: / u:).
fn draw_preview_row(
    pixels: &mut shm::PixelBuffer,
    title: &str,
    subtitle: &str,
    font: &font::Font,
    theme: &Theme,
) {
    let y = RESULTS_START_Y;
    pixels.stamp_rect(0, y, WINDOW_WIDTH, RESULT_HEIGHT, wash(theme));
    draw_row(pixels, y, title, subtitle, font, theme);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A point comfortably inside the window horizontally, which is what every
    /// vertical case below wants to hold still.
    const MID_X: f64 = (WINDOW_WIDTH / 2) as f64;

    /// A column just inside the window's right edge, past where any text run
    /// can reach, so a pixel read there is the window itself rather than a glyph.
    const CLEAR_X: u32 = WINDOW_WIDTH - 1;

    #[test]
    fn an_opaque_color_survives_premultiplication_unchanged() {
        // Truncating instead of rounding would dim every channel by a step here,
        // which is a color the user did not ask for.
        assert_eq!(Color::rgba(0xFF, 0xFF, 0xFF, 0xFF).argb(), 0xFFFF_FFFF);
        assert_eq!(Color::rgba(0x29, 0x2b, 0x30, 0xFF).argb(), 0xFF29_2B30);
        assert_eq!(Color::rgba(0x01, 0x7F, 0xFE, 0xFF).argb(), 0xFF01_7FFE);
    }

    #[test]
    fn alpha_scales_every_channel() {
        assert_eq!(Color::rgba(0xFF, 0xFF, 0xFF, 0x80).argb(), 0x8080_8080);
        assert_eq!(Color::rgba(0xFF, 0x00, 0x00, 0x40).argb(), 0x4040_0000);
        // No alpha is no color at all, whatever channels were written with it.
        assert_eq!(Color::rgba(0xFF, 0xFF, 0xFF, 0x00).argb(), 0x0000_0000);
    }

    #[test]
    fn the_shipped_look_is_the_one_written_down() {
        // A config that sets no colors draws these.
        let t = Theme::DEFAULT;
        assert_eq!(t.background.argb(), 0xF01D_1F22);
        assert_eq!(t.text.argb(), 0xFFEC_ECEC);
        assert_eq!(t.subtitle.argb(), 0xFF88_8888);
        assert_eq!(t.highlight.argb(), 0x0D0D_0D0D);
        assert_eq!(t.caret.argb(), 0xFF6F_D3EF);
        assert_eq!(t.selection.argb(), 0xFF2F_7BA6);

        // Two carry alpha, the rest do not.
        assert!(t.background.argb() >> 24 > 0xE0);
        assert!(t.highlight.argb() >> 24 < 0x20);
        for solid in [t.text, t.subtitle, t.caret, t.selection] {
            assert_eq!(
                solid.argb() >> 24,
                0xFF,
                "{:08x} is not opaque",
                solid.argb()
            );
        }
    }

    #[test]
    fn the_window_grows_a_row_at_a_time_up_to_the_list() {
        assert_eq!(calculate_height(0), INPUT_BOX_H);
        assert_eq!(calculate_height(2), INPUT_BOX_H + 2 * RESULT_HEIGHT);
        // draw_ui paints at most MAX_RESULTS rows, so the window stops there too.
        assert_eq!(
            calculate_height(MAX_RESULTS + 5),
            calculate_height(MAX_RESULTS)
        );
    }

    /// Draw a frame offscreen the way the launcher would, passes times over
    /// into the same buffer: the launcher's buffers are reused turn about, so
    /// every frame lands on the pixels of the frame before last.
    fn frame(
        theme: &Theme,
        text: &str,
        search_enabled: bool,
        results: &[&DesktopEntry],
        rows: usize,
        passes: usize,
    ) -> Vec<u32> {
        let font = font::Font::from_path(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/test-font.ttf"
        ))
        .expect("fixture font");
        let state = AppState::new(desktop::Catalog::new(), Default::default());
        let caret = Caret {
            offset: 0,
            selection: None,
            visible: false,
        };

        let mut buf =
            shm::PixelBuffer::new(WINDOW_WIDTH, calculate_height(rows)).expect("offscreen buffer");
        for _ in 0..passes {
            draw_ui(
                &mut buf,
                text,
                &state,
                results,
                &font,
                search_enabled,
                &caret,
                theme,
            );
        }
        buf.pixels_u32().to_vec()
    }

    /// The usual frame: two results with the first selected, so the background,
    /// the wash over the selected row and the text all land in the same buffer.
    fn frame_with(theme: &Theme) -> Vec<u32> {
        let alpha = DesktopEntry::new("Alpha", "alpha").expect("entry");
        let beta = DesktopEntry::new("Beta", "beta").expect("entry");
        frame(theme, "app", false, &[&alpha, &beta], 2, 1)
    }

    fn at(frame: &[u32], x: u32, y: u32) -> u32 {
        frame[(y * WINDOW_WIDTH + x) as usize]
    }

    fn alpha_of(pixel: u32) -> u32 {
        pixel >> 24 & 0xFF
    }

    #[test]
    fn drawing_a_frame_twice_into_one_buffer_gives_the_same_picture() {
        // A frame that composites onto what the last one left drifts as it
        // repaints: a translucent wash over its own previous pass lightens a
        // little every second frame.
        let theme = Theme {
            background: Color::rgba(0x29, 0x2b, 0x30, 0xE6),
            highlight: Color::rgba(0xFF, 0xFF, 0xFF, 0x14),
            ..Theme::DEFAULT
        };
        let alpha = DesktopEntry::new("Alpha", "alpha").expect("entry");
        let one = [&alpha];
        let none: [&DesktopEntry; 0] = [];
        for (text, search_enabled, results) in [
            ("app", false, &one[..]),
            ("s:cats", true, &none[..]),
            ("u:example.com", false, &none[..]),
        ] {
            assert_eq!(
                frame(&theme, text, search_enabled, results, 1, 1),
                frame(&theme, text, search_enabled, results, 1, 2),
                "{text:?}: the second pass should land on a clean buffer, not on the first"
            );
        }
    }

    #[test]
    fn a_translucent_background_shows_through_the_whole_frame() {
        let theme = Theme {
            background: Color::rgba(0x29, 0x2b, 0x30, 0xE6),
            highlight: Color::rgba(0xFF, 0xFF, 0xFF, 0x14),
            ..Theme::DEFAULT
        };
        let frame = frame_with(&theme);
        let background = theme.background.argb();

        // Away from the text, the window is the background exactly, alpha and
        // all: what the config asked for is what the compositor is handed.
        assert_eq!(at(&frame, CLEAR_X, 10), background);
        // The second row is not selected, so it is bare background too.
        let unselected_y = RESULTS_START_Y + RESULT_HEIGHT + RESULT_HEIGHT / 2;
        assert_eq!(at(&frame, CLEAR_X, unselected_y), background);

        // The selected row is the background plus the wash: lighter, and only a
        // shade less see-through, rather than a hole through the window.
        let washed = at(&frame, CLEAR_X, RESULTS_START_Y + RESULT_HEIGHT / 2);
        assert!(
            alpha_of(washed) > alpha_of(background) && alpha_of(washed) < 0xF0,
            "the wash should lift the row a little, not seal it: {washed:08x}"
        );
        for shift in [16, 8, 0] {
            assert!(
                washed >> shift & 0xFF > background >> shift & 0xFF,
                "the wash should lighten every channel: {washed:08x}"
            );
        }

        // Nothing anywhere is more transparent than the background asked to be,
        // and the text is more solid than the panel it sits on.
        let alphas = frame.iter().map(|&p| alpha_of(p));
        assert_eq!(alphas.clone().min(), Some(0xE6));
        assert!(
            alphas.max().unwrap_or(0) > 0xF0,
            "opaque text should stay opaque over a translucent window"
        );

        // Every pixel is still premultiplied. A channel above its own alpha is
        // a color the compositor cannot read.
        for (i, &p) in frame.iter().enumerate() {
            for shift in [16, 8, 0] {
                assert!(
                    p >> shift & 0xFF <= alpha_of(p),
                    "pixel {i} is {p:08x}, which is not premultiplied"
                );
            }
        }
    }

    #[test]
    fn the_default_theme_is_as_see_through_as_it_asked_to_be() {
        let frame = frame_with(&Theme::DEFAULT);
        let background = alpha_of(Theme::DEFAULT.background.argb());
        assert!(
            frame.iter().all(|&p| alpha_of(p) >= background),
            "nothing is more transparent than the window asked to be"
        );
        assert!(
            frame.iter().any(|&p| alpha_of(p) == 0xFF),
            "text stays opaque over it"
        );
    }

    #[test]
    fn row_at_maps_a_position_to_the_row_drawn_there() {
        let top = RESULTS_START_Y as f64;
        assert_eq!(row_at(MID_X, top, 3), Some(0));
        assert_eq!(row_at(MID_X, top + RESULT_HEIGHT as f64, 3), Some(1));
        assert_eq!(row_at(MID_X, top + 2.0 * RESULT_HEIGHT as f64, 3), Some(2));
    }

    #[test]
    fn a_row_boundary_belongs_to_the_row_below_it() {
        // The rows butt up against each other, so the pixel a row starts on is
        // that row's and the one before it ends a pixel short.
        let boundary = (RESULTS_START_Y + RESULT_HEIGHT) as f64;
        assert_eq!(row_at(MID_X, boundary - 0.5, 3), Some(0));
        assert_eq!(row_at(MID_X, boundary, 3), Some(1));
    }

    #[test]
    fn row_at_ignores_everything_that_is_not_a_row() {
        let top = RESULTS_START_Y as f64;
        // The input field sits above the list.
        assert_eq!(row_at(MID_X, top - 1.0, 3), None);
        assert_eq!(row_at(MID_X, 0.0, 3), None);
        // Past the last row drawn, which is not the same as past the window: a
        // shorter list leaves the rows below it undrawn.
        assert_eq!(row_at(MID_X, top + 3.0 * RESULT_HEIGHT as f64, 3), None);
        assert_eq!(row_at(MID_X, top + RESULT_HEIGHT as f64, 1), None);
        // No results at all means no row anywhere.
        assert_eq!(row_at(MID_X, top, 0), None);
    }

    #[test]
    fn row_at_stops_at_the_edges_of_the_window() {
        let top = RESULTS_START_Y as f64;
        assert_eq!(row_at(0.0, top, 3), Some(0));
        assert_eq!(row_at(-1.0, top, 3), None);
        assert_eq!(row_at((WINDOW_WIDTH - 1) as f64, top, 3), Some(0));
        assert_eq!(row_at(WINDOW_WIDTH as f64, top, 3), None);
    }

    #[test]
    fn row_at_never_points_past_what_the_list_draws() {
        // draw_ui paints at most MAX_RESULTS rows, whatever the count says, so
        // the hit test must not hand back an index into a row nothing painted.
        let top = RESULTS_START_Y as f64;
        let past = top + (MAX_RESULTS as f64) * RESULT_HEIGHT as f64;
        assert_eq!(row_at(MID_X, past, MAX_RESULTS + 5), None);
        assert_eq!(
            row_at(MID_X, past - 1.0, MAX_RESULTS + 5),
            Some(MAX_RESULTS - 1)
        );
    }

    #[test]
    fn the_input_field_is_where_the_text_is() {
        let inside = 10.0;
        assert!(input_box_contains(TEXT_X as f64, inside));
        assert!(input_box_contains((WINDOW_WIDTH - 1) as f64, inside));
        // Left of the text, past the right edge, and below the field.
        assert!(!input_box_contains(TEXT_X as f64 - 1.0, inside));
        assert!(!input_box_contains(WINDOW_WIDTH as f64, inside));
        assert!(!input_box_contains(TEXT_X as f64, RESULTS_START_Y as f64));
    }
}
