//! Keyboard translation backed by libxkbcommon: the FFI block and the state
//! that owns the C-side resources.
//!
//! The compositor sends an XKB keymap through wl_keyboard.keymap. It goes to
//! libxkbcommon, which deals with layout, dead keys, and group switching, and
//! the modifier state from wl_keyboard.modifiers flows straight into
//! xkb_state_update_mask.
//!
//! This answers only machine questions: which character does this key produce
//! under the modifiers in effect, is Ctrl down. What a key means (that Ctrl and
//! the V-position key are a paste) is the app's to decide, and lives there.

use core::ffi::{c_char, c_int, c_void};
use core::ptr::NonNull;

use crate::platform::error::{Error, Result};

/// Linux evdev keycodes, as wl_keyboard reports them. Physical key positions,
/// so they say nothing about what a key types under the user's layout.
pub mod keycode {
    pub const KEY_ESC: u32 = 1;
    pub const KEY_BACKSPACE: u32 = 14;
    pub const KEY_TAB: u32 = 15;
    pub const KEY_A: u32 = 30;
    pub const KEY_C: u32 = 46;
    pub const KEY_V: u32 = 47;
    pub const KEY_X: u32 = 45;
    pub const KEY_ENTER: u32 = 28;
    pub const KEY_LEFTCTRL: u32 = 29;
    pub const KEY_LEFTSHIFT: u32 = 42;
    pub const KEY_RIGHTSHIFT: u32 = 54;
    pub const KEY_LEFTALT: u32 = 56;
    pub const KEY_CAPSLOCK: u32 = 58;
    pub const KEY_RIGHTCTRL: u32 = 97;
    pub const KEY_RIGHTALT: u32 = 100;
    pub const KEY_HOME: u32 = 102;
    pub const KEY_UP: u32 = 103;
    pub const KEY_LEFT: u32 = 105;
    pub const KEY_RIGHT: u32 = 106;
    pub const KEY_END: u32 = 107;
    pub const KEY_DOWN: u32 = 108;
    pub const KEY_DELETE: u32 = 111;
}

/// Wayland passes raw evdev keycodes; XKB expects them offset by +8.
const EVDEV_OFFSET: u32 = 8;

// ---------------------------------------------------------------------------
// libxkbcommon FFI.
// ---------------------------------------------------------------------------
//
// The link attribute below makes libxkbcommon.so.0 a load-time dependency, and
// ld.so resolves the xkb symbols when the binary starts.

const XKB_KEYMAP_FORMAT_TEXT_V1: u32 = 1;

const XKB_STATE_MODS_EFFECTIVE: u32 = 1 << 3;

#[link(name = "xkbcommon")]
unsafe extern "C" {
    fn xkb_context_new(flags: u32) -> *mut c_void;
    fn xkb_context_unref(ctx: *mut c_void);

    /// Takes a length, so the keymap text needs no NUL terminator of its own.
    fn xkb_keymap_new_from_buffer(
        ctx: *mut c_void,
        buffer: *const c_char,
        length: usize,
        format: u32,
        flags: u32,
    ) -> *mut c_void;
    fn xkb_keymap_unref(km: *mut c_void);
    fn xkb_keymap_mod_get_index(km: *mut c_void, name: *const c_char) -> u32;

    fn xkb_state_new(km: *mut c_void) -> *mut c_void;
    fn xkb_state_unref(st: *mut c_void);
    fn xkb_state_update_mask(
        st: *mut c_void,
        depressed_mods: u32,
        latched_mods: u32,
        locked_mods: u32,
        depressed_layout: u32,
        latched_layout: u32,
        locked_layout: u32,
    ) -> u32;
    fn xkb_state_key_get_utf8(st: *mut c_void, key: u32, buf: *mut c_char, size: usize) -> c_int;
    fn xkb_state_mod_index_is_active(st: *mut c_void, idx: u32, components: u32) -> c_int;
}

const XKB_MOD_INVALID: u32 = u32::MAX;

/// Owns the libxkbcommon context, keymap, and state for the active seat.
///
/// The keymap and state are filled in lazily on the first wl_keyboard.keymap
/// event from the compositor. Until then no key produces a character; the
/// layout-independent keys the caller dispatches on keycode still work.
pub struct Xkb {
    ctx: NonNull<c_void>,
    keymap: Option<NonNull<c_void>>,
    state: Option<NonNull<c_void>>,
    ctrl_idx: u32,
    shift_idx: u32,
}

// SAFETY: Xkb owns its libxkbcommon objects and is not shared across
// threads. The pointers are not Send/Sync, but the Client that owns this
// struct lives entirely on the main thread, so the auto-derived !Send is
// what we want, and no explicit unsafe impls are added.

impl Xkb {
    pub fn new() -> Result<Self> {
        // SAFETY: xkb_context_new is safe to call with any flags. Returns
        // null on allocation failure; we check.
        let ctx = unsafe { xkb_context_new(0) };
        let ctx = NonNull::new(ctx).ok_or_else(|| Error::msg("xkb_context_new returned null"))?;
        Ok(Self {
            ctx,
            keymap: None,
            state: None,
            ctrl_idx: XKB_MOD_INVALID,
            shift_idx: XKB_MOD_INVALID,
        })
    }

    /// Install the keymap the compositor sent, as its raw bytes.
    ///
    /// Taking a slice rather than the fd keeps the mapping out of here: the
    /// caller maps the descriptor and hands over what it holds.
    pub fn load_keymap(&mut self, bytes: &[u8], format: u32) -> Result<()> {
        if format != XKB_KEYMAP_FORMAT_TEXT_V1 {
            return Err(Error::msg("compositor sent an unsupported keymap format"));
        }

        // The payload is NUL-terminated and padded. from_buffer takes a length,
        // so trim the trailing NULs and hand it exactly the text: no copy into a
        // fixed buffer, no re-terminating, and no cap of our own to exceed.
        let end = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        let text = &bytes[..end];
        if text.is_empty() {
            return Err(Error::msg("compositor sent an empty keymap"));
        }

        // SAFETY: ctx is valid (NonNull, checked in new). text is a live borrow
        // of len bytes, which is exactly what is passed, and libxkbcommon only
        // reads it during the call.
        let keymap = unsafe {
            xkb_keymap_new_from_buffer(
                self.ctx.as_ptr(),
                text.as_ptr() as *const c_char,
                text.len(),
                format,
                0,
            )
        };
        let keymap =
            NonNull::new(keymap).ok_or_else(|| Error::msg("xkb_keymap_new_from_buffer failed"))?;

        // SAFETY: keymap is valid.
        let state = unsafe { xkb_state_new(keymap.as_ptr()) };
        let state = match NonNull::new(state) {
            Some(s) => s,
            None => {
                // SAFETY: keymap is valid, and this releases the ref we hold.
                unsafe { xkb_keymap_unref(keymap.as_ptr()) };
                return Err(Error::msg("xkb_state_new failed"));
            }
        };

        // Drop the previous keymap and state, install the new ones.
        if let Some(old_state) = self.state.take() {
            // SAFETY: old_state came from xkb_state_new and has not been freed.
            unsafe { xkb_state_unref(old_state.as_ptr()) };
        }
        if let Some(old_keymap) = self.keymap.take() {
            // SAFETY: old_keymap came from xkb_keymap_new_* and has not been freed.
            unsafe { xkb_keymap_unref(old_keymap.as_ptr()) };
        }
        self.keymap = Some(keymap);
        self.state = Some(state);

        // Cache the modifier indices so a per-key check is not a lookup by name.
        // SAFETY: keymap is valid; the C string literals are NUL-terminated.
        unsafe {
            self.ctrl_idx = xkb_keymap_mod_get_index(keymap.as_ptr(), c"Control".as_ptr());
            self.shift_idx = xkb_keymap_mod_get_index(keymap.as_ptr(), c"Shift".as_ptr());
        }
        Ok(())
    }

    /// Forward the modifier mask from wl_keyboard.modifiers to xkb.
    pub fn update_modifiers(&self, depressed: u32, latched: u32, locked: u32, group: u32) {
        let Some(state) = self.state else { return };
        // SAFETY: state is valid (NonNull, owned).
        unsafe {
            xkb_state_update_mask(state.as_ptr(), depressed, latched, locked, 0, 0, group);
        }
    }

    /// Whether Shift is in effect.
    pub fn shift_active(&self) -> bool {
        self.mod_active(self.shift_idx)
    }

    /// Whether Ctrl is in effect.
    pub fn ctrl_active(&self) -> bool {
        self.mod_active(self.ctrl_idx)
    }

    fn mod_active(&self, idx: u32) -> bool {
        if idx == XKB_MOD_INVALID {
            return false;
        }
        let Some(state) = self.state else {
            return false;
        };
        // SAFETY: state is valid; idx came from xkb_keymap_mod_get_index.
        unsafe { xkb_state_mod_index_is_active(state.as_ptr(), idx, XKB_STATE_MODS_EFFECTIVE) > 0 }
    }

    /// The character this key types under the modifiers currently in effect, if
    /// it types one at all.
    ///
    /// None for a key that produces no text (a modifier, an arrow), for a
    /// control character, and for a compose or dead-key sequence that yields
    /// more than one code point: a launcher's input box carries single chars.
    pub fn key_char(&self, keycode: u32) -> Option<char> {
        let state = self.state?;

        let mut buf = [0u8; 16];
        // SAFETY: state is valid (NonNull, owned). buf is a live mutable array
        // and its true length is passed, so libxkbcommon writes within it. A
        // keycode near u32::MAX would wrap the evdev offset, which is harmless
        // (xkb reports no symbol) but panics a debug build, hence the saturate.
        let n = unsafe {
            xkb_state_key_get_utf8(
                state.as_ptr(),
                keycode.saturating_add(EVDEV_OFFSET),
                buf.as_mut_ptr() as *mut c_char,
                buf.len(),
            )
        };
        if n <= 0 {
            return None;
        }
        // Like snprintf, this returns the length the result needed, which can
        // exceed the buffer for a long compose sequence or a hostile keymap.
        // Clamp before slicing so it can never read past what was written.
        let n = (n as usize).min(buf.len());
        let text = core::str::from_utf8(&buf[..n]).ok()?;

        let mut chars = text.chars();
        let first = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        if (first as u32) < 0x20 || first == '\x7f' {
            return None;
        }
        Some(first)
    }
}

impl Drop for Xkb {
    fn drop(&mut self) {
        // SAFETY: each pointer was obtained from the matching xkb_*_new
        // call and has not been freed yet; we own the only references.
        unsafe {
            if let Some(s) = self.state.take() {
                xkb_state_unref(s.as_ptr());
            }
            if let Some(k) = self.keymap.take() {
                xkb_keymap_unref(k.as_ptr());
            }
            xkb_context_unref(self.ctx.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest keymap libxkbcommon will compile that still maps a key: the
    /// A-position key (evdev 30, which is xkb 38 once offset) types "a", or "A"
    /// with Shift.
    const KEYMAP: &str = r#"xkb_keymap {
        xkb_keycodes "test" { <A> = 38; };
        xkb_types "test" { include "basic" };
        xkb_compat "test" { include "basic" };
        xkb_symbols "test" { key <A> { [ a, A ] }; };
    };"#;

    #[test]
    fn without_a_keymap_no_key_types_anything() {
        // The launcher runs in this state until wl_keyboard.keymap arrives.
        let xkb = Xkb::new().expect("xkb context");
        assert_eq!(xkb.key_char(keycode::KEY_A), None);
        assert!(!xkb.shift_active());
        assert!(!xkb.ctrl_active());
    }

    #[test]
    fn a_loaded_keymap_translates_a_key_to_its_character() {
        let mut xkb = Xkb::new().expect("xkb context");
        xkb.load_keymap(KEYMAP.as_bytes(), XKB_KEYMAP_FORMAT_TEXT_V1)
            .expect("load keymap");
        assert_eq!(xkb.key_char(keycode::KEY_A), Some('a'));
    }

    #[test]
    fn the_trailing_nuls_the_compositor_sends_are_not_part_of_the_keymap() {
        // The keymap arrives NUL-terminated and padded out. from_buffer takes a
        // length, so those bytes have to be trimmed off rather than compiled.
        let mut bytes = KEYMAP.as_bytes().to_vec();
        bytes.extend_from_slice(&[0u8; 5]);

        let mut xkb = Xkb::new().expect("xkb context");
        xkb.load_keymap(&bytes, XKB_KEYMAP_FORMAT_TEXT_V1)
            .expect("load keymap");
        assert_eq!(xkb.key_char(keycode::KEY_A), Some('a'));
    }

    #[test]
    fn a_second_keymap_replaces_the_first() {
        // The compositor re-sends the keymap when the layout changes.
        let mut xkb = Xkb::new().expect("xkb context");
        xkb.load_keymap(KEYMAP.as_bytes(), XKB_KEYMAP_FORMAT_TEXT_V1)
            .expect("first keymap");
        let swapped = KEYMAP.replace("[ a, A ]", "[ z, Z ]");
        xkb.load_keymap(swapped.as_bytes(), XKB_KEYMAP_FORMAT_TEXT_V1)
            .expect("second keymap");
        assert_eq!(xkb.key_char(keycode::KEY_A), Some('z'));
    }

    #[test]
    fn a_keymap_that_does_not_compile_is_an_error() {
        let mut xkb = Xkb::new().expect("xkb context");
        assert!(xkb
            .load_keymap(b"this is not a keymap", XKB_KEYMAP_FORMAT_TEXT_V1)
            .is_err());
        assert!(xkb.load_keymap(&[], XKB_KEYMAP_FORMAT_TEXT_V1).is_err());
        // A key still types nothing: the failed load left no half-built state.
        assert_eq!(xkb.key_char(keycode::KEY_A), None);
    }

    #[test]
    fn an_unknown_keymap_format_is_refused_before_any_parsing() {
        let mut xkb = Xkb::new().expect("xkb context");
        assert!(xkb.load_keymap(KEYMAP.as_bytes(), 99).is_err());
    }

    #[test]
    fn a_keycode_near_the_top_of_the_range_does_not_wrap_the_evdev_offset() {
        let xkb = Xkb::new().expect("xkb context");
        assert_eq!(xkb.key_char(u32::MAX), None);
    }
}
