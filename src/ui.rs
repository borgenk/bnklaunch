//! Software rendering of the launcher: theme, layout, and the draw routines
//! that composite the input field and result rows into the pixel buffer.

use crate::app::{normalize_url, parse_action, AppState, InputAction};
use crate::desktop::{self, DesktopEntry};
use crate::launch::URL_CAP;
use crate::platform::arena;
use crate::{font, shm};

const fn argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

// A dark palette. Everything sits on one flat background shade; the selected row
// is the only part that lifts off it, by a few percent of white.
const BODY_BG: u32 = argb(255, 0x29, 0x2b, 0x30);
const TEXT_COLOR: u32 = argb(255, 0xec, 0xec, 0xec);
const SUBTITLE_COLOR: u32 = argb(255, 0x88, 0x88, 0x88);
const CURSOR_COLOR: u32 = argb(255, 0xff, 0x00, 0xaa);
// White at 4%, composited over the background.
const SELECTED_BG: u32 = argb(255, 50, 51, 56);
// A muted accent behind selected input text.
const SELECTION_COLOR: u32 = argb(255, 96, 40, 82);

/// Width of the launcher window in pixels.
pub(crate) const WINDOW_WIDTH: u32 = 800;

/// Result rows drawn under the input. The window grows and shrinks with the
/// number of rows, up to this many.
pub(crate) const MAX_RESULTS: usize = 5;

/// Y position where the input field starts
pub(crate) const INPUT_START_Y: u32 = 0;

/// Input field height
pub(crate) const INPUT_BOX_H: u32 = 54;

/// Left and right gutter. Zero, so the field and rows bleed to the edges.
pub(crate) const MARGIN_X: u32 = 0;

/// Width of the input field and result rows
const FIELD_W: u32 = WINDOW_WIDTH - MARGIN_X * 2;

/// X position where text starts (gutter plus inner padding)
pub(crate) const TEXT_X: u32 = MARGIN_X + 15;

/// Right edge of every text run, as a width from TEXT_X. The trailing gutter is
/// the wider of the two, so a name that fills the row stops well short of the
/// window edge.
const TEXT_W: u32 = WINDOW_WIDTH - TEXT_X - 115;

/// Y position where results start, flush under the input field
const RESULTS_START_Y: u32 = INPUT_START_Y + INPUT_BOX_H;

/// Height of each result item
const RESULT_HEIGHT: u32 = 50;

/// Base height when there are no results: just the input field
const BASE_HEIGHT: u32 = INPUT_START_Y + INPUT_BOX_H;

/// Calculate window height based on number of results
pub(crate) fn calculate_height(num_results: usize) -> u32 {
    let result_count = num_results.min(MAX_RESULTS);
    if result_count == 0 {
        BASE_HEIGHT
    } else {
        BASE_HEIGHT + (result_count as u32 * RESULT_HEIGHT)
    }
}

/// The result row a pointer position falls in, given how many rows are drawn.
/// None for a position above the list, past the last row, or outside the field
/// horizontally. The same arithmetic draw_ui lays the rows out with, read
/// backwards, so the two cannot disagree about where a row is.
pub(crate) fn row_at(x: f64, y: f64, num_results: usize) -> Option<usize> {
    let rows = num_results.min(MAX_RESULTS);
    if x < MARGIN_X as f64 || x >= (MARGIN_X + FIELD_W) as f64 {
        return None;
    }
    let offset = y - RESULTS_START_Y as f64;
    if offset < 0.0 {
        return None;
    }
    let row = (offset / RESULT_HEIGHT as f64) as usize;
    (row < rows).then_some(row)
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
) {
    let cursor = caret.offset;
    let selection = caret.selection;
    let cursor_visible = caret.visible;

    // A single flat surface; the selected row is the only part that washes.
    pixels.fill(BODY_BG);

    // Vertically center text in the input box
    let text_h = font.text_height(font::INPUT_SIZE);
    let text_y = INPUT_START_Y + ((INPUT_BOX_H as f32 - text_h) / 2.0) as u32;
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
                SELECTION_COLOR,
            );
        }
    }

    // Input text
    if !input_text.is_empty() {
        pixels.draw_text(
            TEXT_X,
            text_y,
            input_text,
            TEXT_COLOR,
            TEXT_W,
            font,
            font::INPUT_SIZE,
        );
    }

    // Cursor (hidden on the blink-off phase)
    if cursor_visible {
        let cursor_px = font.x_at_char_offset(input_text, cursor, font::INPUT_SIZE);
        pixels.fill_rect(TEXT_X + cursor_px as u32, text_y, 2, cursor_h, CURSOR_COLOR);
    }

    // Prefix-action preview row (s: / u:) takes precedence over app results.
    match parse_action(input_text, search_enabled) {
        InputAction::Search(q) if !q.is_empty() => {
            draw_preview_row(pixels, "Web search", q, font);
            return;
        }
        InputAction::OpenUrl(u) if !u.is_empty() => {
            let mut url: arena::ArrayString<URL_CAP> = arena::ArrayString::new();
            normalize_url(u, &mut url);
            draw_preview_row(pixels, "Open URL", &url, font);
            return;
        }
        _ => {}
    }

    // Result rows butt up against each other with no gaps; only the selected
    // row gets a wash so it stands out from the body.
    for (i, entry) in results.iter().take(MAX_RESULTS).enumerate() {
        let y = RESULTS_START_Y + (i as u32 * RESULT_HEIGHT);

        if i == state.selected {
            pixels.fill_rect(MARGIN_X, y, FIELD_W, RESULT_HEIGHT, SELECTED_BG);
        }

        // The subtitle previews the resolved command with field codes dropped.
        let mut subtitle: arena::ArrayString<{ desktop::SUBTITLE_CAP }> = arena::ArrayString::new();
        desktop::exec_subtitle(&entry.exec, &mut subtitle);
        draw_row(pixels, y, &entry.name, &subtitle, font);
    }
}

/// Draw a result row: title over subtitle, the two lines vertically centered as
/// a block in the row starting at y. Both lines stop at TEXT_W, so a long name
/// or command line cannot run off the row.
fn draw_row(pixels: &mut shm::PixelBuffer, y: u32, title: &str, subtitle: &str, font: &font::Font) {
    let title_h = font.text_height(font::NAME_SIZE);
    let subtitle_h = font.text_height(font::SUBTITLE_SIZE);
    let gap = 2.0;
    let block_h = title_h + gap + subtitle_h;
    let top = y as f32 + (RESULT_HEIGHT as f32 - block_h) / 2.0;

    pixels.draw_text(
        TEXT_X,
        top as u32,
        title,
        TEXT_COLOR,
        TEXT_W,
        font,
        font::NAME_SIZE,
    );
    pixels.draw_text(
        TEXT_X,
        (top + title_h + gap) as u32,
        subtitle,
        SUBTITLE_COLOR,
        TEXT_W,
        font,
        font::SUBTITLE_SIZE,
    );
}

/// Draw a single preview row for prefix actions (s: / u:).
fn draw_preview_row(pixels: &mut shm::PixelBuffer, title: &str, subtitle: &str, font: &font::Font) {
    let y = RESULTS_START_Y;
    pixels.fill_rect(MARGIN_X, y, FIELD_W, RESULT_HEIGHT, SELECTED_BG);
    draw_row(pixels, y, title, subtitle, font);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A point comfortably inside the field horizontally, which is what every
    /// vertical case below wants to hold still.
    const MID_X: f64 = (MARGIN_X + FIELD_W / 2) as f64;

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
        assert_eq!(row_at(MID_X, INPUT_START_Y as f64, 3), None);
        // Past the last row drawn, which is not the same as past the window: a
        // shorter list leaves the rows below it undrawn.
        assert_eq!(row_at(MID_X, top + 3.0 * RESULT_HEIGHT as f64, 3), None);
        assert_eq!(row_at(MID_X, top + RESULT_HEIGHT as f64, 1), None);
        // No results at all means no row anywhere.
        assert_eq!(row_at(MID_X, top, 0), None);
    }

    #[test]
    fn row_at_stops_at_the_edges_of_the_field() {
        let top = RESULTS_START_Y as f64;
        assert_eq!(row_at(MARGIN_X as f64, top, 3), Some(0));
        assert_eq!(row_at(MARGIN_X as f64 - 1.0, top, 3), None);
        assert_eq!(row_at((MARGIN_X + FIELD_W - 1) as f64, top, 3), Some(0));
        assert_eq!(row_at((MARGIN_X + FIELD_W) as f64, top, 3), None);
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
}
