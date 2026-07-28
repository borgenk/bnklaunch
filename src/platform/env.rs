//! Environment variable lookup for a no_std build.
//!
//! The C library is already linked (freetype and xkbcommon pull it in), so the
//! environment is reached through getenv rather than reimplementing the argv or
//! environ walk. The returned value borrows the process environment, which is
//! stable for the program's lifetime since nothing here sets variables.

#![allow(dead_code)]

use core::ffi::c_char;

use crate::platform::arena::ArrayString;

/// Longest variable name looked up; the names here (XDG_*, HOME, ...) are short.
const NAME_CAP: usize = 128;

// Links libc, which also resolves the C runtime startup and the mem/str
// intrinsics the compiler emits (memcpy, memset, strlen, ...).
#[link(name = "c")]
extern "C" {
    fn getenv(name: *const c_char) -> *const c_char;
}

/// Look up an environment variable by name. None when the name is unset, does
/// not fit NAME_CAP, or the value is not valid UTF-8.
pub fn var(name: &str) -> Option<&'static str> {
    // getenv wants a NUL-terminated name; build one on the stack.
    let mut key: ArrayString<NAME_CAP> = ArrayString::new();
    key.push_str(name).ok()?;
    key.push('\0').ok()?;

    // SAFETY: key holds a NUL-terminated C string for the duration of the call.
    let value = unsafe { getenv(key.as_bytes().as_ptr() as *const c_char) };
    if value.is_null() {
        return None;
    }

    // SAFETY: getenv returns a pointer to a NUL-terminated string inside the
    // environment block, which stays valid and unchanged for the process life.
    let bytes = unsafe { cstr_bytes(value as *const u8) };
    core::str::from_utf8(bytes).ok()
}

/// View a NUL-terminated C string as a byte slice up to the terminator.
///
/// # Safety
/// ptr must point to a NUL-terminated string that remains valid for 'static.
unsafe fn cstr_bytes(ptr: *const u8) -> &'static [u8] {
    let mut len = 0;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    core::slice::from_raw_parts(ptr, len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var_matches_std_lookup() {
        // Compare against std for a variable the environment already has, rather
        // than setting one (set_var is unsafe to call in a threaded harness).
        if let Ok(expected) = std::env::var("PATH") {
            assert_eq!(var("PATH"), Some(expected.as_str()));
        }
    }

    #[test]
    fn var_unset_is_none() {
        assert_eq!(var("BNKLAUNCH_DEFINITELY_UNSET_VAR_42"), None);
    }
}
