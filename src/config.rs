//! User configuration only. The window's layout constants live with the drawing
//! that reads them, in ui.
//!
//! $XDG_CONFIG_HOME/bnklaunch/config (fallback ~/.config/bnklaunch/config), one
//! `field value` per line. Blank lines and # comments are ignored, and an
//! unknown field is skipped rather than refused, so a config written for a later
//! version still loads. A field may repeat: hidden takes one app name per line
//! and they accumulate.
//!
//! A colour is #RRGGBB, or #RRGGBBAA to carry alpha. One that does not parse
//! leaves its field at the default, as an unknown field does.

use crate::desktop::NAME_CAP;
use crate::platform::arena::{ArrayString, ArrayVec};
use crate::platform::{env, fs};
use crate::ui::{Color, Theme};

const FILENAME: &str = "config";

/// Largest config file read; config is a handful of short lines.
const CONFIG_MAX: usize = 16 * 1024;

/// Byte capacity of the configured search URL template. Smaller than the URL
/// the launcher builds from it (see launch::URL_CAP), which also carries the
/// percent-encoded query.
pub const SEARCH_URL_CAP: usize = 512;
/// Most hidden app names the config holds.
pub const MAX_DENY: usize = 64;

/// Parsed configuration. A field absent from the file keeps its default.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// Search URL for the s: prefix; None disables it. The URL holds %s where
    /// the percent-encoded query goes, or has the query appended when it has
    /// no placeholder.
    pub search_url: Option<ArrayString<SEARCH_URL_CAP>>,
    /// App names to hide from results, one per hidden line, deduplicated.
    pub denied: ArrayVec<ArrayString<NAME_CAP>, MAX_DENY>,
    /// The colours to draw with; one the file leaves out keeps its default.
    pub theme: Theme,
}

fn config_path() -> Option<ArrayString<{ fs::PATH_CAP }>> {
    let mut p: ArrayString<{ fs::PATH_CAP }> = ArrayString::new();
    if let Some(dir) = env::var("XDG_CONFIG_HOME") {
        p.push_str(dir).ok()?;
    } else {
        p.push_str(env::var("HOME")?).ok()?;
        p.push_str("/.config").ok()?;
    }
    p.push_str("/bnklaunch/").ok()?;
    p.push_str(FILENAME).ok()?;
    Some(p)
}

/// Load configuration from the standard location. A missing or unreadable file
/// yields the defaults, so a typo or absent file never stops the launcher.
pub fn load() -> Config {
    match config_path() {
        Some(path) => load_from(&path),
        None => Config::default(),
    }
}

fn load_from(path: &str) -> Config {
    let mut buf: ArrayVec<u8, CONFIG_MAX> = ArrayVec::new();
    if fs::read_file(path, &mut buf).is_err() {
        return Config::default();
    }
    match core::str::from_utf8(&buf) {
        Ok(text) => parse(text),
        Err(_) => Config::default(),
    }
}

fn parse(text: &str) -> Config {
    let mut config = Config::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // The first whitespace run splits the field name from its value.
        let Some((field, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match field {
            "search_url" => {
                let mut url: ArrayString<SEARCH_URL_CAP> = ArrayString::new();
                if url.push_str(value).is_ok() {
                    config.search_url = Some(url);
                }
            }
            "hidden" => {
                // Deduplicate so a repeated hidden line is not stored twice.
                let dup = config.denied.iter().any(|d| d.as_str() == value);
                let mut name: ArrayString<NAME_CAP> = ArrayString::new();
                if !dup && name.push_str(value).is_ok() {
                    let _ = config.denied.push(name);
                }
            }
            "background" => set_color(&mut config.theme.background, value),
            "text" => set_color(&mut config.theme.text, value),
            "subtitle" => set_color(&mut config.theme.subtitle, value),
            "highlight" => set_color(&mut config.theme.highlight, value),
            "caret" => set_color(&mut config.theme.caret, value),
            "selection" => set_color(&mut config.theme.selection, value),
            _ => {}
        }
    }
    config
}

/// Set a theme colour, leaving it as it was when the value does not parse.
fn set_color(field: &mut Color, value: &str) {
    if let Some(color) = parse_color(value) {
        *field = color;
    }
}

/// #RRGGBB or #RRGGBBAA, with the hash optional and the digits either case.
/// Six digits means fully opaque.
fn parse_color(value: &str) -> Option<Color> {
    // As bytes: slicing a str mid-character panics.
    let hex = value.strip_prefix('#').unwrap_or(value).as_bytes();
    let (rgb, a) = match hex.len() {
        6 => (hex, 0xFF),
        8 => (&hex[..6], hex_byte(hex[6], hex[7])?),
        _ => return None,
    };
    Some(Color::rgba(
        hex_byte(rgb[0], rgb[1])?,
        hex_byte(rgb[2], rgb[3])?,
        hex_byte(rgb[4], rgb[5])?,
        a,
    ))
}

/// Two hex digits as a byte.
fn hex_byte(hi: u8, lo: u8) -> Option<u8> {
    Some((hex_digit(hi)? << 4) | hex_digit(lo)?)
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_search_url() {
        let cfg = parse("search_url https://duckduckgo.com/?q=%s\n");
        assert_eq!(
            cfg.search_url.as_deref(),
            Some("https://duckduckgo.com/?q=%s")
        );
    }

    #[test]
    fn parse_ignores_blank_and_comment_lines() {
        let cfg = parse("\n# a comment\n   \nsearch_url https://x/?q=%s\n");
        assert_eq!(cfg.search_url.as_deref(), Some("https://x/?q=%s"));
    }

    #[test]
    fn parse_tolerates_unknown_fields() {
        let cfg = parse("future_option whatever\nsearch_url https://x/?q=%s\nmystery 42\n");
        assert_eq!(cfg.search_url.as_deref(), Some("https://x/?q=%s"));
    }

    #[test]
    fn parse_tolerates_extra_whitespace() {
        let cfg = parse("   search_url    https://x/?q=%s   \n");
        assert_eq!(cfg.search_url.as_deref(), Some("https://x/?q=%s"));
    }

    #[test]
    fn parse_field_without_value_is_skipped() {
        assert!(parse("search_url\n").search_url.is_none());
    }

    #[test]
    fn parse_empty_or_comment_only_is_default() {
        assert_eq!(parse(""), Config::default());
        assert_eq!(parse("\n\n# only comments\n"), Config::default());
    }

    #[test]
    fn parse_last_value_wins() {
        let cfg = parse("search_url https://a/?q=%s\nsearch_url https://b/?q=%s\n");
        assert_eq!(cfg.search_url.as_deref(), Some("https://b/?q=%s"));
    }

    fn denies(cfg: &Config, name: &str) -> bool {
        cfg.denied.iter().any(|d| d.as_str() == name)
    }

    #[test]
    fn parse_accumulates_hidden_names() {
        let cfg = parse("hidden Firefox\nhidden GIMP\nhidden Firefox\n");
        assert_eq!(cfg.denied.len(), 2);
        assert!(denies(&cfg, "Firefox"));
        assert!(denies(&cfg, "GIMP"));
    }

    #[test]
    fn parse_hidden_preserves_internal_spaces() {
        let cfg = parse("hidden Qt Widgets Designer\n");
        assert!(denies(&cfg, "Qt Widgets Designer"));
    }

    #[test]
    fn parse_mixes_fields() {
        let cfg = parse("search_url https://x/?q=%s\nhidden GIMP\n");
        assert_eq!(cfg.search_url.as_deref(), Some("https://x/?q=%s"));
        assert!(denies(&cfg, "GIMP"));
    }

    #[test]
    fn parse_reads_every_colour_field() {
        let cfg = parse(
            "background #101112\n\
             text #eeeeee\n\
             subtitle #777777\n\
             highlight #123456\n\
             caret #ff00aa\n\
             selection #602852\n",
        );
        assert_eq!(cfg.theme.background, Color::rgba(0x10, 0x11, 0x12, 0xFF));
        assert_eq!(cfg.theme.text, Color::rgba(0xEE, 0xEE, 0xEE, 0xFF));
        assert_eq!(cfg.theme.subtitle, Color::rgba(0x77, 0x77, 0x77, 0xFF));
        assert_eq!(cfg.theme.highlight, Color::rgba(0x12, 0x34, 0x56, 0xFF));
        assert_eq!(cfg.theme.caret, Color::rgba(0xFF, 0x00, 0xAA, 0xFF));
        assert_eq!(cfg.theme.selection, Color::rgba(0x60, 0x28, 0x52, 0xFF));
    }

    #[test]
    fn parse_reads_the_alpha_of_an_eight_digit_colour() {
        let cfg = parse("background #292b30e6\n");
        assert_eq!(cfg.theme.background, Color::rgba(0x29, 0x2b, 0x30, 0xE6));
        // Six digits is the same colour at full alpha, not a different one.
        assert_eq!(
            parse("background #292b30\n").theme.background,
            Color::rgba(0x29, 0x2b, 0x30, 0xFF)
        );
    }

    #[test]
    fn parse_colour_takes_either_case_and_an_optional_hash() {
        let want = Color::rgba(0xAB, 0xCD, 0xEF, 0x12);
        assert_eq!(parse("caret #ABCDEF12\n").theme.caret, want);
        assert_eq!(parse("caret #abcdef12\n").theme.caret, want);
        assert_eq!(parse("caret abcdef12\n").theme.caret, want);
    }

    #[test]
    fn parse_keeps_the_default_for_a_colour_it_cannot_read() {
        // Wrong length, a digit that is not hex, and a word: none of them refuse
        // the file, they just leave the shipped colour standing.
        for line in [
            "background #fff\n",
            "background #12345\n",
            "background #1234567\n",
            "background #zzzzzz\n",
            "background #12345g\n",
            "background transparent\n",
            "background ##ffffff\n",
        ] {
            assert_eq!(
                parse(line).theme.background,
                Theme::DEFAULT.background,
                "{line:?} should have been ignored"
            );
        }
    }

    #[test]
    fn parse_leaves_unnamed_colours_at_their_defaults() {
        let cfg = parse("background #000000ff\n");
        assert_eq!(cfg.theme.text, Theme::DEFAULT.text);
        assert_eq!(cfg.theme.highlight, Theme::DEFAULT.highlight);
        assert_eq!(parse("").theme, Theme::DEFAULT);
    }

    #[test]
    fn parse_last_colour_wins() {
        let cfg = parse("caret #ffffff\ncaret #000000\n");
        assert_eq!(cfg.theme.caret, Color::rgba(0, 0, 0, 0xFF));
    }

    #[test]
    fn load_from_missing_file_is_default() {
        assert_eq!(
            load_from("/nonexistent/path/should/not/exist/config"),
            Config::default()
        );
    }
}
