//! One frame of the launcher, painted with no compositor in sight.
//!
//! draw_ui takes a pixel buffer, so the same call fills an offscreen one. make
//! screenshot paints the scene with this machine's font for the README; the test
//! paints it with the fixture font and measures it against
//! tests/fixtures/reference-frame.txt, which make frame-update rewrites.
//!
//! The measurements avoid pixel comparison, since freetype rounds an edge
//! differently from version to version. Past every text run only the window's
//! own colors reach, so those columns are read off exactly, as the rows where
//! each color starts and stops. The text is measured by the ink it lays down
//! and the box that ink sits in.

use crate::app::AppState;
use crate::desktop::{Catalog, DesktopEntry};
use crate::dev::png;
use crate::font::Font;
use crate::shm::PixelBuffer;
use crate::ui::{calculate_height, draw_ui, Caret, Theme, TEXT_W, TEXT_X, WINDOW_WIDTH};

const FIXTURE_FONT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test-font.ttf");
const REFERENCE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/reference-frame.txt"
);
const SCREENSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/screenshot.png");

/// What has been typed, and the rows a search for it turns up. The first row is
/// the selected one. The second exec carries a field code, which the subtitle
/// drops, so the frame covers that path too.
const QUERY: &str = "fi";
const RESULTS: [(&str, &str); 5] = [
    ("Firefox", "firefox"),
    ("Files", "nautilus --new-window %U"),
    ("Filelight", "filelight"),
    ("File Roller", "file-roller"),
    ("Firmware Updater", "gnome-firmware"),
];

/// Where the columns past every text run start: TEXT_W stops a name or a command
/// line well short of the window edge, and only window colors reach past it.
const GUTTER_X: u32 = TEXT_X + TEXT_W;

/// Per-channel difference at which a pixel counts as another color. Above the
/// faintest edge of a glyph, under the step from the background to the wash.
const TOLERANCE: u32 = 4;

/// How far the amount of ink may move, as a fraction: enough for edge pixels a
/// rasterizer rounds differently, not for another text size.
const MASS_TOLERANCE: f64 = 0.10;

/// How far the box the ink sits in may move, in pixels: a hinted glyph lands
/// either way, text somewhere else does not.
const BOUNDS_TOLERANCE: u32 = 2;

/// A small radius on the screenshot's corners. The frame itself is square.
const CORNER_RADIUS: f32 = 4.0;

/// Paint the scene into an offscreen buffer, and return it with its size.
fn frame(font: &Font) -> (Vec<u32>, u32, u32) {
    let entries: Vec<DesktopEntry> = RESULTS
        .iter()
        .map(|(name, exec)| DesktopEntry::new(name, exec).expect("scene entry"))
        .collect();
    let rows: Vec<&DesktopEntry> = entries.iter().collect();

    let state = AppState::new(Catalog::new(), Default::default());
    let caret = Caret {
        offset: QUERY.chars().count(),
        selection: None,
        visible: true,
    };

    let height = calculate_height(rows.len());
    let mut buffer = PixelBuffer::new(WINDOW_WIDTH, height).expect("offscreen buffer");
    draw_ui(
        &mut buffer,
        QUERY,
        &state,
        &rows,
        font,
        false,
        &caret,
        &Theme::DEFAULT,
    );
    (buffer.pixels_u32().to_vec(), WINDOW_WIDTH, height)
}

/// Undo the premultiply, since a PNG carries straight alpha.
fn straight_alpha(pixels: &mut [u32]) {
    for p in pixels {
        let alpha = *p >> 24;
        if alpha == 0 || alpha == 0xff {
            continue;
        }
        let channel = |shift: u32| ((((*p >> shift) & 0xff) * 0xff + alpha / 2) / alpha) << shift;
        *p = alpha << 24 | channel(16) | channel(8) | channel(0);
    }
}

/// Fade the alpha outside a rounded rectangle, over the distance to the corner's
/// circle so the curve stays smooth.
fn round_corners(pixels: &mut [u32], width: u32, height: u32, radius: f32) {
    let (w, h) = (width as f32, height as f32);
    let span = radius.ceil() as u32;
    for y in 0..height {
        if y >= span && y < height - span {
            continue;
        }
        for x in 0..width {
            if x >= span && x < width - span {
                continue;
            }
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            // How far past the corner circle the pixel sits.
            let dx = (radius - px).max(px - (w - radius)).max(0.0);
            let dy = (radius - py).max(py - (h - radius)).max(0.0);
            let cover = (0.5 - (dx.hypot(dy) - radius)).clamp(0.0, 1.0);
            let Some(slot) = pixels.get_mut((y * width + x) as usize) else {
                continue;
            };
            let alpha = ((*slot >> 24) as f32 * cover).round() as u32;
            *slot = (alpha << 24) | (*slot & 0x00ff_ffff);
        }
    }
}

/// Whether two pixels disagree in color. Alpha is left out: it follows the
/// window's transparency, not what was drawn on it.
fn differs(a: u32, b: u32) -> bool {
    (0..3).any(|channel| {
        let shift = channel * 8;
        ((a >> shift) & 0xff).abs_diff((b >> shift) & 0xff) > TOLERANCE
    })
}

/// Rows top..bottom that are one color past the text.
#[derive(Debug, PartialEq, Eq)]
struct Band {
    top: u32,
    bottom: u32,
    color: u32,
}

/// Read the bands off a frame. A row that is not one color out there means
/// something drew past TEXT_W.
fn bands(pixels: &[u32], width: u32) -> Result<Vec<Band>, String> {
    let height = pixels.len() as u32 / width;
    let mut out: Vec<Band> = Vec::new();
    for y in 0..height {
        let row = &pixels[(y * width) as usize..((y + 1) * width) as usize];
        let color = row[GUTTER_X as usize] & 0x00ff_ffff;
        if let Some(x) = (GUTTER_X..width).find(|&x| differs(row[x as usize], color)) {
            return Err(format!(
                "row {y} is not one color past the text: {:06x} at {x}, {color:06x} at {GUTTER_X}",
                row[x as usize] & 0x00ff_ffff
            ));
        }
        match out.last_mut() {
            Some(band) if band.color == color => band.bottom = y + 1,
            _ => out.push(Band {
                top: y,
                bottom: y + 1,
                color,
            }),
        }
    }
    Ok(out)
}

/// How many pixels carry ink, and the box they sit in.
#[derive(Debug, PartialEq, Eq)]
struct Ink {
    mass: usize,
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

/// Measure the ink. Each row is held against its own color past the text, so a
/// washed row counts the same as a plain one.
fn ink(pixels: &[u32], width: u32) -> Ink {
    let (mut left, mut top) = (u32::MAX, u32::MAX);
    let (mut right, mut bottom) = (0, 0);
    let mut mass = 0;
    for (y, row) in pixels.chunks(width as usize).enumerate() {
        let window = row[GUTTER_X as usize];
        for (x, &p) in row.iter().enumerate() {
            if !differs(p, window) {
                continue;
            }
            let (x, y) = (x as u32, y as u32);
            mass += 1;
            left = left.min(x);
            top = top.min(y);
            right = right.max(x);
            bottom = bottom.max(y);
        }
    }
    Ink {
        mass,
        left,
        top,
        right,
        bottom,
    }
}

/// How the bands in a frame disagree with the ones recorded, if they do.
fn band_mismatch(actual: &[Band], reference: &[Band]) -> Option<String> {
    if let Some((a, b)) = actual.iter().zip(reference).find(|(a, b)| a != b) {
        return Some(format!(
            "rows {}..{} are {:06x}, where the frame had rows {}..{} in {:06x}",
            a.top, a.bottom, a.color, b.top, b.bottom, b.color
        ));
    }
    if actual.len() != reference.len() {
        return Some(format!(
            "the window is {} bands of color, not the {} it was",
            actual.len(),
            reference.len()
        ));
    }
    None
}

/// How the ink disagrees with what was recorded. Mass first: a frame that lost
/// its text has no box worth comparing.
fn ink_mismatch(a: &Ink, b: &Ink) -> Option<String> {
    let (mass, reference) = (a.mass as f64, b.mass as f64);
    if (mass - reference).abs() > reference * MASS_TOLERANCE {
        return Some(format!(
            "the text covers {} pixels, not the {} it did",
            a.mass, b.mass
        ));
    }
    [
        ("left", a.left, b.left),
        ("top", a.top, b.top),
        ("right", a.right, b.right),
        ("bottom", a.bottom, b.bottom),
    ]
    .into_iter()
    .find(|&(_, x, y)| x.abs_diff(y) > BOUNDS_TOLERANCE)
    .map(|(edge, x, y)| format!("the text reaches to {x} on the {edge}, not {y}"))
}

/// Everything the test holds a frame against.
struct Measured {
    width: u32,
    height: u32,
    bands: Vec<Band>,
    ink: Ink,
}

impl Measured {
    fn of(pixels: &[u32], width: u32, height: u32) -> Result<Self, String> {
        Ok(Measured {
            width,
            height,
            bands: bands(pixels, width)?,
            ink: ink(pixels, width),
        })
    }
}

/// One measurement to a line, so a change to the frame reads as one in review.
fn write_reference(measured: &Measured) -> String {
    let mut out = String::from(
        "# What the frame in src/dev/screenshot.rs measures out to, colors as the\n\
         # frame holds them, premultiplied. Rewritten by `make frame-update`;\n\
         # look at assets/screenshot.png before committing.\n",
    );
    out.push_str(&format!("size {} {}\n", measured.width, measured.height));
    for band in &measured.bands {
        out.push_str(&format!(
            "band {} {} {:06x}\n",
            band.top, band.bottom, band.color
        ));
    }
    let ink = &measured.ink;
    out.push_str(&format!(
        "ink {} {} {} {} {}\n",
        ink.mass, ink.left, ink.top, ink.right, ink.bottom
    ));
    out
}

/// Read one back. A malformed file is not a changed frame, so it says which line
/// it could not read.
fn read_reference(text: &str) -> Measured {
    let mut measured = Measured {
        width: 0,
        height: 0,
        bands: Vec::new(),
        ink: Ink {
            mass: 0,
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || -> ! { panic!("{REFERENCE}: cannot read the line: {line}") };
        let mut field = line.split_whitespace().skip(1);
        let mut number = || -> u32 {
            match field.next().map(str::parse) {
                Some(Ok(n)) => n,
                _ => bad(),
            }
        };
        match line.split_whitespace().next() {
            Some("size") => {
                measured.width = number();
                measured.height = number();
            }
            Some("band") => {
                let (top, bottom) = (number(), number());
                let color = match field.next().map(|f| u32::from_str_radix(f, 16)) {
                    Some(Ok(c)) => c,
                    _ => bad(),
                };
                measured.bands.push(Band { top, bottom, color });
            }
            Some("ink") => {
                measured.ink = Ink {
                    mass: number() as usize,
                    left: number(),
                    top: number(),
                    right: number(),
                    bottom: number(),
                };
            }
            _ => bad(),
        }
    }
    measured
}

/// Write the frame that failed under target/, and return where it landed.
fn report(pixels: &[u32], width: u32, height: u32) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("reference-frame-actual.png");
    let mut out = pixels.to_vec();
    straight_alpha(&mut out);
    std::fs::write(&path, png::encode(&out, width, height)).expect("write the frame");
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paint the scene with this machine's font and write it out, which is what
    /// make screenshot runs. Ignored otherwise: what it draws depends on the
    /// fonts installed here.
    #[test]
    #[ignore]
    fn write_screenshot() {
        let font = Font::load().expect("a system font");
        let (mut pixels, width, height) = frame(&font);
        straight_alpha(&mut pixels);
        round_corners(&mut pixels, width, height, CORNER_RADIUS);
        let dir = std::path::Path::new(SCREENSHOT)
            .parent()
            .expect("assets directory");
        std::fs::create_dir_all(dir).expect("create assets");
        std::fs::write(SCREENSHOT, png::encode(&pixels, width, height)).expect("write screenshot");
        println!("wrote {SCREENSHOT} ({width}x{height})");
    }

    /// Paint the scene with the fixture font and measure it against the
    /// committed reference, leaving the frame under target/ on a mismatch.
    #[test]
    fn the_frame_matches_its_reference() {
        let font = Font::from_path(FIXTURE_FONT).expect("fixture font");
        let (pixels, width, height) = frame(&font);
        let measured = Measured::of(&pixels, width, height).unwrap_or_else(|how| {
            panic!(
                "{how}.\nThe frame is at {}.",
                report(&pixels, width, height)
            )
        });

        if std::env::var("BNKLAUNCH_FRAME_UPDATE").is_ok() {
            std::fs::write(REFERENCE, write_reference(&measured)).expect("write the reference");
            println!("reference written to {REFERENCE}");
            return;
        }

        let text = std::fs::read_to_string(REFERENCE).unwrap_or_else(|e| {
            panic!("no reference at {REFERENCE}: {e}. Write one with `make frame-update`.")
        });
        let reference = read_reference(&text);
        assert_eq!(
            (measured.width, measured.height),
            (reference.width, reference.height),
            "the window changed size"
        );

        let what = band_mismatch(&measured.bands, &reference.bands)
            .map(|how| format!("the window changed: {how}"))
            .or_else(|| {
                ink_mismatch(&measured.ink, &reference.ink)
                    .map(|how| format!("the text changed: {how}"))
            });
        let Some(what) = what else {
            return;
        };
        panic!(
            "{what}.\n\
             The frame is at {}.\n\
             If the change was meant, accept it with `make frame-update`.",
            report(&pixels, width, height)
        );
    }

    /// A color or a pixel out of place moves the bands; text elsewhere moves
    /// the ink.
    #[test]
    fn the_measurements_notice_a_frame_that_changed() {
        let font = Font::from_path(FIXTURE_FONT).expect("fixture font");
        let (pixels, width, _) = frame(&font);
        let here_bands = bands(&pixels, width).expect("the frame has bands");
        let here_ink = ink(&pixels, width);
        let banding =
            |other: &[u32]| band_mismatch(&bands(other, width).expect("bands"), &here_bands);
        let text = |other: &[u32]| ink_mismatch(&ink(other, width), &here_ink);

        assert!(banding(&pixels).is_none(), "a frame agrees with itself");
        assert!(text(&pixels).is_none(), "a frame agrees with itself");

        // The smallest layout change there is: the wash starts a pixel late.
        let moved: Vec<u32> = std::iter::repeat_n(pixels[0], width as usize)
            .chain(pixels.iter().copied())
            .take(pixels.len())
            .collect();
        assert!(
            banding(&moved).is_some(),
            "a frame one pixel out of place has to read as changed"
        );

        // Every channel a step darker, which is a color nobody asked for.
        let recolored: Vec<u32> = pixels
            .iter()
            .map(|p| {
                let channel = |shift: u32| ((p >> shift) & 0xff).saturating_sub(8) << shift;
                0xff00_0000 | channel(16) | channel(8) | channel(0)
            })
            .collect();
        assert!(
            banding(&recolored).is_some(),
            "a recolored frame has to read as changed"
        );

        // Text that moved sideways, which the columns past it never see.
        let sideways: Vec<u32> = pixels
            .chunks(width as usize)
            .flat_map(|row| {
                std::iter::repeat_n(row[0], 12)
                    .chain(row.iter().copied())
                    .take(row.len())
            })
            .collect();
        assert!(
            banding(&sideways).is_none(),
            "text that moved leaves the bands alone"
        );
        assert!(
            text(&sideways).is_some(),
            "text somewhere else has to read as changed"
        );

        // A row of text gone, painted out in the color the row already is,
        // which no band can see.
        let mut missing = pixels.clone();
        let row = (calculate_height(1) - calculate_height(0)) as usize * width as usize;
        let start = calculate_height(0) as usize * width as usize;
        let wash = pixels[start + GUTTER_X as usize];
        missing[start..start + row].fill(wash);
        assert!(
            banding(&missing).is_none(),
            "text that went missing leaves the bands alone"
        );
        assert!(
            text(&missing).is_some(),
            "a row of text that went missing has to read as changed"
        );
    }

    /// What is written to the file comes back the same.
    #[test]
    fn a_reference_round_trips_through_its_file() {
        let font = Font::from_path(FIXTURE_FONT).expect("fixture font");
        let (pixels, width, height) = frame(&font);
        let measured = Measured::of(&pixels, width, height).expect("measurements");

        let back = read_reference(&write_reference(&measured));
        assert_eq!((back.width, back.height), (width, height));
        assert_eq!(back.bands, measured.bands);
        assert_eq!(back.ink, measured.ink);
    }
}
