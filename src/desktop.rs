//! Desktop entry (.desktop file) parsing.
//!
//! Parses freedesktop.org desktop entry files to discover installed applications.
//! See: https://specifications.freedesktop.org/desktop-entry-spec/latest/

use crate::platform::arena::{ArrayString, ArrayVec};
use crate::platform::syscall::{DT_DIR, DT_LNK, DT_REG};
use crate::platform::{env, fs};

/// Most XDG data directories scanned for applications.
const MAX_DATA_DIRS: usize = 16;
/// Largest .desktop file read; these are short key=value files.
const DESKTOP_FILE_MAX: usize = 64 * 1024;

/// A scanned filesystem path.
type ScanPath = ArrayString<{ fs::PATH_CAP }>;

/// Join two path segments with a single separator, or None if the result does
/// not fit.
fn join(base: &str, child: &str) -> Option<ScanPath> {
    let mut p = ScanPath::new();
    p.push_str(base).ok()?;
    if !base.ends_with('/') {
        p.push('/').ok()?;
    }
    p.push_str(child).ok()?;
    Some(p)
}

/// Byte capacity of an entry's display name. Names longer than this are skipped
/// rather than truncated; real desktop entries are far shorter.
pub const NAME_CAP: usize = 256;
/// Byte capacity of an entry's raw Exec value. An entry whose Exec exceeds this
/// is skipped entirely, so the budget is generous: wine and flatpak entries
/// wrap the real command in env setup and forwarding flags that routinely run
/// past a few hundred bytes, and dropping those installed apps is worse than
/// the extra catalog bytes.
pub const EXEC_CAP: usize = 1024;
/// Byte capacity of an entry's icon name (empty means no icon).
pub const ICON_CAP: usize = 64;
/// Byte capacity of one tokenized Exec argument. Tokenizing only ever removes
/// characters from the Exec value, so an argument cannot outgrow the value it
/// came from: matching EXEC_CAP here makes a truncated argument impossible
/// rather than merely unlikely.
pub const ARG_CAP: usize = EXEC_CAP;
/// Most arguments a tokenized Exec value yields.
pub const MAX_ARGS: usize = 32;
/// Byte capacity of the rendered command-line subtitle under a result.
pub const SUBTITLE_CAP: usize = 320;
/// Most desktop entries the catalog holds; extras past this are dropped. Two
/// catalogs (the live one and the pending rescan) sit on the stack, so this is
/// kept modest while staying well above any real application count.
pub const MAX_ENTRIES: usize = 1024;
/// Most results search or recents resolution return. Only the first handful
/// are ever displayed, so this is comfortably above what the UI shows.
pub const RESULT_CAP: usize = 32;
/// Byte capacity of a desktop file ID (the flattened relative path).
const ID_CAP: usize = 256;

/// The discovered set of applications, held inline without a heap.
pub type Catalog = ArrayVec<DesktopEntry, MAX_ENTRIES>;

/// The Type key of a desktop entry. Only Application entries are launchable;
/// Link and Directory entries (and anything unrecognized) are skipped.
#[derive(PartialEq, Eq)]
enum EntryType {
    Application,
    Link,
    Directory,
    Unknown,
}

impl EntryType {
    fn parse(value: &str) -> Self {
        match value {
            "Application" => EntryType::Application,
            "Link" => EntryType::Link,
            "Directory" => EntryType::Directory,
            _ => EntryType::Unknown,
        }
    }
}

/// A parsed desktop entry representing an application. Fixed-size fields keep
/// the catalog heapless; the lowercased name used for matching is recomputed on
/// the fly rather than stored alongside the name.
#[derive(Clone, Copy, Debug)]
pub struct DesktopEntry {
    /// Application name (from the Name field).
    pub name: ArrayString<NAME_CAP>,
    /// Raw Exec value, tokenized per the spec at launch time (see exec_argv).
    pub exec: ArrayString<EXEC_CAP>,
    /// Icon name, empty when the entry has none. Not rendered yet.
    #[allow(dead_code)]
    pub icon: ArrayString<ICON_CAP>,
}

impl DesktopEntry {
    /// Build an entry from its fields. None when a field exceeds its capacity,
    /// so an absurdly long field skips the entry rather than truncating it.
    /// icon may be empty.
    pub fn new(name: &str, exec: &str, icon: &str) -> Option<Self> {
        let mut e = DesktopEntry {
            name: ArrayString::new(),
            exec: ArrayString::new(),
            icon: ArrayString::new(),
        };
        e.name.push_str(name).ok()?;
        e.exec.push_str(exec).ok()?;
        e.icon.push_str(icon).ok()?;
        Some(e)
    }

    /// Parse a desktop file from the given path.
    pub fn from_file(path: &str) -> Option<Self> {
        let mut buf: ArrayVec<u8, DESKTOP_FILE_MAX> = ArrayVec::new();
        fs::read_file(path, &mut buf).ok()?;
        let content = core::str::from_utf8(&buf).ok()?;
        Self::parse(content)
    }

    /// Parse desktop entry content. None when the entry is not a launchable,
    /// visible Application or a required field is missing or oversized.
    fn parse(content: &str) -> Option<Self> {
        let mut in_desktop_entry = false;
        // Track only the keys that matter, as slices into content. A repeated
        // key keeps the last value, matching a map insert.
        let mut type_value = "";
        let mut name: Option<&str> = None;
        let mut exec: Option<&str> = None;
        let mut icon = "";
        let mut no_display = false;
        let mut hidden = false;

        for line in content.lines() {
            let line = line.trim();

            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Section header
            if line.starts_with('[') {
                in_desktop_entry = line == "[Desktop Entry]";
                continue;
            }

            // Only parse fields in [Desktop Entry] section
            if !in_desktop_entry {
                continue;
            }

            // Parse key=value
            if let Some(eq_pos) = line.find('=') {
                let key = line[..eq_pos].trim();
                let value = line[eq_pos + 1..].trim();

                // Skip localized keys (e.g., Name[en_US])
                if key.contains('[') {
                    continue;
                }

                match key {
                    "Type" => type_value = value,
                    "Name" => name = Some(value),
                    "Exec" => exec = Some(value),
                    "Icon" => icon = value,
                    "NoDisplay" => no_display = value == "true",
                    "Hidden" => hidden = value == "true",
                    _ => {}
                }
            }
        }

        // Only launchable, visible Application entries.
        if EntryType::parse(type_value) != EntryType::Application {
            return None;
        }
        if no_display || hidden {
            return None;
        }

        // Name and Exec are required; the raw Exec is tokenized at launch time.
        // All three are string-typed keys, so their escape sequences resolve
        // here, before anything reads the value.
        let mut e = DesktopEntry {
            name: ArrayString::new(),
            exec: ArrayString::new(),
            icon: ArrayString::new(),
        };
        unescape_into(&mut e.name, name?).ok()?;
        unescape_into(&mut e.exec, exec?).ok()?;
        unescape_into(&mut e.icon, icon).ok()?;
        Some(e)
    }

    /// Check if this entry matches a search query (case-insensitive substring
    /// match on name).
    pub fn matches(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let mut q: ArrayString<NAME_CAP> = ArrayString::new();
        push_lower(&mut q, query);
        let mut n: ArrayString<NAME_CAP> = ArrayString::new();
        push_lower(&mut n, &self.name);
        n.as_str().contains(q.as_str())
    }

    /// Calculate a relevance score for sorting (higher = more relevant).
    /// Tiers: exact match, then prefix match, then substring match; within a
    /// tier shorter names rank higher.
    pub fn relevance(&self, query: &str) -> i32 {
        if query.is_empty() {
            return 0;
        }

        let mut q: ArrayString<NAME_CAP> = ArrayString::new();
        push_lower(&mut q, query);
        let mut n: ArrayString<NAME_CAP> = ArrayString::new();
        push_lower(&mut n, &self.name);
        let len_penalty = (self.name.chars().count() as i32).min(50);

        if n.as_str() == q.as_str() {
            300
        } else if n.as_str().starts_with(q.as_str()) {
            200 - len_penalty
        } else if n.as_str().contains(q.as_str()) {
            100 - len_penalty
        } else {
            0
        }
    }
}

/// Copy a Desktop Entry string value into out with its escape sequences
/// resolved: \s is a space, \n a newline, \t a tab, \r a carriage return, and
/// \\ a single backslash. Any other escape is kept as written.
///
/// The spec applies these before the Exec quoting rules, so \\ has to collapse
/// here: leave it, and the tokenizer reads two backslashes where the file meant
/// one. Err when the value does not fit, which drops the entry rather than
/// storing a truncated command.
fn unescape_into<const N: usize>(out: &mut ArrayString<N>, s: &str) -> Result<(), ()> {
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c)?;
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' ')?,
            Some('n') => out.push('\n')?,
            Some('t') => out.push('\t')?,
            Some('r') => out.push('\r')?,
            Some('\\') => out.push('\\')?,
            Some(other) => {
                out.push('\\')?;
                out.push(other)?;
            }
            None => out.push('\\')?,
        }
    }
    Ok(())
}

/// Append the lowercase of s to out, dropping anything past out's capacity.
/// Used to build a comparison key without allocating; a truncated key only
/// affects matching for pathologically long names.
fn push_lower<const N: usize>(out: &mut ArrayString<N>, s: &str) {
    for c in s.chars() {
        for lc in c.to_lowercase() {
            if out.push(lc).is_err() {
                return;
            }
        }
    }
}

/// Tokenize an Exec value per the Desktop Entry spec and call emit once per
/// argument. The shared tokenizer behind exec_argv and exec_subtitle; the
/// quoting and field-code rules live here. Err means the value did not fit: an
/// argument overran ARG_CAP, or emit rejected one.
fn for_each_exec_arg(exec: &str, mut emit: impl FnMut(&str) -> Result<(), ()>) -> Result<(), ()> {
    let mut cur: ArrayString<ARG_CAP> = ArrayString::new();
    let mut has_arg = false;
    let mut chars = exec.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' => {
                if has_arg {
                    emit(cur.as_str())?;
                    cur.clear();
                    has_arg = false;
                }
            }
            '"' => {
                has_arg = true;
                while let Some(qc) = chars.next() {
                    match qc {
                        '"' => break,
                        '\\' => match chars.peek() {
                            Some(&next @ ('"' | '`' | '$' | '\\')) => {
                                cur.push(next)?;
                                chars.next();
                            }
                            _ => cur.push('\\')?,
                        },
                        other => cur.push(other)?,
                    }
                }
            }
            '%' => match chars.peek() {
                // %% is a literal percent sign.
                Some('%') => {
                    chars.next();
                    cur.push('%')?;
                    has_arg = true;
                }
                // Every other field code (%f, %U, %i, ...) expands to nothing:
                // the launcher has no file or URL to substitute. Only the code
                // letter is consumed, so a lone % standing before a separator
                // cannot swallow it and glue two arguments into one.
                Some(&next) if next != ' ' && next != '\t' => {
                    chars.next();
                }
                _ => {}
            },
            other => {
                cur.push(other)?;
                has_arg = true;
            }
        }
    }
    if has_arg {
        emit(cur.as_str())?;
    }
    Ok(())
}

/// Tokenize an Exec value into an argument vector per the Desktop Entry spec
/// quoting rules. Double quotes group an argument and backslash escapes the
/// four reserved characters inside them; %% becomes a literal %, and the field
/// codes (%f, %U, %i, ...) are dropped since the launcher has no file or URL
/// to substitute. The result is exec'd directly, never through a shell, so
/// shell metacharacters in a .desktop file are inert.
///
/// Err when the value yields more arguments than MAX_ARGS. A truncated argv
/// would exec a mangled command line, so the caller refuses the launch instead.
pub fn exec_argv(exec: &str, out: &mut ArrayVec<ArrayString<ARG_CAP>, MAX_ARGS>) -> Result<(), ()> {
    out.clear();
    for_each_exec_arg(exec, |arg| {
        let mut s: ArrayString<ARG_CAP> = ArrayString::new();
        s.push_str(arg)?;
        out.push(s).map_err(|_| ())
    })
}

/// Render the tokenized Exec value as a single space-joined line for the result
/// subtitle, with field codes already dropped. The subtitle is cosmetic, so a
/// command line longer than the line renders as much as fits and stops.
pub fn exec_subtitle(exec: &str, out: &mut ArrayString<SUBTITLE_CAP>) {
    out.clear();
    let mut first = true;
    let _ = for_each_exec_arg(exec, |arg| {
        if !first {
            out.push(' ')?;
        }
        first = false;
        out.push_str(arg)
    });
}

/// Discover all desktop entries from XDG data directories into out, replacing
/// its previous contents.
pub fn discover_entries(out: &mut ArrayVec<DesktopEntry, MAX_ENTRIES>) {
    out.clear();
    let mut seen_ids: ArrayVec<ArrayString<ID_CAP>, MAX_ENTRIES> = ArrayVec::new();

    let mut dirs: ArrayVec<ScanPath, MAX_DATA_DIRS> = ArrayVec::new();
    get_data_dirs(&mut dirs);
    for dir in dirs.iter() {
        if let Some(apps_dir) = join(dir, "applications") {
            if fs::is_dir(&apps_dir) {
                collect_entries(&apps_dir, &apps_dir, &mut seen_ids, out);
            }
        }
    }

    // Sort alphabetically by name.
    out.as_mut_slice()
        .sort_unstable_by(|a, b| cmp_lower(&a.name, &b.name));
}

/// Walk an applications directory tree, collecting visible entries. `root` is
/// the applications dir each desktop file ID is computed against; `dir` is the
/// directory currently being scanned. The first file seen for an ID wins, so
/// listing data dirs in precedence order makes earlier dirs shadow later ones,
/// and a higher-precedence Hidden entry hides the app per the spec.
fn collect_entries(
    root: &str,
    dir: &str,
    seen_ids: &mut ArrayVec<ArrayString<ID_CAP>, MAX_ENTRIES>,
    entries: &mut ArrayVec<DesktopEntry, MAX_ENTRIES>,
) {
    let Ok(mut read_dir) = fs::ReadDir::open(dir) else {
        return;
    };
    let _ = read_dir.for_each(|name, d_type| {
        let Some(path) = join(dir, name) else {
            return;
        };
        // A symlinked directory is treated as a non-directory and not recursed
        // into, avoiding a cycle; a symlinked .desktop file is still read.
        let is_directory = match d_type {
            DT_DIR => true,
            DT_REG | DT_LNK => false,
            _ => fs::is_dir_nofollow(&path),
        };
        if is_directory {
            collect_entries(root, &path, seen_ids, entries);
        } else if name.ends_with(".desktop") {
            let Some(id) = desktop_id(root, &path) else {
                return;
            };
            // First id seen wins; a duplicate (a lower-precedence dir) is skipped.
            if seen_ids.iter().any(|s| s.as_str() == id.as_str()) {
                return;
            }
            let _ = seen_ids.push(id);
            if let Some(desktop_entry) = DesktopEntry::from_file(&path) {
                let _ = entries.push(desktop_entry);
            }
        }
    });
}

/// The desktop file ID for a path under an applications root: the relative path
/// with directory separators turned into dashes and the .desktop suffix
/// removed (so kde4/foo.desktop becomes kde4-foo).
fn desktop_id(root: &str, path: &str) -> Option<ArrayString<ID_CAP>> {
    let rel = path.strip_prefix(root)?;
    let rel = rel.strip_prefix('/').unwrap_or(rel);
    let stem = rel.strip_suffix(".desktop").unwrap_or(rel);
    let mut out: ArrayString<ID_CAP> = ArrayString::new();
    for ch in stem.chars() {
        out.push(if ch == '/' { '-' } else { ch }).ok()?;
    }
    Some(out)
}

/// Fill out with the XDG data directories to search, highest priority first.
fn get_data_dirs(out: &mut ArrayVec<ScanPath, MAX_DATA_DIRS>) {
    out.clear();

    // User data dir (highest priority). XDG_DATA_HOME stands on its own; HOME
    // only supplies the base for the default when it is unset or empty, so a
    // session that sets XDG_DATA_HOME without HOME still resolves.
    let mut dir = ScanPath::new();
    let ok = match env::var("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        Some(xdg) => dir.push_str(xdg).is_ok(),
        None => match env::var("HOME") {
            Some(home) => dir
                .push_str(home)
                .and_then(|_| dir.push_str("/.local/share"))
                .is_ok(),
            None => false,
        },
    };
    if ok {
        let _ = out.push(dir);
    }

    // System data dirs. Unset and empty both mean the spec's default.
    let system_dirs = env::var("XDG_DATA_DIRS")
        .filter(|s| !s.is_empty())
        .unwrap_or("/usr/local/share:/usr/share");
    for dir in system_dirs.split(':') {
        if dir.is_empty() {
            continue;
        }
        let mut p = ScanPath::new();
        if p.push_str(dir).is_ok() {
            let _ = out.push(p);
        }
    }
}

/// Fingerprint of the application directories: the newest mtime across the
/// applications subdirs that discover_entries scans. A directory's mtime
/// changes whenever a .desktop file is added to or removed from it, which is
/// the install/uninstall signal the cache needs to notice. Returns 0 when no
/// directory resolves (in-place edits that keep the same filename do not move
/// a directory's mtime, but app installs and removals always do).
pub fn dirs_fingerprint() -> u64 {
    let mut newest = 0u64;
    let mut dirs: ArrayVec<ScanPath, MAX_DATA_DIRS> = ArrayVec::new();
    get_data_dirs(&mut dirs);
    for dir in dirs.iter() {
        if let Some(apps_dir) = join(dir, "applications") {
            if let Some(mtime) = fs::mtime_nanos(&apps_dir) {
                newest = newest.max(mtime);
            }
        }
    }
    newest
}

/// Compare two strings by their lowercase, without allocating a lowercased
/// copy of either.
fn cmp_lower(a: &str, b: &str) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    let mut ai = a.chars().flat_map(char::to_lowercase);
    let mut bi = b.chars().flat_map(char::to_lowercase);
    loop {
        match (ai.next(), bi.next()) {
            (Some(x), Some(y)) if x == y => continue,
            (Some(x), Some(y)) => return x.cmp(&y),
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

/// Search entries and return the top matches ranked by relevance, then name.
/// Relevance is computed once per match into a scratch buffer, which is sorted
/// in place; the caller only ever shows the first few.
pub fn search<'a>(
    entries: &'a [DesktopEntry],
    query: &str,
) -> ArrayVec<&'a DesktopEntry, RESULT_CAP> {
    let mut scored: ArrayVec<(i32, &DesktopEntry), MAX_ENTRIES> = ArrayVec::new();
    for e in entries {
        if e.matches(query) && scored.push((e.relevance(query), e)).is_err() {
            break;
        }
    }

    // Relevance descending, then name ascending.
    scored
        .as_mut_slice()
        .sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| cmp_lower(&a.1.name, &b.1.name)));

    let mut out: ArrayVec<&DesktopEntry, RESULT_CAP> = ArrayVec::new();
    for &(_, e) in scored.iter().take(RESULT_CAP) {
        let _ = out.push(e);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn make_entry(name: &str, exec: &str) -> DesktopEntry {
        DesktopEntry::new(name, exec, "").unwrap()
    }

    // === Parsing tests ===

    #[test]
    fn parse_valid_desktop_file() {
        let content = r#"
[Desktop Entry]
Type=Application
Name=Firefox
Exec=firefox %u
Icon=firefox
"#;
        let entry = DesktopEntry::parse(content).expect("should parse successfully");
        assert_eq!(entry.name, "Firefox");
        // Exec is stored raw; field codes are handled at launch time.
        assert_eq!(entry.exec, "firefox %u");
        assert_eq!(entry.icon, "firefox");
    }

    #[test]
    fn parse_skips_nodisplay() {
        let content = r#"
[Desktop Entry]
Type=Application
Name=Hidden App
Exec=hidden
NoDisplay=true
"#;
        let result = DesktopEntry::parse(content);
        assert!(result.is_none(), "NoDisplay=true should be skipped");
    }

    #[test]
    fn parse_skips_hidden() {
        let content = r#"
[Desktop Entry]
Type=Application
Name=Hidden App
Exec=hidden
Hidden=true
"#;
        let result = DesktopEntry::parse(content);
        assert!(result.is_none(), "Hidden=true should be skipped");
    }

    #[test]
    fn parse_skips_non_application() {
        let content = r#"
[Desktop Entry]
Type=Link
Name=Some Link
URL=https://example.com
"#;
        let result = DesktopEntry::parse(content);
        assert!(result.is_none(), "Type=Link should be skipped");
    }

    #[test]
    fn parse_skips_localized_keys() {
        let content = r#"
[Desktop Entry]
Type=Application
Name=Firefox
Name[de]=Feuerfuchs
Exec=firefox
"#;
        let result = DesktopEntry::parse(content);
        let entry = result.unwrap();
        assert_eq!(entry.name, "Firefox", "should use non-localized Name");
    }

    #[test]
    fn parse_ignores_other_sections() {
        let content = r#"
[Desktop Entry]
Type=Application
Name=MyApp
Exec=myapp

[Desktop Action New]
Name=New Window
Exec=myapp --new
"#;
        let result = DesktopEntry::parse(content);
        let entry = result.unwrap();
        assert_eq!(entry.name, "MyApp");
        assert_eq!(entry.exec, "myapp");
    }

    // === exec_argv tests ===

    /// Collect the tokenized arguments as owned strings for comparison.
    fn argv(exec: &str) -> Vec<String> {
        let mut out: ArrayVec<ArrayString<ARG_CAP>, MAX_ARGS> = ArrayVec::new();
        exec_argv(exec, &mut out).expect("tokenize");
        out.iter().map(|a| a.as_str().to_string()).collect()
    }

    #[test]
    fn exec_argv_drops_field_codes() {
        assert_eq!(argv("firefox %u"), ["firefox"]);
        assert_eq!(argv("code %F"), ["code"]);
        assert_eq!(argv("app %f %u %U"), ["app"]);
    }

    #[test]
    fn exec_argv_splits_plain_arguments() {
        assert_eq!(argv("firefox"), ["firefox"]);
        assert_eq!(argv("my-app --flag"), ["my-app", "--flag"]);
    }

    #[test]
    fn exec_argv_respects_double_quotes() {
        assert_eq!(argv(r#"prog "one arg" two"#), ["prog", "one arg", "two"]);
    }

    #[test]
    fn exec_argv_unescapes_reserved_chars_in_quotes() {
        assert_eq!(argv(r#"echo "a \"b\" $c""#), ["echo", r#"a "b" $c"#]);
    }

    #[test]
    fn exec_argv_double_percent_is_literal() {
        assert_eq!(argv("printf 100%%"), ["printf", "100%"]);
    }

    #[test]
    fn exec_argv_lone_percent_does_not_eat_the_separator() {
        // The % is a stray field code with no letter. Consuming the space after
        // it would glue the next argument onto this one.
        assert_eq!(argv("app % --flag"), ["app", "--flag"]);
        assert_eq!(argv("app %"), ["app"]);
        assert_eq!(argv("app %\tsecond"), ["app", "second"]);
    }

    #[test]
    fn exec_argv_unterminated_quote_takes_the_rest() {
        assert_eq!(argv(r#"prog "one arg"#), ["prog", "one arg"]);
    }

    #[test]
    fn exec_argv_rejects_too_many_arguments() {
        let mut exec = String::from("app");
        for i in 0..MAX_ARGS {
            exec.push_str(&format!(" --flag{}", i));
        }
        let mut out: ArrayVec<ArrayString<ARG_CAP>, MAX_ARGS> = ArrayVec::new();
        // MAX_ARGS + 1 tokens: refused, not trimmed to the first MAX_ARGS.
        assert!(exec_argv(&exec, &mut out).is_err());
    }

    #[test]
    fn exec_argv_takes_exactly_max_args() {
        let mut exec = String::from("app");
        for i in 0..MAX_ARGS - 1 {
            exec.push_str(&format!(" --flag{}", i));
        }
        let mut out: ArrayVec<ArrayString<ARG_CAP>, MAX_ARGS> = ArrayVec::new();
        assert!(exec_argv(&exec, &mut out).is_ok());
        assert_eq!(out.len(), MAX_ARGS);
    }

    #[test]
    fn exec_argv_holds_an_argument_as_long_as_the_exec_value() {
        // An argument cannot outgrow the Exec value it is tokenized from, so a
        // full-capacity single argument still round-trips whole.
        let long = "x".repeat(EXEC_CAP);
        assert_eq!(argv(&long), [long]);
    }

    #[test]
    fn parse_resolves_string_escapes() {
        let content = "[Desktop Entry]\n\
                       Type=Application\n\
                       Name=My\\sApp\n\
                       Exec=prog --title=a\\\\b\n";
        let entry = DesktopEntry::parse(content).expect("entry");
        assert_eq!(entry.name, "My App");
        // The \\ collapses to one backslash before the Exec quoting rules run.
        assert_eq!(entry.exec, r"prog --title=a\b");
    }

    #[test]
    fn parse_keeps_unknown_escapes_as_written() {
        let content = "[Desktop Entry]\n\
                       Type=Application\n\
                       Name=A\\qB\n\
                       Exec=prog\n";
        let entry = DesktopEntry::parse(content).expect("entry");
        assert_eq!(entry.name, r"A\qB");
    }

    #[test]
    fn exec_argv_leaves_shell_metacharacters_inert() {
        // Nothing is a shell operator: the argv is exec'd directly.
        assert_eq!(argv("app; rm -rf ~"), ["app;", "rm", "-rf", "~"]);
    }

    // === matches tests ===

    #[test]
    fn matches_empty_query() {
        let entry = make_entry("Firefox", "firefox");
        assert!(entry.matches(""), "empty query should match everything");
    }

    #[test]
    fn matches_case_insensitive_prefix() {
        let entry = make_entry("Firefox", "firefox");
        assert!(entry.matches("fire"));
        assert!(entry.matches("Fire"));
        assert!(entry.matches("FIRE"));
        assert!(entry.matches("firefox"));
    }

    #[test]
    fn matches_substring_and_rejects_absent() {
        let entry = make_entry("Firefox", "firefox");
        assert!(entry.matches("fox"), "substring should match");
        assert!(
            !entry.matches("chrome"),
            "absent substring should not match"
        );
    }

    // === relevance tests ===

    #[test]
    fn relevance_exact_match_highest() {
        let entry = make_entry("Firefox", "firefox");
        assert_eq!(entry.relevance("firefox"), 300);
        assert_eq!(entry.relevance("Firefox"), 300);
    }

    #[test]
    fn relevance_prefix_beats_substring() {
        let prefix = make_entry("Firefox", "firefox");
        let substring = make_entry("Waterfox", "waterfox");
        // Both contain "fox"; the prefix match must rank higher.
        assert!(prefix.relevance("fire") > substring.relevance("fox"));
        // A substring-only match still scores above no match.
        assert!(substring.relevance("fox") > 0);
    }

    #[test]
    fn relevance_prefix_match_by_length() {
        let short = make_entry("Git", "git");
        let long = make_entry("GitHub Desktop", "github-desktop");

        let short_score = short.relevance("gi");
        let long_score = long.relevance("gi");

        assert!(short_score > long_score, "shorter name should rank higher");
    }

    #[test]
    fn relevance_no_match_zero() {
        let entry = make_entry("Firefox", "firefox");
        assert_eq!(entry.relevance("chrome"), 0);
    }

    // === search tests ===

    #[test]
    fn search_returns_matches_sorted_by_relevance() {
        let entries = vec![
            make_entry("GitHub Desktop", "github-desktop"),
            make_entry("Git", "git"),
            make_entry("Gitk", "gitk"),
        ];

        let results = search(&entries, "git");
        let names: Vec<_> = results.iter().map(|e| e.name.as_str()).collect();

        // Exact match first, then by length
        assert_eq!(names, vec!["Git", "Gitk", "GitHub Desktop"]);
    }

    #[test]
    fn search_empty_query_returns_all() {
        let entries = vec![
            make_entry("Firefox", "firefox"),
            make_entry("Chrome", "chrome"),
        ];

        let results = search(&entries, "");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_no_matches_returns_empty() {
        let entries = vec![
            make_entry("Firefox", "firefox"),
            make_entry("Chrome", "chrome"),
        ];

        let results = search(&entries, "vim");
        assert!(results.is_empty());
    }

    // === discovery tests ===

    #[test]
    fn desktop_id_flattens_subdirs() {
        let root = "/usr/share/applications";
        assert_eq!(
            desktop_id(root, "/usr/share/applications/firefox.desktop").as_deref(),
            Some("firefox")
        );
        assert_eq!(
            desktop_id(root, "/usr/share/applications/kde4/foo.desktop").as_deref(),
            Some("kde4-foo")
        );
    }

    fn write_desktop(path: &str, body: &str) {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn collect_entries_recurses_into_subdirs() {
        let base = format!(
            "{}/bnklaunch_recurse_{}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::remove_dir_all(&base);
        let apps = format!("{base}/applications");
        write_desktop(
            &format!("{apps}/a.desktop"),
            "[Desktop Entry]\nType=Application\nName=Aaa\nExec=aaa\n",
        );
        write_desktop(
            &format!("{apps}/kde/b.desktop"),
            "[Desktop Entry]\nType=Application\nName=Bbb\nExec=bbb\n",
        );

        let mut seen: ArrayVec<ArrayString<ID_CAP>, MAX_ENTRIES> = ArrayVec::new();
        let mut entries: ArrayVec<DesktopEntry, MAX_ENTRIES> = ArrayVec::new();
        collect_entries(&apps, &apps, &mut seen, &mut entries);

        let names: HashSet<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains("Aaa"));
        assert!(names.contains("Bbb"), "subdirectory entry should be found");
        assert!(
            seen.iter().any(|s| s.as_str() == "kde-b"),
            "id reflects the subdirectory"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn collect_entries_higher_precedence_id_shadows_lower() {
        let base = format!(
            "{}/bnklaunch_shadow_{}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::remove_dir_all(&base);
        let high = format!("{base}/high/applications");
        let low = format!("{base}/low/applications");
        // Same id "foo" in both dirs; the high-precedence one is Hidden.
        write_desktop(
            &format!("{high}/foo.desktop"),
            "[Desktop Entry]\nType=Application\nName=Foo\nExec=foo\nHidden=true\n",
        );
        write_desktop(
            &format!("{low}/foo.desktop"),
            "[Desktop Entry]\nType=Application\nName=Foo Low\nExec=foo\n",
        );

        let mut seen: ArrayVec<ArrayString<ID_CAP>, MAX_ENTRIES> = ArrayVec::new();
        let mut entries: ArrayVec<DesktopEntry, MAX_ENTRIES> = ArrayVec::new();
        // high precedence scanned first.
        collect_entries(&high, &high, &mut seen, &mut entries);
        collect_entries(&low, &low, &mut seen, &mut entries);

        assert!(seen.iter().any(|s| s.as_str() == "foo"));
        assert!(
            entries.is_empty(),
            "a hidden higher-precedence entry shadows the lower one"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
