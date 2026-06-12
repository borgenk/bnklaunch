//! Keyboard handling backed by libxkbcommon.
//!
//! All XKB-related code lives in this single module: the FFI block, the
//! state struct that owns the C-side resources, the key dispatch table,
//! and the public KeyAction enum the rest of the codebase consumes.
//!
//! The compositor sends an XKB keymap through wl_keyboard.keymap. We
//! hand it to libxkbcommon and let it deal with layout, dead keys, and
//! group switching. Modifier-state changes from wl_keyboard.modifiers
//! flow straight into xkb_state_update_mask.
//!
//! Ctrl+letter shortcuts deliberately dispatch on physical evdev keycode
//! (not the layout-mapped keysym), matching the convention every other
//! app on Linux uses: Ctrl+V is "the V-position key", whatever that key
//! happens to type on the user's layout.

use core::ffi::{c_char, c_int, c_void};
use core::ptr::NonNull;

use crate::arena::ArrayVec;
use crate::error::{Error, Result};
use crate::syscall::{self, Fd};

/// Largest keymap accepted from the compositor.
const KEYMAP_MAX: usize = 128 * 1024;

// ---------------------------------------------------------------------------
// Linux evdev keycodes (subset we handle directly, layout-independent).
// ---------------------------------------------------------------------------

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

    fn xkb_keymap_new_from_string(
        ctx: *mut c_void,
        string: *const c_char,
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

// ---------------------------------------------------------------------------
// Public types.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum KeyAction {
    Char(char),
    Backspace,
    Delete,
    Enter,
    Escape,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    SelectAll,
    Copy,
    Cut,
    Paste,
    None,
}

/// Owns the libxkbcommon context, keymap, and state for the active seat.
///
/// The keymap and state are filled in lazily on the first wl_keyboard.keymap
/// event from the compositor. Until then, key dispatch falls through the
/// special-key fast path (Enter/arrows/etc. are layout-independent) and
/// drops character keys.
pub struct XkbState {
    ctx: NonNull<c_void>,
    keymap: Option<NonNull<c_void>>,
    state: Option<NonNull<c_void>>,
    ctrl_idx: u32,
    shift_idx: u32,
}

// SAFETY: XkbState owns its libxkbcommon objects and is not shared across
// threads. The pointers are not Send/Sync, but the Client that owns this
// struct lives entirely on the main thread, so the auto-derived !Send is
// what we want, and no explicit unsafe impls are added.

impl XkbState {
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

    /// Replace the active keymap (and reset state) using the file descriptor
    /// the compositor sent in wl_keyboard.keymap.
    pub fn load_keymap(&mut self, fd: Fd, size: u32, format: u32) -> Result<()> {
        if format != XKB_KEYMAP_FORMAT_TEXT_V1 {
            return Err(Error::msg("compositor sent unsupported keymap format"));
        }

        // The fd we just received via SCM_RIGHTS shares its seek position
        // with the compositor's file-table entry; not every compositor
        // resets it to 0 before sending. mmap sidesteps that entirely and
        // is the canonical pattern recommended by the Wayland book.
        let raw_fd = fd.as_raw_fd();
        let len = size as usize;
        // SAFETY: PROT_READ + MAP_PRIVATE on a freshly received fd is sound;
        // we check for failure below and free both the mapping and the fd.
        let ptr = unsafe {
            syscall::mmap(
                core::ptr::null_mut(),
                len,
                syscall::PROT_READ,
                syscall::MAP_PRIVATE,
                raw_fd,
                0,
            )
        };
        // The keymap length is compositor-controlled, so a zero or oversized
        // size can legitimately make mmap fail. The raw syscall returns -errno,
        // which mmap_failed detects.
        if syscall::mmap_failed(ptr) {
            let errno = syscall::mmap_errno(ptr);
            // fd closes when its Fd drops on return.
            return Err(Error::from_errno(errno));
        }
        // SAFETY: mmap succeeded (checked above), len matches the mmap, and
        // the mapping is alive until the matching munmap below.
        let mut buf: ArrayVec<u8, KEYMAP_MAX> = ArrayVec::new();
        let copied = buf
            .extend_from_slice(unsafe { core::slice::from_raw_parts(ptr as *const u8, len) })
            .is_ok();
        // SAFETY: ptr/len match the mmap above. The fd closes when fd drops.
        unsafe {
            syscall::munmap(ptr, len);
        }
        if !copied {
            return Err(Error::msg("keymap too large"));
        }
        // The keymap must be passed as a C string. The compositor's payload is
        // null-terminated; strip the trailing zeros, reject any interior null,
        // then append a single terminator.
        while buf.last() == Some(&0) {
            buf.pop();
        }
        if buf.contains(&0) {
            return Err(Error::msg("keymap contains an embedded null"));
        }
        // A keymap that fills the buffer with no room left for the terminator is
        // rejected rather than passed unterminated to libxkbcommon.
        buf.push(0).map_err(|_| Error::msg("keymap too large"))?;

        // SAFETY: ctx is valid (checked NonNull in new). buf is NUL-terminated
        // and stays valid for the duration of the call.
        let keymap_ptr = unsafe {
            xkb_keymap_new_from_string(self.ctx.as_ptr(), buf.as_ptr() as *const c_char, format, 0)
        };
        let keymap = NonNull::new(keymap_ptr)
            .ok_or_else(|| Error::msg("xkb_keymap_new_from_string failed"))?;

        // SAFETY: keymap pointer is valid.
        let state_ptr = unsafe { xkb_state_new(keymap.as_ptr()) };
        let state = NonNull::new(state_ptr).ok_or_else(|| {
            // SAFETY: keymap is non-null and valid; we are giving back its ref.
            unsafe { xkb_keymap_unref(keymap.as_ptr()) };
            Error::msg("xkb_state_new failed")
        })?;

        // Drop the previous keymap+state, install the new ones.
        if let Some(old_state) = self.state.take() {
            // SAFETY: old_state was created by xkb_state_new and never freed.
            unsafe { xkb_state_unref(old_state.as_ptr()) };
        }
        if let Some(old_keymap) = self.keymap.take() {
            // SAFETY: old_keymap was created by xkb_keymap_new_* and never freed.
            unsafe { xkb_keymap_unref(old_keymap.as_ptr()) };
        }
        self.keymap = Some(keymap);
        self.state = Some(state);

        // Cache modifier indices so per-key checks don't re-look-up by name.
        // SAFETY: keymap is valid; the C-string literals are null-terminated.
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

    /// Whether shift is held. Used by the cursor/selection code in client.rs.
    pub fn shift(&self) -> bool {
        self.mod_active(self.shift_idx)
    }

    fn ctrl(&self) -> bool {
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

    /// Translate a Wayland (evdev) keycode plus current modifier state into
    /// a KeyAction. Special keys (Enter, arrows, etc.) and Ctrl+letter
    /// shortcuts dispatch on the physical keycode; everything else asks
    /// libxkbcommon for the resulting UTF-8 input string.
    pub fn keycode_to_action(&self, keycode: u32) -> KeyAction {
        use keycode::*;

        // Modifier keys themselves never produce an action.
        match keycode {
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT | KEY_LEFTCTRL | KEY_RIGHTCTRL | KEY_LEFTALT
            | KEY_RIGHTALT | KEY_CAPSLOCK => return KeyAction::None,
            _ => {}
        }

        // Layout-independent special keys.
        match keycode {
            KEY_ENTER => return KeyAction::Enter,
            KEY_ESC => return KeyAction::Escape,
            KEY_TAB => return KeyAction::Tab,
            KEY_BACKSPACE => return KeyAction::Backspace,
            KEY_DELETE => return KeyAction::Delete,
            KEY_UP => return KeyAction::Up,
            KEY_DOWN => return KeyAction::Down,
            KEY_LEFT => return KeyAction::Left,
            KEY_RIGHT => return KeyAction::Right,
            KEY_HOME => return KeyAction::Home,
            KEY_END => return KeyAction::End,
            _ => {}
        }

        // Ctrl+letter shortcuts dispatch on the physical key, matching
        // the convention used by every other Linux app: Ctrl+V is the
        // V-position key, regardless of layout.
        if self.ctrl() {
            return match keycode {
                KEY_A => KeyAction::SelectAll,
                KEY_C => KeyAction::Copy,
                KEY_X => KeyAction::Cut,
                KEY_V => KeyAction::Paste,
                _ => KeyAction::None,
            };
        }

        // Character input: ask xkb for the UTF-8 string this key produces
        // under the current modifier state.
        let Some(state) = self.state else {
            return KeyAction::None;
        };
        let mut buf = [0u8; 16];
        // SAFETY: state is valid; buf is mutable and large enough for any
        // single keysym's UTF-8 representation (max 4 bytes plus null).
        let n = unsafe {
            xkb_state_key_get_utf8(
                state.as_ptr(),
                keycode + EVDEV_OFFSET,
                buf.as_mut_ptr() as *mut c_char,
                buf.len(),
            )
        };
        if n <= 0 {
            return KeyAction::None;
        }
        // xkb_state_key_get_utf8 returns the number of bytes required (like
        // snprintf), which can exceed our 16-byte buffer for compose/dead-key
        // sequences or a hostile keymap. Clamp before slicing so it can never
        // overrun; oversized (multi-codepoint) results are dropped below anyway.
        let n = (n as usize).min(buf.len());
        let s = match core::str::from_utf8(&buf[..n]) {
            Ok(s) => s,
            Err(_) => return KeyAction::None,
        };
        // We only carry a single char in KeyAction::Char. Compose sequences
        // and supplementary planes that produce multi-codepoint output are
        // dropped; that is fine for a launcher input box.
        let mut chars = s.chars();
        let first = match chars.next() {
            Some(c) => c,
            None => return KeyAction::None,
        };
        if chars.next().is_some() {
            return KeyAction::None;
        }
        // Filter ASCII control characters. We should not have hit this path
        // when ctrl is held, but defend in case (e.g. dead-key results).
        if (first as u32) < 0x20 || first == '\x7f' {
            return KeyAction::None;
        }
        KeyAction::Char(first)
    }
}

impl Drop for XkbState {
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

    /// Special-key dispatch is layout-independent and runs without a keymap.
    /// These tests don't need libxkbcommon to produce useful results.
    #[test]
    fn special_keys_route_correctly() {
        let xkb = XkbState::new().expect("xkb context");
        assert!(matches!(
            xkb.keycode_to_action(keycode::KEY_ENTER),
            KeyAction::Enter
        ));
        assert!(matches!(
            xkb.keycode_to_action(keycode::KEY_BACKSPACE),
            KeyAction::Backspace
        ));
        assert!(matches!(
            xkb.keycode_to_action(keycode::KEY_UP),
            KeyAction::Up
        ));
        assert!(matches!(
            xkb.keycode_to_action(keycode::KEY_HOME),
            KeyAction::Home
        ));
    }

    #[test]
    fn modifier_keys_return_none() {
        let xkb = XkbState::new().expect("xkb context");
        assert!(matches!(
            xkb.keycode_to_action(keycode::KEY_LEFTSHIFT),
            KeyAction::None
        ));
        assert!(matches!(
            xkb.keycode_to_action(keycode::KEY_LEFTCTRL),
            KeyAction::None
        ));
        assert!(matches!(
            xkb.keycode_to_action(keycode::KEY_CAPSLOCK),
            KeyAction::None
        ));
    }

    #[test]
    fn character_keys_without_keymap_are_none() {
        // Without a loaded keymap, character dispatch falls through to None.
        // The launcher only loads a keymap once it receives wl_keyboard.keymap,
        // so this is the legitimate startup state.
        let xkb = XkbState::new().expect("xkb context");
        assert!(matches!(
            xkb.keycode_to_action(keycode::KEY_A),
            KeyAction::None
        ));
    }
}
