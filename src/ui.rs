//! Software rendering of the launcher: theme, layout, and the draw routines
//! that composite the input field and result rows into the pixel buffer.

use crate::app::{normalize_url, parse_action, AppState, InputAction};
use crate::client::Client;
use crate::config::{MAX_RESULTS, WINDOW_WIDTH};
use crate::desktop::{self, DesktopEntry};
use crate::launch::URL_CAP;
use crate::platform::arena;
use crate::{font, shm};

const fn argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

// UI colours, mirroring bnksound's dark palette. Everything sits on the
// titlebar shade; the selected row picks up the same wash bnksound paints on
// the titlebar profile button.
const BODY_BG: u32 = argb(255, 0x29, 0x2b, 0x30);
const TEXT_COLOR: u32 = argb(255, 0xec, 0xec, 0xec);
const SUBTITLE_COLOR: u32 = argb(255, 0x88, 0x88, 0x88);
const CURSOR_COLOR: u32 = argb(255, 0xff, 0x00, 0xaa);
// White at 4% composited over the titlebar shade (the profile button's fill).
const SELECTED_BG: u32 = argb(255, 50, 51, 56);
// Muted brand accent behind selected input text.
const SELECTION_COLOR: u32 = argb(255, 96, 40, 82);

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

/// Draw the full UI
pub(crate) fn draw_ui(
    client: &mut Client,
    input_text: &str,
    state: &AppState,
    results: &[&DesktopEntry],
    font: &font::Font,
    search_enabled: bool,
    cursor_visible: bool,
) {
    // Extract cursor/selection before borrowing pixels
    let cursor = client.editor.cursor();
    let selection = client.editor.selection();

    let pixels = match client.pixels() {
        Some(p) => p,
        None => return,
    };

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
            670,
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
/// a block in the row starting at y. The subtitle is clamped so a long command
/// line cannot run off the row.
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
        670,
        font,
        font::NAME_SIZE,
    );

    // Clamp to 80 characters on a char boundary so a long command line cannot
    // run off the row; draw_text clips further by width.
    let cut = subtitle
        .char_indices()
        .nth(80)
        .map(|(i, _)| i)
        .unwrap_or(subtitle.len());
    let truncated = &subtitle[..cut];
    pixels.draw_text(
        TEXT_X,
        (top + title_h + gap) as u32,
        truncated,
        SUBTITLE_COLOR,
        670,
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
