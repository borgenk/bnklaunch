//! Removing user-hidden applications from results.
//!
//! The hidden names come from the config file (hidden <NAME> lines, parsed in
//! config). Applied at presentation time so edits take effect on the next
//! launch without invalidating the desktop entry cache.

use crate::desktop::{Catalog, NAME_CAP};
use crate::platform::arena::ArrayString;

/// Drop entries whose name is in deny (in place, preserving order of the rest).
pub fn apply(entries: &mut Catalog, deny: &[ArrayString<NAME_CAP>]) {
    if deny.is_empty() {
        return;
    }
    entries.retain(|e| !deny.iter().any(|d| d.as_str() == e.name.as_str()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::DesktopEntry;
    use crate::platform::arena::ArrayVec;

    fn entry(name: &str) -> DesktopEntry {
        DesktopEntry::new(name, "x").unwrap()
    }

    fn catalog(names: &[&str]) -> Catalog {
        let mut c = Catalog::new();
        for n in names {
            c.push(entry(n)).unwrap();
        }
        c
    }

    fn deny_list(names: &[&str]) -> ArrayVec<ArrayString<NAME_CAP>, 8> {
        let mut out = ArrayVec::new();
        for n in names {
            let mut s = ArrayString::new();
            s.push_str(n).unwrap();
            out.push(s).unwrap();
        }
        out
    }

    #[test]
    fn apply_removes_matching_and_preserves_order() {
        let mut entries = catalog(&["Firefox", "Qt Linguist", "GIMP", "Qt Assistant", "Inkscape"]);

        apply(&mut entries, &deny_list(&["Qt Linguist", "Qt Assistant"]));

        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Firefox", "GIMP", "Inkscape"]);
    }

    #[test]
    fn apply_empty_denylist_is_noop() {
        let mut entries = catalog(&["Firefox", "GIMP"]);
        let before: Vec<_> = entries.iter().map(|e| e.name).collect();

        apply(&mut entries, &deny_list(&[]));

        let after: Vec<_> = entries.iter().map(|e| e.name).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn apply_is_case_sensitive() {
        let mut entries = catalog(&["Firefox"]);

        apply(&mut entries, &deny_list(&["firefox"]));

        assert_eq!(
            entries.len(),
            1,
            "lowercase 'firefox' must not match 'Firefox'"
        );
    }
}
