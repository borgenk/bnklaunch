//! Desktop entry (.desktop file) parsing.
//!
//! Parses freedesktop.org desktop entry files to discover installed applications.
//! See: https://specifications.freedesktop.org/desktop-entry-spec/latest/

use crate::platform::arena::{ArrayString, ArrayVec};
use crate::platform::fs::{join, ScanPath};
use crate::platform::syscall::{DT_DIR, DT_LNK, DT_REG};
use crate::platform::{env, fs};
#[cfg(test)]
use crate::platform::{syscall, uring};

/// Most XDG data directories scanned for applications.
const MAX_DATA_DIRS: usize = 16;
/// Largest .desktop file read; these are short key=value files.
const DESKTOP_FILE_MAX: usize = 64 * 1024;

/// Byte capacity of an entry's display name. Names longer than this are skipped
/// rather than truncated; real desktop entries are far shorter.
pub const NAME_CAP: usize = 256;
/// Byte capacity of an entry's raw Exec value. An entry whose Exec exceeds this
/// is skipped entirely, so the budget is generous: wine and flatpak entries
/// wrap the real command in env setup and forwarding flags that routinely run
/// past a few hundred bytes, and dropping those installed apps is worse than
/// the extra catalog bytes.
pub const EXEC_CAP: usize = 1024;
/// Byte capacity of one tokenized Exec argument. Tokenizing only ever removes
/// characters from the Exec value, so an argument cannot outgrow the value it
/// came from: matching EXEC_CAP here makes a truncated argument impossible
/// rather than merely unlikely.
pub const ARG_CAP: usize = EXEC_CAP;
/// Most arguments a tokenized Exec value yields.
pub const MAX_ARGS: usize = 32;
/// Byte capacity of the rendered command-line subtitle under a result.
pub const SUBTITLE_CAP: usize = 320;
/// Most desktop entries the catalog holds; extras past this are dropped. With no
/// allocator the catalog is one fixed array of entries, and it lives on the
/// stack, so this is kept modest while staying well above the number of
/// applications any real system installs.
pub const MAX_ENTRIES: usize = 1024;
/// Most results search or recents resolution return. Only the first handful
/// are ever displayed, so this is comfortably above what the UI shows.
pub const RESULT_CAP: usize = 32;
/// Byte capacity of a desktop file ID (the flattened relative path).
const ID_CAP: usize = 256;

/// Files read from the ring in one batch, and the bytes reserved for each.
///
/// A desktop file runs to a couple of kilobytes; the largest on a full desktop
/// is a few tens, and one that fills its slot is re-read whole rather than
/// parsed from a buffer that may have been cut short. The batch and the slot
/// together set the scratch buffer, at half a megabyte.
///
/// Only the bench uses these: reading the files through the ring is measurably
/// slower than reading them one at a time (see read_entries).
#[cfg(test)]
const BATCH: usize = 32;
#[cfg(test)]
const FILE_SLOT: usize = 16 * 1024;

/// Submission queue depth for the scan's ring: a batch costs one open per file,
/// then a read and a close per file that opened.
#[cfg(test)]
pub(crate) const RING_ENTRIES: u32 = (BATCH * 2) as u32;

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
/// the catalog heapless.
#[derive(Clone, Copy, Debug)]
pub struct DesktopEntry {
    /// Application name (from the Name field).
    pub name: ArrayString<NAME_CAP>,
    /// The name lowercased, which is what search compares against.
    ///
    /// Derived, not stored on disk: the cache holds the name and this is rebuilt
    /// when an entry is made. Keeping it costs a few hundred bytes per entry and
    /// saves lowercasing every name in the catalog on every keystroke, which is
    /// the difference between a search that scales with the alphabet and one
    /// that scales with the catalog.
    name_lower: ArrayString<NAME_CAP>,
    /// Character count of the name, the length ranking reads. Derived with
    /// name_lower, because counting it is a UTF-8 walk and score would
    /// otherwise do one per catalog entry per keystroke.
    name_chars: u32,
    /// Raw Exec value, tokenized per the spec at launch time (see exec_argv).
    pub exec: ArrayString<EXEC_CAP>,
}

impl DesktopEntry {
    /// Build an entry from its fields. None when a field exceeds its capacity,
    /// so an absurdly long field skips the entry rather than truncating it.
    pub fn new(name: &str, exec: &str) -> Option<Self> {
        let mut e = DesktopEntry::blank();
        e.name.push_str(name).ok()?;
        e.exec.push_str(exec).ok()?;
        e.derive_search_keys();
        Some(e)
    }

    /// An entry with every field empty, for a caller that fills them itself.
    fn blank() -> Self {
        DesktopEntry {
            name: ArrayString::new(),
            name_lower: ArrayString::new(),
            name_chars: 0,
            exec: ArrayString::new(),
        }
    }

    /// Rebuild what search compares and ranks against, from the name.
    fn derive_search_keys(&mut self) {
        self.name_lower.clear();
        push_lower(&mut self.name_lower, &self.name);
        self.name_chars = self.name.chars().count() as u32;
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
    pub(crate) fn parse(content: &str) -> Option<Self> {
        let mut in_desktop_entry = false;
        // Track only the keys that matter, as slices into content. A repeated
        // key keeps the last value, matching a map insert.
        let mut type_value = "";
        let mut name: Option<&str> = None;
        let mut exec: Option<&str> = None;
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
        // Both are string-typed keys, so their escape sequences resolve here,
        // before anything reads the value.
        let mut e = DesktopEntry::blank();
        unescape_into(&mut e.name, name?).ok()?;
        unescape_into(&mut e.exec, exec?).ok()?;
        e.derive_search_keys();
        Some(e)
    }

    /// Score this entry against a query that is already lowercased. Zero means
    /// it does not match at all.
    ///
    /// Matching and ranking are one pass over one lowercased name, which is what
    /// keeps a keystroke cheap: the query is lowercased once for the whole
    /// catalog, and no name is lowercased at all.
    ///
    /// Tiers: an exact match, then a prefix, then a substring anywhere. Within a
    /// tier a shorter name ranks higher, on the reasoning that a query is a
    /// larger fraction of it.
    fn score(&self, query_lower: &str) -> i32 {
        if query_lower.is_empty() {
            return 1; // everything matches, and nothing outranks anything
        }
        let name = self.name_lower.as_str();
        if name == query_lower {
            return 300;
        }
        let len_penalty = (self.name_chars as i32).min(50);
        if name.starts_with(query_lower) {
            return 200 - len_penalty;
        }
        if name.contains(query_lower) {
            return 100 - len_penalty;
        }
        0
    }

    /// Whether this entry matches a search query, case-insensitively. The
    /// search itself goes through score, having lowercased the query once for
    /// the whole catalog; this is the same question asked of one entry.
    #[cfg(test)]
    pub fn matches(&self, query: &str) -> bool {
        let mut q: ArrayString<NAME_CAP> = ArrayString::new();
        push_lower(&mut q, query);
        self.score(q.as_str()) > 0
    }

    /// The relevance of this entry to a query: higher sorts first, zero does not
    /// match.
    #[cfg(test)]
    pub fn relevance(&self, query: &str) -> i32 {
        if query.is_empty() {
            return 0;
        }
        let mut q: ArrayString<NAME_CAP> = ArrayString::new();
        push_lower(&mut q, query);
        self.score(q.as_str())
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
        // Almost every application name is ASCII, and an ASCII letter folds to
        // exactly one ASCII letter. char::to_lowercase is a Unicode table lookup
        // that hands back an iterator, because a general fold can yield several
        // chars; sidestepping it for ASCII is most of the cost of a match key.
        if c.is_ascii() {
            if out.push(c.to_ascii_lowercase()).is_err() {
                return;
            }
            continue;
        }
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

    // Walk the directories first and collect the paths. The walk is inherently
    // serial (a directory has to be read before its files are known), but the
    // reads that follow are not: a few hundred small files, none of which cares
    // about the others.
    let mut paths: ArrayVec<ScanPath, MAX_ENTRIES> = ArrayVec::new();
    collect_all_paths(&mut paths);
    read_entries(&paths, out);

    // Sort alphabetically by name, on the lowercased key each entry carries.
    out.as_mut_slice()
        .sort_unstable_by(|a, b| a.name_lower.as_str().cmp(b.name_lower.as_str()));
}

/// The path of every desktop file the data dirs offer, deduplicated by id in
/// precedence order. The walk half of a scan, split out from the reading half so
/// the bench can time the reading on its own.
pub(crate) fn collect_all_paths(paths: &mut ArrayVec<ScanPath, MAX_ENTRIES>) {
    let mut seen_ids: ArrayVec<ArrayString<ID_CAP>, MAX_ENTRIES> = ArrayVec::new();
    let mut dirs: ArrayVec<ScanPath, MAX_DATA_DIRS> = ArrayVec::new();
    get_data_dirs(&mut dirs);
    for dir in dirs.iter() {
        if let Some(apps_dir) = join(dir, "applications") {
            if fs::is_dir(&apps_dir) {
                collect_paths(&apps_dir, &apps_dir, &mut seen_ids, paths);
            }
        }
    }
}

/// Read and parse every discovered file, one at a time: open, read, close,
/// parse, next.
///
/// This looks like the obvious candidate for io_uring: a few hundred small
/// independent files, order irrelevant, three syscalls apiece. The ring can do
/// exactly that, and uring::read_files does, in a couple of submissions instead
/// of eight hundred syscalls. It was measured, and it is much slower.
///
/// The first file operation a ring performs costs 12 to 20 ms, once per process.
/// io_uring hands an operation that would block to a kernel worker pool, and
/// standing that pool up is not cheap. A long-lived program does not care: a
/// database amortizes it over millions of operations. A launcher starts, scans
/// once, shows a window, and exits, so it pays that cost in full, on the cold
/// start it was meant to speed up, and it is ten times the entire scan.
///
/// Reading the same files one at a time takes 1.2 to 1.7 ms, first time or not.
/// Once the pool is warm the ring roughly matches it (1.4 ms), which is the
/// other half of the answer: even at its best it wins nothing here, because the
/// syscalls were never what the scan was spending its time on. The kernel-side
/// path walk in openat is, and io_uring does that walk too.
///
/// The capability stays in the platform layer, tested, because the finding is
/// worth being able to re-check. `make scan-bench` times both against each
/// other.
pub(crate) fn read_entries(paths: &[ScanPath], out: &mut ArrayVec<DesktopEntry, MAX_ENTRIES>) {
    for path in paths {
        if let Some(entry) = DesktopEntry::from_file(path) {
            let _ = out.push(entry);
        }
    }
}

#[cfg(test)]
pub(crate) fn read_entries_batched(
    ring: &mut uring::Ring,
    paths: &[ScanPath],
    out: &mut ArrayVec<DesktopEntry, MAX_ENTRIES>,
) {
    // One slot per file in the batch. A slot holds any desktop file worth the
    // name: they run to a couple of kilobytes, and the largest on a full desktop
    // is a few tens. A file that fills its slot might have been cut short, so it
    // is read again the plain way rather than parsed from a maybe-truncated
    // buffer.
    let mut bufs: ArrayVec<u8, { BATCH * FILE_SLOT }> = ArrayVec::new();
    for _ in 0..BATCH * FILE_SLOT {
        let _ = bufs.push(0);
    }
    let mut lens = [0usize; BATCH];

    for chunk in paths.chunks(BATCH) {
        let mut cpaths: ArrayVec<syscall::CPath, BATCH> = ArrayVec::new();
        for path in chunk {
            match syscall::CPath::new(path) {
                Some(cp) => {
                    let _ = cpaths.push(cp);
                }
                None => break, // a path with an interior NUL is not a path
            }
        }
        if cpaths.len() != chunk.len() {
            continue;
        }

        if uring::read_files(ring, &cpaths, &mut bufs, FILE_SLOT, &mut lens).is_err() {
            // The batch did not go through; read it the plain way rather than
            // drop the applications in it.
            for path in chunk {
                if let Some(entry) = DesktopEntry::from_file(path) {
                    let _ = out.push(entry);
                }
            }
            continue;
        }

        for (i, path) in chunk.iter().enumerate() {
            let len = lens[i];
            if len == 0 {
                continue; // unopenable or empty
            }
            let entry = if len == FILE_SLOT {
                // It filled the slot, so it may have more to give. Read it whole.
                DesktopEntry::from_file(path)
            } else {
                core::str::from_utf8(&bufs[i * FILE_SLOT..i * FILE_SLOT + len])
                    .ok()
                    .and_then(DesktopEntry::parse)
            };
            if let Some(entry) = entry {
                let _ = out.push(entry);
            }
        }
    }
}

/// Walk an applications directory tree, collecting the path of every desktop
/// file worth reading. `root` is the applications dir each desktop file ID is
/// computed against; `dir` is the directory currently being walked. The first
/// file seen for an ID wins, so listing the data dirs in precedence order makes
/// an earlier one shadow a later one, and a higher-precedence Hidden entry hides
/// the application per the spec.
fn collect_paths(
    root: &str,
    dir: &str,
    seen_ids: &mut ArrayVec<ArrayString<ID_CAP>, MAX_ENTRIES>,
    paths: &mut ArrayVec<ScanPath, MAX_ENTRIES>,
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
            collect_paths(root, &path, seen_ids, paths);
        } else if name.ends_with(".desktop") {
            let Some(id) = desktop_id(root, &path) else {
                return;
            };
            // First id seen wins; a duplicate (from a lower-precedence dir) is
            // skipped.
            if seen_ids.iter().any(|s| s.as_str() == id.as_str()) {
                return;
            }
            let _ = seen_ids.push(id);
            let _ = paths.push(path);
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
    collect_data_dirs(
        out,
        env::in_flatpak(),
        env::var("HOME"),
        env::var("XDG_DATA_HOME"),
        env::var("XDG_DATA_DIRS"),
    );
}

/// The list itself, with the environment passed in so both layouts are testable.
fn collect_data_dirs(
    out: &mut ArrayVec<ScanPath, MAX_DATA_DIRS>,
    in_flatpak: bool,
    home: Option<&str>,
    data_home: Option<&str>,
    data_dirs: Option<&str>,
) {
    out.clear();

    // The XDG variables describe the sandbox, where nothing is installed. The
    // host's /usr is bound under /run/host and its home at the usual path, in
    // the order a host session sees them.
    if in_flatpak {
        if let Some(home) = home {
            push_data_dir(out, home, "/.local/share");
            push_data_dir(out, home, "/.local/share/flatpak/exports/share");
        }
        push_data_dir(out, "/var/lib/flatpak/exports/share", "");
        push_data_dir(out, "/run/host/usr/local/share", "");
        push_data_dir(out, "/run/host/usr/share", "");
        return;
    }

    // User data dir (highest priority). XDG_DATA_HOME stands on its own; HOME
    // only supplies the base for the default when it is unset or empty, so a
    // session that sets XDG_DATA_HOME without HOME still resolves.
    match data_home.filter(|s| !s.is_empty()) {
        Some(xdg) => push_data_dir(out, xdg, ""),
        None => {
            if let Some(home) = home {
                push_data_dir(out, home, "/.local/share");
            }
        }
    }

    // System data dirs. Unset and empty both mean the spec's default.
    let system_dirs = data_dirs
        .filter(|s| !s.is_empty())
        .unwrap_or("/usr/local/share:/usr/share");
    for dir in system_dirs.split(':') {
        if dir.is_empty() {
            continue;
        }
        push_data_dir(out, dir, "");
    }
}

/// Push base with suffix appended, dropping it if the two do not fit a path.
fn push_data_dir(out: &mut ArrayVec<ScanPath, MAX_DATA_DIRS>, base: &str, suffix: &str) {
    let mut dir = ScanPath::new();
    if dir
        .push_str(base)
        .and_then(|_| dir.push_str(suffix))
        .is_ok()
    {
        let _ = out.push(dir);
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

/// Search entries and return the top matches ranked by relevance, then name.
/// Relevance is computed once per match into a scratch buffer, which is sorted
/// in place; the caller only ever shows the first few.
pub fn search<'a>(
    entries: &'a [DesktopEntry],
    query: &str,
) -> ArrayVec<&'a DesktopEntry, RESULT_CAP> {
    // The query is lowercased once, for the whole catalog. Every name it is
    // compared against is lowercase already.
    let mut q: ArrayString<NAME_CAP> = ArrayString::new();
    push_lower(&mut q, query);

    let mut scored: ArrayVec<(i32, &DesktopEntry), MAX_ENTRIES> = ArrayVec::new();
    for e in entries {
        let score = e.score(q.as_str());
        if score > 0 && scored.push((score, e)).is_err() {
            break;
        }
    }

    // Relevance descending, then name ascending. The tiebreak compares the
    // lowercased names the entries already carry, so a sort does no case folding
    // of its own.
    scored.as_mut_slice().sort_unstable_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.name_lower.as_str().cmp(b.1.name_lower.as_str()))
    });

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
        DesktopEntry::new(name, exec).unwrap()
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
    fn discovery_recurses_into_subdirs() {
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
        let mut paths: ArrayVec<ScanPath, MAX_ENTRIES> = ArrayVec::new();
        let mut entries: ArrayVec<DesktopEntry, MAX_ENTRIES> = ArrayVec::new();
        collect_paths(&apps, &apps, &mut seen, &mut paths);
        read_entries(&paths, &mut entries);

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
    fn discovery_higher_precedence_id_shadows_lower() {
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
        let mut paths: ArrayVec<ScanPath, MAX_ENTRIES> = ArrayVec::new();
        let mut entries: ArrayVec<DesktopEntry, MAX_ENTRIES> = ArrayVec::new();
        // high precedence scanned first.
        collect_paths(&high, &high, &mut seen, &mut paths);
        collect_paths(&low, &low, &mut seen, &mut paths);
        read_entries(&paths, &mut entries);

        assert!(seen.iter().any(|s| s.as_str() == "foo"));
        assert!(
            entries.is_empty(),
            "a hidden higher-precedence entry shadows the lower one"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The data directories the two layouts produce, as plain strings.
    fn data_dirs(
        in_flatpak: bool,
        home: Option<&str>,
        data_home: Option<&str>,
        data_dirs: Option<&str>,
    ) -> Vec<String> {
        let mut out: ArrayVec<ScanPath, MAX_DATA_DIRS> = ArrayVec::new();
        collect_data_dirs(&mut out, in_flatpak, home, data_home, data_dirs);
        out.iter().map(|d| d.as_str().to_string()).collect()
    }

    #[test]
    fn data_dirs_take_xdg_data_home_first() {
        assert_eq!(
            data_dirs(false, Some("/home/u"), Some("/data"), Some("/a:/b")),
            ["/data", "/a", "/b"]
        );
    }

    #[test]
    fn data_dirs_fall_back_to_home_and_the_spec_defaults() {
        assert_eq!(
            data_dirs(false, Some("/home/u"), None, None),
            ["/home/u/.local/share", "/usr/local/share", "/usr/share"]
        );
    }

    #[test]
    fn data_dirs_treat_an_empty_variable_as_unset() {
        assert_eq!(
            data_dirs(false, Some("/home/u"), Some(""), Some("")),
            ["/home/u/.local/share", "/usr/local/share", "/usr/share"]
        );
    }

    #[test]
    fn data_dirs_skip_empty_entries_in_xdg_data_dirs() {
        assert_eq!(
            data_dirs(false, None, Some("/data"), Some("/a::/b")),
            ["/data", "/a", "/b"]
        );
    }

    #[test]
    fn flatpak_data_dirs_are_the_hosts_not_the_sandboxs() {
        assert_eq!(
            data_dirs(
                true,
                Some("/home/u"),
                Some("/home/u/.var/app/io.github.borgenk.BnkLaunch/data"),
                Some("/app/share:/usr/share"),
            ),
            [
                "/home/u/.local/share",
                "/home/u/.local/share/flatpak/exports/share",
                "/var/lib/flatpak/exports/share",
                "/run/host/usr/local/share",
                "/run/host/usr/share",
            ]
        );
    }

    #[test]
    fn flatpak_data_dirs_without_home_keep_the_system_ones() {
        assert_eq!(
            data_dirs(true, None, None, None),
            [
                "/var/lib/flatpak/exports/share",
                "/run/host/usr/local/share",
                "/run/host/usr/share",
            ]
        );
    }
}
