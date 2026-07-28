//! User configuration.
//!
//! $XDG_CONFIG_HOME/bnklaunch/config (fallback ~/.config/bnklaunch/config), one
//! `field value` per line. Blank lines and # comments are ignored, and an
//! unknown field is skipped rather than refused, so a config written for a later
//! version still loads. A field may repeat: hidden takes one app name per line
//! and they accumulate.

use crate::desktop::NAME_CAP;
use crate::platform::arena::{ArrayString, ArrayVec};
use crate::platform::{env, fs};

const FILENAME: &str = "config";

/// Largest config file read; config is a handful of short lines.
const CONFIG_MAX: usize = 16 * 1024;

/// Byte capacity of the configured search URL.
pub const URL_CAP: usize = 512;
/// Most hidden app names the config holds.
pub const MAX_DENY: usize = 64;

/// Result rows drawn under the input. The window grows and shrinks with the
/// number of rows, up to this many.
pub const MAX_RESULTS: usize = 5;

/// Width of the launcher window in pixels.
pub const WINDOW_WIDTH: u32 = 800;

/// Parsed configuration. A field absent from the file keeps its default.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// Search URL for the s: prefix; None disables it. The URL holds %s where
    /// the percent-encoded query goes, or has the query appended when it has
    /// no placeholder.
    pub search_url: Option<ArrayString<URL_CAP>>,
    /// App names to hide from results, one per hidden line, deduplicated.
    pub denied: ArrayVec<ArrayString<NAME_CAP>, MAX_DENY>,
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
                let mut url: ArrayString<URL_CAP> = ArrayString::new();
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
            _ => {}
        }
    }
    config
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
    fn load_from_missing_file_is_default() {
        assert_eq!(
            load_from("/nonexistent/path/should/not/exist/config"),
            Config::default()
        );
    }
}
