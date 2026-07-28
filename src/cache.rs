//! Binary cache of discovered desktop entries plus recent launches.
//!
//! Scanning the application directories takes milliseconds; decoding this takes
//! microseconds. So a start reads the cache, compares the fingerprint it carries
//! against the directories, and only walks them again when the two disagree,
//! which means an application was installed or removed since last time.
//!
//! The file is disposable. load returns None on any problem with it at all
//! (missing, wrong magic, wrong version, truncated, bad utf-8), and the caller
//! simply scans instead. Nothing here needs to be repaired, only rebuilt.
//!
//! Two sections, two write paths. The entries section changes only when the set
//! of installed apps changes, and is written whole through a temp file and
//! rename so a crash mid-write leaves the old cache intact. The recents section
//! trails the entries and changes on every launch; record overwrites just that
//! tail in place, seeking past the entries rather than rewriting them. The tail
//! write is not atomic, so decode parses recents leniently: a torn write there
//! yields empty recents but never discards the entries.
//!
//! Recents are stored as names (the same key as the denylist), not offsets into
//! the entries, so they survive a rescan reordering and an uninstalled app just
//! stops resolving with no pruning.
//!
//! Layout (little-endian, no padding):
//!   magic        4 bytes "BNKL"
//!   version      u8
//!   fingerprint  u64   newest mtime of the scanned application dirs (nanos)
//!   count        u32
//!   per entry:   u16 name len + utf-8, then u16 exec len + utf-8
//!   recents:     u16 count, then per name u16 len + utf-8

use crate::desktop::{Catalog, DesktopEntry, NAME_CAP, RESULT_CAP};
use crate::platform::arena::{ArrayString, ArrayVec};
use crate::platform::bytes::Cursor;
use crate::platform::error::{Error, Result};
use crate::platform::syscall::{self, Fd, AT_FDCWD, O_CLOEXEC, O_WRONLY, SEEK_SET};
use crate::platform::{env, fs};

const MAGIC: &[u8; 4] = b"BNKL";
const VERSION: u8 = 2;

/// Largest cache file, on the read side and the write side alike. Comfortably
/// above a realistic catalog (a full thousand entries average well under a
/// hundred bytes each), but under the 1.3 MB the field caps technically allow,
/// so encode checks every write and a pathological catalog is left uncached
/// rather than half written. A larger file on disk fails to read and falls back
/// to a fresh scan.
const CACHE_MAX: usize = 512 * 1024;

/// Most number of recent launches kept.
pub const RECENT_CAP: usize = 20;

/// Recent launch names, most-recently-used first.
pub type Recents = ArrayVec<ArrayString<NAME_CAP>, RECENT_CAP>;

/// Cached entries, the directory fingerprint they were built from, the recent
/// launches in MRU order, and the byte offset where the recents section begins.
/// The fingerprint lets the background refresh skip a rescan when nothing
/// changed; the offset lets record overwrite the recents tail without touching
/// the entries ahead of it.
pub struct Cached {
    pub entries: Catalog,
    pub fingerprint: u64,
    pub recents: Recents,
    pub recents_offset: u64,
}

fn cache_path() -> Option<ArrayString<{ fs::PATH_CAP }>> {
    let mut p: ArrayString<{ fs::PATH_CAP }> = ArrayString::new();
    if let Some(dir) = env::var("XDG_CACHE_HOME") {
        p.push_str(dir).ok()?;
    } else {
        p.push_str(env::var("HOME")?).ok()?;
        p.push_str("/.cache").ok()?;
    }
    p.push_str("/bnklaunch/apps.bin").ok()?;
    Some(p)
}

/// Load the cache. Returns None when it is missing or the header or entries fail
/// to validate, in which case the caller rebuilds from a fresh scan.
pub fn load() -> Option<Cached> {
    let path = cache_path()?;
    load_from(&path)
}

fn load_from(path: &str) -> Option<Cached> {
    let mut data: ArrayVec<u8, CACHE_MAX> = ArrayVec::new();
    if fs::read_file(path, &mut data).is_err() {
        return None;
    }
    decode(&data)
}

/// Write the whole file: header, entries, and recents. Writes to a temp file and
/// renames over the target so a crash mid-write leaves the old cache intact.
/// Returns the byte offset where the recents section landed, which the caller
/// keeps so a later record overwrites just that tail. A name or exec longer than
/// u16::MAX is dropped; such desktop fields do not occur in practice.
pub fn save(
    entries: &[DesktopEntry],
    fingerprint: u64,
    recents: &[ArrayString<NAME_CAP>],
) -> Result<u64> {
    let path = cache_path().ok_or_else(|| Error::msg("no cache dir"))?;
    save_to(&path, entries, fingerprint, recents)
}

fn save_to(
    path: &str,
    entries: &[DesktopEntry],
    fingerprint: u64,
    recents: &[ArrayString<NAME_CAP>],
) -> Result<u64> {
    // Create the parent directory (everything up to the last separator).
    if let Some((parent, _)) = path.rsplit_once('/') {
        fs::mkdir_p(parent)?;
    }
    let (bytes, recents_offset) = encode(entries, fingerprint, recents)?;
    // Write to a temp file and rename over the target so a crash leaves the old
    // cache intact.
    let mut tmp: ArrayString<{ fs::PATH_CAP }> = ArrayString::new();
    tmp.push_str(path)
        .and_then(|_| tmp.push_str(".tmp"))
        .map_err(|_| Error::msg("path too long"))?;
    fs::write_file(&tmp, &bytes)?;
    fs::rename(&tmp, path)?;
    Ok(recents_offset)
}

/// Record a launch as the most recent and overwrite just the trailing recents
/// section in place. recents_offset comes from load or the last save; the
/// entries ahead of it are never rewritten. The list is updated in memory too so
/// the caller's view stays in sync.
pub fn record(recents_offset: u64, recents: &mut Recents, name: &str) -> Result<()> {
    record_into(recents, name);
    let path = cache_path().ok_or_else(|| Error::msg("no cache dir"))?;
    record_to(&path, recents_offset, recents)
}

fn record_to(path: &str, recents_offset: u64, recents: &[ArrayString<NAME_CAP>]) -> Result<()> {
    let block = encode_recents(recents)?;
    let cp = fs::cpath(path)?;
    // O_CLOEXEC so a launched app never inherits this descriptor, whatever
    // order a caller does the launch and the record in.
    let fd = syscall::openat(AT_FDCWD, &cp, O_WRONLY | O_CLOEXEC, 0);
    if fd < 0 {
        return Err(Error::from_errno(-fd));
    }
    let fd = Fd::new(fd);
    let r = syscall::lseek(fd.as_raw_fd(), recents_offset as i64, SEEK_SET);
    if r < 0 {
        return Err(Error::from_errno(-r as i32));
    }
    fs::write_all(fd.as_raw_fd(), &block)?;
    // Truncate any leftover from a previously longer recents section so a shrink
    // leaves no stale trailing bytes.
    let end = (recents_offset + block.len() as u64) as i64;
    let r = syscall::ftruncate(fd.as_raw_fd(), end);
    if r < 0 {
        return Err(Error::from_errno(-r as i32));
    }
    Ok(())
}

/// Move new_name to the front of the MRU list, dropping any earlier copy, and
/// cap the length. A name that does not fit an entry's capacity is ignored.
fn record_into(names: &mut Recents, new_name: &str) {
    if let Some(pos) = names.iter().position(|n| n.as_str() == new_name) {
        names.remove(pos);
    }
    let mut name: ArrayString<NAME_CAP> = ArrayString::new();
    if name.push_str(new_name).is_err() {
        return;
    }
    // Make room at the front when full by dropping the least-recent entry.
    if names.is_full() {
        names.pop();
    }
    let _ = names.insert(0, name);
}

/// Resolve recent names to entries that still exist, in MRU order. Names without
/// a matching entry are dropped, so an uninstalled app needs no pruning.
pub fn resolve<'a>(
    recents: &[ArrayString<NAME_CAP>],
    entries: &'a [DesktopEntry],
) -> ArrayVec<&'a DesktopEntry, RESULT_CAP> {
    let mut out: ArrayVec<&DesktopEntry, RESULT_CAP> = ArrayVec::new();
    // Names are unique, so a linear lookup matches each recent at most once.
    for name in recents {
        if let Some(entry) = entries.iter().find(|e| e.name.as_str() == name.as_str()) {
            if out.push(entry).is_err() {
                break;
            }
        }
    }
    out
}

/// Encoded recents section: bounded by the recents count, never the full cache.
const RECENTS_ENC_CAP: usize = RECENT_CAP * (2 + NAME_CAP) + 2;

/// Encode the whole file and report where the recents section starts.
///
/// Every write is checked. A catalog whose encoding exceeds CACHE_MAX is an
/// error, not a truncation: save renames its output over the live cache, and a
/// half-written record there would fail decode on every subsequent start, which
/// rescans and writes the same broken file again.
pub(crate) fn encode(
    entries: &[DesktopEntry],
    fingerprint: u64,
    recents: &[ArrayString<NAME_CAP>],
) -> Result<(ArrayVec<u8, CACHE_MAX>, u64)> {
    let mut out: ArrayVec<u8, CACHE_MAX> = ArrayVec::new();
    write_bytes(&mut out, MAGIC)?;
    write_bytes(&mut out, &[VERSION])?;
    write_bytes(&mut out, &fingerprint.to_le_bytes())?;
    write_bytes(&mut out, &(entries.len() as u32).to_le_bytes())?;
    for entry in entries {
        write_string(&mut out, &entry.name)?;
        write_string(&mut out, &entry.exec)?;
    }
    let recents_offset = out.len() as u64;
    write_bytes(&mut out, &encode_recents(recents)?)?;
    Ok((out, recents_offset))
}

fn encode_recents(recents: &[ArrayString<NAME_CAP>]) -> Result<ArrayVec<u8, RECENTS_ENC_CAP>> {
    let count = recents.len().min(RECENT_CAP);

    let mut out: ArrayVec<u8, RECENTS_ENC_CAP> = ArrayVec::new();
    write_bytes(&mut out, &(count as u16).to_le_bytes())?;
    for name in recents.iter().take(RECENT_CAP) {
        write_string(&mut out, name)?;
    }
    Ok(out)
}

fn write_bytes<const N: usize>(out: &mut ArrayVec<u8, N>, bytes: &[u8]) -> Result<()> {
    out.extend_from_slice(bytes)
        .map_err(|_| Error::msg("cache too large to encode"))
}

fn write_string<const N: usize>(out: &mut ArrayVec<u8, N>, s: &str) -> Result<()> {
    // A capacity-bounded string always fits the u16 length prefix.
    write_bytes(out, &(s.len() as u16).to_le_bytes())?;
    write_bytes(out, s.as_bytes())
}

pub(crate) fn decode(data: &[u8]) -> Option<Cached> {
    let mut r = Reader::new(data);
    if r.take(MAGIC.len())? != MAGIC {
        return None;
    }
    if r.u8()? != VERSION {
        return None;
    }
    let fingerprint = r.u64()?;
    let count = r.u32()? as usize;

    // count is untrusted; the catalog has a fixed cap, so a corrupt header
    // cannot request an unbounded allocation. Records past the cap are still
    // read to keep the stream aligned, just not stored.
    let mut entries = Catalog::new();
    for _ in 0..count {
        let name = r.string()?;
        let exec = r.string()?;
        // A stored field longer than an entry can hold is dropped, not
        // truncated; the record is still consumed so the stream stays aligned.
        if let Some(entry) = DesktopEntry::new(name, exec) {
            let _ = entries.push(entry);
        }
    }

    let recents_offset = r.cur.pos() as u64;
    // The recents tail is rewritten in place on launch, outside the atomic
    // temp-and-rename. A torn write there must not cost the entries, so parse it
    // leniently: any problem yields empty recents, never None.
    let recents = decode_recents(&mut r).unwrap_or_default();

    Some(Cached {
        entries,
        fingerprint,
        recents,
        recents_offset,
    })
}

fn decode_recents(r: &mut Reader) -> Option<Recents> {
    let count = (r.u16()? as usize).min(RECENT_CAP);
    let mut out: Recents = ArrayVec::new();
    for _ in 0..count {
        let s = r.string()?;
        let mut name: ArrayString<NAME_CAP> = ArrayString::new();
        // A stored name longer than capacity is dropped, not truncated.
        if name.push_str(s).is_ok() {
            let _ = out.push(name);
        }
    }
    Some(out)
}

/// The cache's little-endian decoder, layered on the shared byte cursor: the
/// cursor does the bounds checks and hands out sub-slices, this adds the
/// endianness. The wire codec layers its own native-endian decoding on the same
/// cursor.
struct Reader<'a> {
    cur: Cursor<'a>,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            cur: Cursor::new(data),
        }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        self.cur.take(n)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Option<u64> {
        let b = self.take(8)?;
        Some(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn string(&mut self) -> Option<&'a str> {
        let b = self.take(2)?;
        let len = u16::from_le_bytes([b[0], b[1]]) as usize;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, exec: &str) -> DesktopEntry {
        DesktopEntry::new(name, exec).unwrap()
    }

    fn bytes(
        entries: &[DesktopEntry],
        fingerprint: u64,
        recents: &[ArrayString<NAME_CAP>],
    ) -> ArrayVec<u8, CACHE_MAX> {
        encode(entries, fingerprint, recents).expect("encode").0
    }

    fn names(list: &[&str]) -> Recents {
        let mut out: Recents = ArrayVec::new();
        for s in list {
            let mut a = ArrayString::new();
            a.push_str(s).unwrap();
            out.push(a).unwrap();
        }
        out
    }

    fn assert_same(a: &[DesktopEntry], b: &[DesktopEntry]) {
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b) {
            assert_eq!(x.name, y.name);
            assert_eq!(x.exec, y.exec);
        }
    }

    fn temp_dir(tag: &str) -> String {
        format!(
            "{}/bnklaunch_cache_{}_{}",
            std::env::temp_dir().display(),
            tag,
            std::process::id()
        )
    }

    #[test]
    fn round_trips_entries_fingerprint_and_recents() {
        let entries = vec![
            entry("Firefox", "firefox %u"),
            entry("Café ☕", "/usr/bin/café --日本語"),
        ];
        let recents = names(&["Café ☕", "Firefox"]);
        let decoded = decode(&bytes(&entries, 0xDEAD_BEEF, &recents)).expect("decode");
        assert_eq!(decoded.fingerprint, 0xDEAD_BEEF);
        assert_same(&decoded.entries, &entries);
        assert_eq!(decoded.recents, recents);
    }

    #[test]
    fn empty_round_trips() {
        let decoded = decode(&bytes(&[], 42, &[])).expect("decode");
        assert_eq!(decoded.fingerprint, 42);
        assert!(decoded.entries.is_empty());
        assert!(decoded.recents.is_empty());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut b = bytes(&[entry("A", "a")], 1, &[]);
        b[0] = b'X';
        assert!(decode(&b).is_none());
    }

    #[test]
    fn rejects_unknown_version() {
        let mut b = bytes(&[entry("A", "a")], 1, &[]);
        b[4] = 99;
        assert!(decode(&b).is_none());
    }

    #[test]
    fn rejects_entries_running_past_end() {
        // Claim more entries than the data holds; decode runs out parsing them.
        let mut b = bytes(&[entry("A", "a")], 1, &[]);
        b[13] = 99; // low byte of the u32 entry count
        assert!(decode(&b).is_none());
    }

    #[test]
    fn rejects_empty_input() {
        assert!(decode(&[]).is_none());
    }

    #[test]
    fn skips_oversized_fields_instead_of_truncating() {
        // A name that fits the cache field but not an entry is dropped on
        // decode, not truncated. Craft the bytes directly since the entry type
        // cannot hold such a name to begin with.
        let long = "x".repeat(crate::desktop::NAME_CAP + 1);
        let mut data: ArrayVec<u8, CACHE_MAX> = ArrayVec::new();
        let _ = data.extend_from_slice(MAGIC);
        let _ = data.push(VERSION);
        let _ = data.extend_from_slice(&1u64.to_le_bytes()); // fingerprint
        let _ = data.extend_from_slice(&2u32.to_le_bytes()); // count
        write_string(&mut data, "Keep").unwrap();
        write_string(&mut data, "ok").unwrap();
        write_string(&mut data, &long).unwrap();
        write_string(&mut data, "exec").unwrap();
        let _ = data.extend_from_slice(&0u16.to_le_bytes()); // no recents

        let decoded = decode(&data).expect("decode");
        assert_same(&decoded.entries, &[entry("Keep", "ok")]);
    }

    #[test]
    fn caps_recents_at_recent_cap() {
        // A file claiming more recents than the cap decodes to exactly the cap.
        let mut data: ArrayVec<u8, CACHE_MAX> = ArrayVec::new();
        let _ = data.extend_from_slice(MAGIC);
        let _ = data.push(VERSION);
        let _ = data.extend_from_slice(&1u64.to_le_bytes()); // fingerprint
        let _ = data.extend_from_slice(&0u32.to_le_bytes()); // no entries
        let _ = data.extend_from_slice(&50u16.to_le_bytes()); // recents count claims 50
        for i in 0..50 {
            write_string(&mut data, &format!("App{}", i)).unwrap();
        }
        let decoded = decode(&data).expect("decode");
        assert_eq!(decoded.recents.len(), RECENT_CAP);
    }

    #[test]
    fn torn_recents_keep_entries() {
        let entries = vec![entry("Firefox", "firefox")];
        let recents = names(&["Firefox"]);
        let (mut b, offset) = encode(&entries, 7, &recents).expect("encode");
        // Chop mid-name in the recents tail: count is intact, the name is not.
        b.truncate(offset as usize + 3);
        let decoded = decode(&b).expect("entries still decode");
        assert_eq!(decoded.fingerprint, 7);
        assert_same(&decoded.entries, &entries);
        assert!(decoded.recents.is_empty());
    }

    /// One entry whose name and exec sit at their field caps: 1284 encoded
    /// bytes, so 409 of them overflow CACHE_MAX.
    fn max_entry(i: usize) -> DesktopEntry {
        let mut name = format!("{:04}", i);
        name.push_str(&"n".repeat(crate::desktop::NAME_CAP - name.len()));
        let exec = "e".repeat(crate::desktop::EXEC_CAP);
        entry(&name, &exec)
    }

    #[test]
    fn encode_at_the_cap_still_round_trips() {
        let mut entries: Vec<DesktopEntry> = Vec::new();
        let mut encoded = 17; // magic, version, fingerprint, count
        while encoded + 1284 + RECENTS_ENC_CAP <= CACHE_MAX {
            entries.push(max_entry(entries.len()));
            encoded += 1284;
        }
        let recents = names(&["0000"]);
        let decoded = decode(&bytes(&entries, 1, &recents)).expect("decode");
        assert_same(&decoded.entries, &entries);
    }

    #[test]
    fn encode_over_the_cap_fails_instead_of_truncating() {
        // The field caps allow a catalog that does not fit CACHE_MAX. Encoding
        // one must be an error: a truncated buffer renamed over the live cache
        // fails decode forever after.
        let entries: Vec<DesktopEntry> = (0..500).map(max_entry).collect();
        assert!(encode(&entries, 1, &[]).is_err());
    }

    #[test]
    fn save_over_the_cap_leaves_the_old_cache_alone() {
        let dir = temp_dir("overflow");
        let _ = std::fs::create_dir_all(&dir);
        let path = format!("{}/apps.bin", dir);

        let good = vec![entry("Firefox", "firefox")];
        save_to(&path, &good, 1, &[]).expect("first save");

        let huge: Vec<DesktopEntry> = (0..500).map(max_entry).collect();
        assert!(save_to(&path, &huge, 2, &[]).is_err());

        // The old cache is untouched and still decodes.
        let decoded = load_from(&path).expect("old cache survives");
        assert_eq!(decoded.fingerprint, 1);
        assert_same(&decoded.entries, &good);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_into_prepends_new_name() {
        let mut v = names(&["A", "B"]);
        record_into(&mut v, "C");
        assert_eq!(v, names(&["C", "A", "B"]));
    }

    #[test]
    fn record_into_bumps_existing_to_front() {
        let mut v = names(&["A", "B", "C"]);
        record_into(&mut v, "B");
        assert_eq!(v, names(&["B", "A", "C"]));
    }

    #[test]
    fn record_into_caps_at_recent_cap() {
        let start: Vec<String> = (0..RECENT_CAP).map(|i| format!("App{}", i)).collect();
        let mut v: Recents = ArrayVec::new();
        for s in &start {
            let mut a = ArrayString::new();
            a.push_str(s).unwrap();
            v.push(a).unwrap();
        }
        record_into(&mut v, "New");
        assert_eq!(v.len(), RECENT_CAP);
        assert_eq!(v[0], "New");
        let oldest = format!("App{}", RECENT_CAP - 1);
        assert!(!v.iter().any(|n| n.as_str() == oldest));
    }

    #[test]
    fn resolve_returns_entries_in_mru_order() {
        let entries = vec![
            entry("Firefox", "x"),
            entry("GIMP", "x"),
            entry("Inkscape", "x"),
        ];
        let recents = names(&["Inkscape", "Firefox"]);
        let got: Vec<_> = resolve(&recents, &entries)
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(got, vec!["Inkscape", "Firefox"]);
    }

    #[test]
    fn resolve_drops_uninstalled_entries() {
        let entries = vec![entry("Firefox", "x")];
        let recents = names(&["Uninstalled", "Firefox"]);
        let resolved = resolve(&recents, &entries);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "Firefox");
    }

    #[test]
    fn record_to_overwrites_recents_in_place() {
        let dir = temp_dir("inplace");
        let path = format!("{dir}/apps.bin");
        let entries = vec![entry("Firefox", "firefox"), entry("GIMP", "gimp")];

        let offset = save_to(&path, &entries, 9, &names(&["Firefox"])).expect("save");

        let mut recents = names(&["Firefox"]);
        record_into(&mut recents, "GIMP");
        record_to(&path, offset, &recents).expect("record");

        let loaded = load_from(&path).expect("load");
        assert_same(&loaded.entries, &entries);
        assert_eq!(loaded.fingerprint, 9);
        assert_eq!(loaded.recents, names(&["GIMP", "Firefox"]));
        assert_eq!(loaded.recents_offset, offset);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_to_truncates_when_recents_shrink() {
        let dir = temp_dir("shrink");
        let path = format!("{dir}/apps.bin");
        let offset = save_to(
            &path,
            &[entry("A", "a")],
            1,
            &names(&["AVeryLongRecentName", "Second"]),
        )
        .expect("save");

        record_to(&path, offset, &names(&["X"])).expect("record");

        let loaded = load_from(&path).expect("load");
        assert_eq!(loaded.recents, names(&["X"]));
        let len = std::fs::metadata(&path).expect("metadata").len();
        let block = encode_recents(&names(&["X"])).expect("encode recents");
        assert_eq!(len, offset + block.len() as u64);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_missing_file_is_none() {
        assert!(load_from("/nonexistent/path/should/not/exist/apps.bin").is_none());
    }
}
