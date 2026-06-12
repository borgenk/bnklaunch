//! High-level Wayland client state machine.

use crate::arena::{ArrayString, ArrayVec};
use crate::clipboard;
use crate::error::{Error, Result};
use crate::time::Instant;
use crate::wire::WIRE_STR_CAP;

use crate::protocol::layer_shell::{self, KeyboardInteractivity, Layer};
use crate::protocol::{self, interface, ShmFormat};
use crate::shm::PixelBuffer;
use crate::socket::{Message, WaylandSocket};
use crate::xkb::{KeyAction, XkbState};

/// Most recycled ids held at once.
const MAX_IDS: usize = 256;
/// Most globals the compositor advertises that we track.
const MAX_GLOBALS: usize = 64;
/// Most buffers retired (awaiting release) at once.
const MAX_RETIRED: usize = 8;
/// Capacity of the search input field.
pub const INPUT_CAP: usize = 1024;

/// Object ID allocator - starts at 2 since 1 is wl_display.
pub struct IdAllocator {
    next_id: u32,
    /// Ids the compositor has reported as deleted via wl_display.delete_id,
    /// handed back out before bumping next_id so a long session that creates
    /// and destroys many objects does not run the id space up.
    free: ArrayVec<u32, MAX_IDS>,
}

impl IdAllocator {
    pub fn new() -> Self {
        Self {
            next_id: 2,
            free: ArrayVec::new(),
        }
    }

    pub fn allocate(&mut self) -> u32 {
        if let Some(id) = self.free.pop() {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Return an id the compositor reported as deleted to the free pool.
    pub fn recycle(&mut self, id: u32) {
        let _ = self.free.push(id);
    }
}

/// Information about a global object advertised by the compositor.
#[derive(Debug, Clone)]
pub struct Global {
    pub name: u32,
    pub interface: ArrayString<WIRE_STR_CAP>,
    pub version: u32,
}

/// Whether the launcher binds this interface, so the global is worth tracking.
/// A multi-monitor session advertises dozens of wl_output and per-output
/// globals; filtering at advertise time keeps that flood from pushing wl_seat
/// or the layer shell past the bounded globals array and dropping it.
fn is_bindable_interface(iface: &str) -> bool {
    iface == interface::WL_COMPOSITOR
        || iface == interface::WL_SHM
        || iface == interface::WL_SEAT
        || iface == interface::ZWLR_LAYER_SHELL_V1
        || iface == interface::WL_DATA_DEVICE_MANAGER
}

/// Bound object IDs for essential interfaces.
#[derive(Debug, Default)]
pub struct Bindings {
    pub registry: Option<u32>,
    pub compositor: Option<u32>,
    pub shm: Option<u32>,
    pub seat: Option<u32>,
    pub layer_shell: Option<u32>,
    pub data_device_manager: Option<u32>,
}

/// Text MIME types accepted from the clipboard, ordered worst to best so the
/// derived Ord picks the richest one advertised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TextMime {
    Plain,
    Utf8,
}

impl TextMime {
    fn as_str(self) -> &'static str {
        match self {
            TextMime::Plain => "text/plain",
            TextMime::Utf8 => "text/plain;charset=utf-8",
        }
    }

    /// Map an advertised MIME string to one we accept, if any.
    fn from_mime(s: &str) -> Option<TextMime> {
        match s {
            "text/plain;charset=utf-8" => Some(TextMime::Utf8),
            "text/plain" => Some(TextMime::Plain),
            _ => None,
        }
    }
}

/// Clipboard state for the core wl_data_device protocol.
pub struct ClipboardState {
    /// Data device ID (per-seat clipboard access)
    pub device_id: Option<u32>,
    /// Active data source ID we own, for serving copy requests
    pub source_id: Option<u32>,
    /// Text offered through the data source we own
    pub source_text: Option<ArrayString<INPUT_CAP>>,
    /// Offer the compositor is describing with offer() events, before it marks
    /// the offer as the selection.
    pub pending_offer: Option<u32>,
    /// Best text MIME seen on the pending offer so far.
    pub pending_mime: Option<TextMime>,
    /// Offer the compositor marked as the clipboard selection, with its MIME.
    pub selection_offer: Option<u32>,
    pub selection_mime: Option<TextMime>,
}

/// Upper bound on a surface dimension accepted from a configure event. A buggy
/// or hostile compositor could otherwise echo an enormous size; the buffer math
/// is checked, but capping here keeps a usable surface instead of an error.
const MAX_SURFACE_DIM: u32 = 16384;

/// Surface state.
pub struct Surface {
    pub id: u32,
    /// Layer surface ID
    pub layer_surface_id: u32,
    pub configured: bool,
    pub width: u32,
    pub height: u32,
}

/// A buffer retired on resize, kept mapped until the compositor sends
/// wl_buffer.release. Tearing it down sooner could unmap memory the compositor
/// is still scanning out, flashing a stale or blank frame.
struct RetiredBuffer {
    buffer_id: u32,
    pool_id: u32,
    _buffer: PixelBuffer,
}

/// Keyboard state. Owns an XkbState that handles layout-aware translation;
/// the compositor populates the keymap on the first wl_keyboard.keymap event.
pub struct Keyboard {
    pub id: Option<u32>,
    pub focused: bool,
    pub xkb: XkbState,
}

/// Key repeat state.
pub struct KeyRepeat {
    /// Delay before repeat starts (milliseconds)
    pub delay_ms: u32,
    /// Repeat rate (repeats per second)
    pub rate: u32,
    /// Currently held key (keycode)
    pub held_key: Option<u32>,
    /// Time when key was pressed
    pub press_time: Option<Instant>,
    /// Time of last repeat
    pub last_repeat: Option<Instant>,
    /// Whether initial delay has passed
    pub repeating: bool,
}

impl Default for KeyRepeat {
    fn default() -> Self {
        Self {
            delay_ms: 400,
            rate: 25,
            held_key: None,
            press_time: None,
            last_repeat: None,
            repeating: false,
        }
    }
}

impl KeyRepeat {
    /// Check if it's time to emit a repeat.
    pub fn should_repeat(&self) -> bool {
        if self.held_key.is_none() {
            return false;
        }
        let Some(press_time) = self.press_time else {
            return false;
        };

        let now = Instant::now();
        let elapsed = now.ms_since(press_time) as u32;

        if !self.repeating {
            elapsed >= self.delay_ms
        } else if let Some(last) = self.last_repeat {
            // rate is repeats per second, so the gap between repeats is
            // 1000 / rate milliseconds.
            if self.rate == 0 {
                return false;
            }
            let interval_ms = 1000 / self.rate;
            now.ms_since(last) as u32 >= interval_ms
        } else {
            true
        }
    }
}

/// Pointer (mouse) state.
#[derive(Default)]
pub struct Pointer {
    pub id: Option<u32>,
    pub x: f64,
    pub y: f64,
    pub button_pressed: bool,
    /// Set when a click just happened (cleared by main loop)
    pub clicked: bool,
    /// Set when dragging with button held (cleared by main loop)
    pub dragging: bool,
    /// Timestamp of the last click for double-click detection
    pub last_click_time: Option<Instant>,
    /// Click count for double/triple click detection
    pub click_count: u32,
}

/// Deferred clipboard operation.
pub enum ClipboardOp {
    None,
    Copy(ArrayString<INPUT_CAP>),
    Cut(ArrayString<INPUT_CAP>),
    Paste,
}

/// Pending action from input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingAction {
    None,
    SelectUp,
    SelectDown,
    Launch,
}

/// Convert char offset to byte offset in a string.
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Client state.
pub struct Client {
    pub socket: WaylandSocket,
    pub ids: IdAllocator,
    pub globals: ArrayVec<Global, MAX_GLOBALS>,
    pub bindings: Bindings,
    pub surface: Option<Surface>,
    pub buffer: Option<PixelBuffer>,
    pub pool_id: Option<u32>,
    pub buffer_id: Option<u32>,
    /// Buffers awaiting wl_buffer.release before their memory is unmapped.
    retired_buffers: ArrayVec<RetiredBuffer, MAX_RETIRED>,
    pub running: bool,
    /// Pending sync callback ID
    sync_callback: Option<u32>,
    /// Sync completed flag
    sync_done: bool,
    /// Keyboard state
    pub keyboard: Keyboard,
    /// Key repeat state
    pub key_repeat: KeyRepeat,
    /// Pointer state
    pub pointer: Pointer,
    /// Current input text
    pub input_text: ArrayString<INPUT_CAP>,
    /// Cursor position (char offset)
    pub cursor: usize,
    /// Selection anchor (char offset), None = no selection
    pub selection_anchor: Option<usize>,
    /// Deferred clipboard operation
    pub clipboard_op: ClipboardOp,
    /// Native Wayland clipboard state
    pub clipboard: ClipboardState,
    /// Serial of the most recent input event, required to set the selection.
    pub last_serial: u32,
    /// Flag indicating input changed (for redraw)
    pub input_changed: bool,
    /// Pending action from input
    pub pending_action: PendingAction,
}

impl Client {
    /// Connect to the Wayland compositor.
    pub fn connect() -> Result<Self> {
        let socket = WaylandSocket::connect()?;
        Ok(Self {
            socket,
            ids: IdAllocator::new(),
            globals: ArrayVec::new(),
            bindings: Bindings::default(),
            surface: None,
            buffer: None,
            pool_id: None,
            buffer_id: None,
            retired_buffers: ArrayVec::new(),
            running: true,
            sync_callback: None,
            sync_done: false,
            keyboard: Keyboard {
                id: None,
                focused: false,
                xkb: XkbState::new()?,
            },
            key_repeat: KeyRepeat::default(),
            pointer: Pointer::default(),
            input_text: ArrayString::new(),
            cursor: 0,
            selection_anchor: None,
            clipboard_op: ClipboardOp::None,
            clipboard: ClipboardState {
                device_id: None,
                source_id: None,
                source_text: None,
                pending_offer: None,
                pending_mime: None,
                selection_offer: None,
                selection_mime: None,
            },
            last_serial: 0,
            input_changed: false,
            pending_action: PendingAction::None,
        })
    }

    /// Check if layer shell is available.
    pub fn has_layer_shell(&self) -> bool {
        self.globals
            .iter()
            .any(|g| g.interface == interface::ZWLR_LAYER_SHELL_V1)
    }

    /// Send a sync request and wait for the callback.
    pub fn roundtrip(&mut self) -> Result<()> {
        let callback_id = self.ids.allocate();
        self.sync_callback = Some(callback_id);
        self.sync_done = false;
        protocol::display::sync(&mut self.socket, callback_id)?;
        self.socket.flush()?;

        while !self.sync_done {
            self.dispatch()?;
        }
        self.sync_callback = None;
        Ok(())
    }

    /// Initialize the connection by getting the registry.
    pub fn init(&mut self) -> Result<()> {
        let registry_id = self.ids.allocate();
        self.bindings.registry = Some(registry_id);
        protocol::display::get_registry(&mut self.socket, registry_id)?;
        self.socket.flush()?;
        Ok(())
    }

    /// Process a single message from the compositor.
    pub fn dispatch(&mut self) -> Result<()> {
        let msg = self.socket.read_message()?;
        self.handle_message(msg)?;
        Ok(())
    }

    /// Handle a received message.
    fn handle_message(&mut self, msg: Message) -> Result<()> {
        let registry_id = self.bindings.registry;

        // wl_callback.done (for sync)
        if Some(msg.object_id) == self.sync_callback {
            if msg.opcode == protocol::wl_callback_event::DONE {
                self.sync_done = true;
            }
            return Ok(());
        }

        // wl_display events
        if msg.object_id == protocol::object::WL_DISPLAY {
            match msg.opcode {
                protocol::wl_display_event::ERROR => {
                    // Log the error (object id, code, message); otherwise a
                    // protocol error is a silent teardown.
                    let mut parser = msg.parser();
                    if let (Ok(object), Ok(code), Ok(message)) =
                        (parser.get_u32(), parser.get_u32(), parser.get_string())
                    {
                        crate::elog!(
                            "bnklaunch: protocol error from object {object} (code {code}): {message}"
                        );
                    }
                    self.running = false;
                }
                protocol::wl_display_event::DELETE_ID => {
                    let mut parser = msg.parser();
                    if let Ok(id) = parser.get_u32() {
                        self.ids.recycle(id);
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        // wl_registry events
        if Some(msg.object_id) == registry_id {
            match msg.opcode {
                protocol::wl_registry_event::GLOBAL => {
                    let mut parser = msg.parser();
                    let name = parser.get_u32()?;
                    let interface = parser.get_string()?;
                    let version = parser.get_u32()?;
                    if is_bindable_interface(interface.as_str()) {
                        let _ = self.globals.push(Global {
                            name,
                            interface,
                            version,
                        });
                    }
                }
                protocol::wl_registry_event::GLOBAL_REMOVE => {
                    let mut parser = msg.parser();
                    let name = parser.get_u32()?;
                    if let Some(i) = self.globals.iter().position(|g| g.name == name) {
                        self.globals.swap_remove(i);
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        // Layer surface events
        if let Some(ref mut surface) = self.surface {
            if msg.object_id == surface.layer_surface_id {
                match msg.opcode {
                    layer_shell::zwlr_layer_surface_v1_event::CONFIGURE => {
                        let mut parser = msg.parser();
                        let serial = parser.get_u32()?;
                        let width = parser.get_u32()?;
                        let height = parser.get_u32()?;
                        if (1..=MAX_SURFACE_DIM).contains(&width)
                            && (1..=MAX_SURFACE_DIM).contains(&height)
                        {
                            surface.width = width;
                            surface.height = height;
                        }
                        layer_shell::ack_configure(
                            &mut self.socket,
                            surface.layer_surface_id,
                            serial,
                        )?;
                        surface.configured = true;
                    }
                    layer_shell::zwlr_layer_surface_v1_event::CLOSED => {
                        self.running = false;
                    }
                    _ => {}
                }
                return Ok(());
            }
        }

        // A retired buffer's release means the compositor is done scanning it
        // out, so unmapping is finally safe.
        if let Some(idx) = self
            .retired_buffers
            .iter()
            .position(|b| b.buffer_id == msg.object_id)
        {
            if msg.opcode == protocol::wl_buffer_event::RELEASE {
                if let Some(retired) = self.retired_buffers.swap_remove(idx) {
                    protocol::shm::buffer_destroy(&mut self.socket, retired.buffer_id)?;
                    protocol::shm::pool_destroy(&mut self.socket, retired.pool_id)?;
                    self.socket.flush()?;
                    // retired._buffer drops here: munmap + close the memfd.
                }
            }
            return Ok(());
        }

        // The active buffer is reused in place, so its release needs no action.
        if Some(msg.object_id) == self.buffer_id {
            return Ok(());
        }

        // wl_seat events
        if Some(msg.object_id) == self.bindings.seat {
            match msg.opcode {
                protocol::wl_seat_event::CAPABILITIES => {
                    let mut parser = msg.parser();
                    let caps = parser.get_u32()?;
                    // Request keyboard if available
                    if (caps & protocol::wl_seat_capability::KEYBOARD) != 0
                        && self.keyboard.id.is_none()
                    {
                        let keyboard_id = self.ids.allocate();
                        protocol::seat::get_keyboard(&mut self.socket, msg.object_id, keyboard_id)?;
                        self.keyboard.id = Some(keyboard_id);
                    }
                    // Request pointer if available
                    if (caps & protocol::wl_seat_capability::POINTER) != 0
                        && self.pointer.id.is_none()
                    {
                        let pointer_id = self.ids.allocate();
                        protocol::seat::get_pointer(&mut self.socket, msg.object_id, pointer_id)?;
                        self.pointer.id = Some(pointer_id);
                    }
                    self.socket.flush()?;
                }
                protocol::wl_seat_event::NAME => {}
                _ => {}
            }
            return Ok(());
        }

        // wl_keyboard events
        if Some(msg.object_id) == self.keyboard.id {
            match msg.opcode {
                protocol::wl_keyboard_event::KEYMAP => {
                    let mut parser = msg.parser();
                    let format = parser.get_u32()?;
                    let size = parser.get_u32()?;
                    if let Some(fd) = self.socket.take_fd() {
                        if let Err(e) = self.keyboard.xkb.load_keymap(fd, size, format) {
                            crate::elog!("bnklaunch: failed to load keymap: {e}");
                        }
                    }
                }
                protocol::wl_keyboard_event::ENTER => {
                    let mut parser = msg.parser();
                    self.last_serial = parser.get_u32()?;
                    self.keyboard.focused = true;
                }
                protocol::wl_keyboard_event::LEAVE => {
                    self.keyboard.focused = false;
                    self.running = false;
                }
                protocol::wl_keyboard_event::KEY => {
                    let mut parser = msg.parser();
                    self.last_serial = parser.get_u32()?;
                    let _time = parser.get_u32()?;
                    let key = parser.get_u32()?;
                    let state = parser.get_u32()?;

                    if state == protocol::wl_keyboard_key_state::PRESSED {
                        self.key_repeat.held_key = Some(key);
                        self.key_repeat.press_time = Some(Instant::now());
                        self.key_repeat.last_repeat = None;
                        self.key_repeat.repeating = false;

                        self.handle_key_action(key);
                    } else {
                        if self.key_repeat.held_key == Some(key) {
                            self.key_repeat.held_key = None;
                            self.key_repeat.press_time = None;
                            self.key_repeat.repeating = false;
                        }
                    }
                }
                protocol::wl_keyboard_event::MODIFIERS => {
                    let mut parser = msg.parser();
                    let _serial = parser.get_u32()?;
                    let mods_depressed = parser.get_u32()?;
                    let mods_latched = parser.get_u32()?;
                    let mods_locked = parser.get_u32()?;
                    let group = parser.get_u32()?;
                    self.keyboard.xkb.update_modifiers(
                        mods_depressed,
                        mods_latched,
                        mods_locked,
                        group,
                    );
                }
                protocol::wl_keyboard_event::REPEAT_INFO => {
                    let mut parser = msg.parser();
                    let rate = parser.get_i32()?;
                    let delay = parser.get_i32()?;
                    if rate > 0 {
                        self.key_repeat.rate = rate as u32;
                    }
                    if delay > 0 {
                        self.key_repeat.delay_ms = delay as u32;
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        // wl_pointer events
        if Some(msg.object_id) == self.pointer.id {
            match msg.opcode {
                protocol::wl_pointer_event::ENTER => {
                    let mut parser = msg.parser();
                    self.last_serial = parser.get_u32()?;
                    let _surface = parser.get_u32()?;
                    self.pointer.x = parser.get_fixed()?;
                    self.pointer.y = parser.get_fixed()?;
                }
                protocol::wl_pointer_event::LEAVE => {}
                protocol::wl_pointer_event::MOTION => {
                    let mut parser = msg.parser();
                    let _time = parser.get_u32()?;
                    self.pointer.x = parser.get_fixed()?;
                    self.pointer.y = parser.get_fixed()?;
                    if self.pointer.button_pressed {
                        self.pointer.dragging = true;
                        self.input_changed = true;
                    }
                }
                protocol::wl_pointer_event::BUTTON => {
                    let mut parser = msg.parser();
                    self.last_serial = parser.get_u32()?;
                    let _time = parser.get_u32()?;
                    let button = parser.get_u32()?;
                    let state = parser.get_u32()?;

                    if button == protocol::button::BTN_LEFT {
                        if state == protocol::wl_pointer_button_state::PRESSED {
                            self.pointer.button_pressed = true;
                            // Double/triple click detection
                            let now = Instant::now();
                            let is_repeat_click = self
                                .pointer
                                .last_click_time
                                .map(|t| now.ms_since(t) < 400)
                                .unwrap_or(false);
                            if is_repeat_click {
                                self.pointer.click_count += 1;
                            } else {
                                self.pointer.click_count = 1;
                            }
                            self.pointer.last_click_time = Some(now);
                            self.pointer.clicked = true;
                            self.input_changed = true;
                        } else {
                            self.pointer.button_pressed = false;
                            self.pointer.dragging = false;
                        }
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        // Data device events advertise and select clipboard offers. The
        // compositor sends data_offer, then offer() per MIME on that offer,
        // then selection() naming it as the clipboard (or null to clear).
        if Some(msg.object_id) == self.clipboard.device_id {
            match msg.opcode {
                protocol::data_device::device_event::DATA_OFFER => {
                    let mut parser = msg.parser();
                    self.clipboard.pending_offer = Some(parser.get_u32()?);
                    self.clipboard.pending_mime = None;
                }
                protocol::data_device::device_event::SELECTION => {
                    let mut parser = msg.parser();
                    let offer_id = parser.get_u32()?;
                    if let Some(old) = self.clipboard.selection_offer.take() {
                        let _ = protocol::data_device::offer_destroy(&mut self.socket, old);
                    }
                    if offer_id == 0 {
                        self.clipboard.selection_mime = None;
                    } else {
                        self.clipboard.selection_offer = Some(offer_id);
                        self.clipboard.selection_mime = self.clipboard.pending_mime;
                    }
                    self.clipboard.pending_offer = None;
                }
                _ => {}
            }
            return Ok(());
        }

        // MIME types advertised on the offer currently being described.
        if Some(msg.object_id) == self.clipboard.pending_offer
            && msg.opcode == protocol::data_device::offer_event::OFFER
        {
            let mut parser = msg.parser();
            let mime = parser.get_string()?;
            if let Some(m) = TextMime::from_mime(mime.as_str()) {
                self.clipboard.pending_mime = Some(match self.clipboard.pending_mime {
                    Some(cur) => cur.max(m),
                    None => m,
                });
            }
            return Ok(());
        }

        // Data source events: serve copied text and notice ownership loss.
        if Some(msg.object_id) == self.clipboard.source_id {
            match msg.opcode {
                protocol::data_device::source_event::SEND => {
                    // Compositor asks us to write clipboard data to an fd
                    let mut parser = msg.parser();
                    let _mime = parser.get_string()?;
                    if let Some(fd) = self.socket.take_fd() {
                        let raw_fd = fd.as_raw_fd();
                        if let Some(ref text) = self.clipboard.source_text {
                            write_all_to_fd(raw_fd, text.as_bytes());
                        }
                        // fd closes when it drops at the end of this block.
                    }
                }
                protocol::data_device::source_event::CANCELLED => {
                    // Another app took the clipboard
                    self.clipboard.source_id = None;
                    self.clipboard.source_text = None;
                }
                _ => {}
            }
            return Ok(());
        }

        Ok(())
    }

    /// Handle a key action (used for initial press and repeat).
    fn handle_key_action(&mut self, key: u32) {
        let action = self.keyboard.xkb.keycode_to_action(key);
        let shift = self.keyboard.xkb.shift();
        let char_count = self.input_text.chars().count();

        match action {
            KeyAction::Char(ch) => {
                self.delete_selection();
                let byte_pos = char_to_byte(&self.input_text, self.cursor);
                // Advance only if the insert landed. A full input rejects it
                // (insert is atomic), and moving the cursor past the real length
                // would desync it: later Backspace and Delete go dead until the
                // phantom offset drains.
                if self.input_text.insert(byte_pos, ch).is_ok() {
                    self.cursor += 1;
                    self.input_changed = true;
                }
                self.selection_anchor = None;
            }
            KeyAction::Backspace => {
                if self.has_selection() {
                    self.delete_selection();
                } else if self.cursor > 0 {
                    self.cursor -= 1;
                    let byte_pos = char_to_byte(&self.input_text, self.cursor);
                    self.input_text.remove(byte_pos);
                }
                self.input_changed = true;
            }
            KeyAction::Delete => {
                if self.has_selection() {
                    self.delete_selection();
                } else if self.cursor < char_count {
                    let byte_pos = char_to_byte(&self.input_text, self.cursor);
                    self.input_text.remove(byte_pos);
                }
                self.input_changed = true;
            }
            KeyAction::Left => {
                if shift {
                    if self.selection_anchor.is_none() {
                        self.selection_anchor = Some(self.cursor);
                    }
                } else {
                    // If there's a selection, jump cursor to the start of it
                    if let Some((start, _)) = self.selection_range() {
                        self.cursor = start;
                        self.selection_anchor = None;
                        self.input_changed = true;
                        return;
                    }
                    self.selection_anchor = None;
                }
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                self.input_changed = true;
            }
            KeyAction::Right => {
                if shift {
                    if self.selection_anchor.is_none() {
                        self.selection_anchor = Some(self.cursor);
                    }
                } else {
                    if let Some((_, end)) = self.selection_range() {
                        self.cursor = end;
                        self.selection_anchor = None;
                        self.input_changed = true;
                        return;
                    }
                    self.selection_anchor = None;
                }
                if self.cursor < char_count {
                    self.cursor += 1;
                }
                self.input_changed = true;
            }
            KeyAction::Home => {
                if shift {
                    if self.selection_anchor.is_none() {
                        self.selection_anchor = Some(self.cursor);
                    }
                } else {
                    self.selection_anchor = None;
                }
                self.cursor = 0;
                self.input_changed = true;
            }
            KeyAction::End => {
                if shift {
                    if self.selection_anchor.is_none() {
                        self.selection_anchor = Some(self.cursor);
                    }
                } else {
                    self.selection_anchor = None;
                }
                self.cursor = char_count;
                self.input_changed = true;
            }
            KeyAction::SelectAll => {
                self.selection_anchor = Some(0);
                self.cursor = char_count;
                self.input_changed = true;
            }
            KeyAction::Copy => {
                if let Some(text) = self.selected_text() {
                    self.clipboard_op = ClipboardOp::Copy(text);
                }
            }
            KeyAction::Cut => {
                if let Some(text) = self.selected_text() {
                    self.clipboard_op = ClipboardOp::Cut(text);
                }
            }
            KeyAction::Paste => {
                self.clipboard_op = ClipboardOp::Paste;
            }
            KeyAction::Enter => {
                self.pending_action = PendingAction::Launch;
                self.running = false;
            }
            KeyAction::Escape => {
                self.input_text.clear();
                self.cursor = 0;
                self.selection_anchor = None;
                self.running = false;
            }
            KeyAction::Up => {
                if shift {
                    // Shift+Up: select all
                    self.selection_anchor = Some(0);
                    self.cursor = char_count;
                    self.input_changed = true;
                } else {
                    // Result-list navigation leaves the text caret in place, so
                    // it signals a selection move rather than input_changed.
                    // Marking input_changed here would reset the caret to solid
                    // every press and stall the blink.
                    self.pending_action = PendingAction::SelectUp;
                }
            }
            KeyAction::Down => {
                if shift {
                    // Shift+Down: deselect
                    self.selection_anchor = None;
                    self.input_changed = true;
                } else {
                    self.pending_action = PendingAction::SelectDown;
                }
            }
            KeyAction::Tab | KeyAction::None => {}
        }
    }

    /// Process key repeat if a key is being held.
    pub fn process_key_repeat(&mut self) {
        if self.key_repeat.should_repeat() {
            if let Some(key) = self.key_repeat.held_key {
                self.key_repeat.repeating = true;
                self.key_repeat.last_repeat = Some(Instant::now());
                self.handle_key_action(key);
            }
        }
    }

    // --- Selection helpers ---

    /// Whether there is an active selection.
    fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    /// Get the selection range as (start, end) char offsets, if any.
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    /// Get the selected text, if any.
    fn selected_text(&self) -> Option<ArrayString<INPUT_CAP>> {
        let (start, end) = self.selection_range()?;
        let byte_start = char_to_byte(&self.input_text, start);
        let byte_end = char_to_byte(&self.input_text, end);
        let mut out: ArrayString<INPUT_CAP> = ArrayString::new();
        out.push_str(&self.input_text.as_str()[byte_start..byte_end])
            .ok()?;
        Some(out)
    }

    /// Delete the selected text and move cursor to selection start.
    fn delete_selection(&mut self) {
        if let Some((start, end)) = self.selection_range() {
            let byte_start = char_to_byte(&self.input_text, start);
            let byte_end = char_to_byte(&self.input_text, end);
            self.input_text.delete_range(byte_start, byte_end);
            self.cursor = start;
            self.selection_anchor = None;
        }
    }

    /// Insert text at the cursor position.
    pub fn insert_at_cursor(&mut self, s: &str) {
        let byte_pos = char_to_byte(&self.input_text, self.cursor);
        // insert_str is atomic: all of s lands or none does. Advance the cursor
        // only when it landed, so a paste that does not fit leaves the cursor on
        // the unchanged text rather than past its end.
        if self.input_text.insert_str(byte_pos, s).is_ok() {
            self.cursor += s.chars().count();
            self.input_changed = true;
        }
        self.selection_anchor = None;
    }

    /// Handle a pointer click in the input box area.
    /// text_x_offset is the pixel x relative to the start of the text.
    pub fn handle_pointer_click(&mut self, char_offset: usize) {
        match self.pointer.click_count {
            2 => {
                // Double-click: select word
                let (start, end) = word_boundaries(&self.input_text, char_offset);
                self.selection_anchor = Some(start);
                self.cursor = end;
            }
            n if n >= 3 => {
                // Triple-click: select all
                self.selection_anchor = Some(0);
                self.cursor = self.input_text.chars().count();
            }
            _ => {
                // Single click: position cursor, clear selection
                let char_count = self.input_text.chars().count();
                self.cursor = char_offset.min(char_count);
                self.selection_anchor = None;
            }
        }
        self.pointer.clicked = false;
        self.input_changed = true;
    }

    /// Handle pointer drag in the input box area.
    pub fn handle_pointer_drag(&mut self, char_offset: usize) {
        let char_count = self.input_text.chars().count();
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }
        self.cursor = char_offset.min(char_count);
        self.pointer.dragging = false;
        self.input_changed = true;
    }

    /// Process a deferred clipboard operation using native Wayland protocol.
    pub fn process_clipboard(&mut self) {
        let op = core::mem::replace(&mut self.clipboard_op, ClipboardOp::None);
        match op {
            ClipboardOp::Copy(text) => {
                if let Err(e) = self.clipboard_set(&text) {
                    crate::elog!("bnklaunch: copy failed: {e}");
                }
            }
            ClipboardOp::Cut(text) => {
                if let Err(e) = self.clipboard_set(&text) {
                    crate::elog!("bnklaunch: cut failed: {e}");
                }
                self.delete_selection();
                self.input_changed = true;
            }
            ClipboardOp::Paste => {
                // If we own the clipboard, use our stored text directly.
                // Receiving our own offer would deadlock: the compositor sends
                // source.SEND back to this connection while we block on the pipe.
                let mut text: ArrayString<INPUT_CAP> = ArrayString::new();
                let got = if let Some(ref t) = self.clipboard.source_text {
                    let _ = text.push_str(t.as_str());
                    true
                } else {
                    match self.clipboard_read() {
                        Ok(t) => {
                            let _ = text.push_str(t.as_str());
                            true
                        }
                        Err(e) => {
                            crate::elog!("bnklaunch: paste failed: {e}");
                            false
                        }
                    }
                };
                if got {
                    self.delete_selection();
                    self.insert_at_cursor(&text);
                }
            }
            ClipboardOp::None => {}
        }
    }

    /// Set the clipboard selection to text owned by this client.
    fn clipboard_set(&mut self, text: &str) -> Result<()> {
        let manager_id = self
            .bindings
            .data_device_manager
            .ok_or_else(|| Error::msg("no data device manager"))?;
        let device_id = self
            .clipboard
            .device_id
            .ok_or_else(|| Error::msg("no data device"))?;

        // Destroy previous source if any
        if let Some(old_source) = self.clipboard.source_id.take() {
            let _ = protocol::data_device::source_destroy(&mut self.socket, old_source);
        }

        // Create new data source
        let source_id = self.ids.allocate();
        protocol::data_device::create_data_source(&mut self.socket, manager_id, source_id)?;

        // Offer text MIME types
        protocol::data_device::source_offer(&mut self.socket, source_id, TextMime::Utf8.as_str())?;
        protocol::data_device::source_offer(&mut self.socket, source_id, TextMime::Plain.as_str())?;

        // Take ownership of the selection with the latest input serial
        protocol::data_device::set_selection(
            &mut self.socket,
            device_id,
            source_id,
            self.last_serial,
        )?;
        self.socket.flush()?;

        // Store source state so we can serve send events
        self.clipboard.source_id = Some(source_id);
        let mut stored: ArrayString<INPUT_CAP> = ArrayString::new();
        let _ = stored.push_str(text);
        self.clipboard.source_text = Some(stored);

        Ok(())
    }

    /// Read the current clipboard selection through a pipe on this connection.
    /// Only valid when another client owns the selection; a self-owned offer is
    /// served from source_text by the caller to avoid the send/read deadlock.
    fn clipboard_read(&mut self) -> Result<ArrayString<{ clipboard::CLIP_CAP }>> {
        let offer_id = self
            .clipboard
            .selection_offer
            .ok_or_else(|| Error::msg("no clipboard selection"))?;
        let mime = self
            .clipboard
            .selection_mime
            .ok_or_else(|| Error::msg("clipboard has no text content"))?;

        let mut fds = [0i32; 2];
        let ret = crate::syscall::pipe2(&mut fds, crate::syscall::O_CLOEXEC);
        if ret < 0 {
            return Err(Error::from_errno(-ret));
        }
        // Own both ends immediately so any early return closes them. Both fds
        // come fresh from pipe2 and are owned by no one else.
        let read_end = crate::syscall::Fd::new(fds[0]);
        let write_end = crate::syscall::Fd::new(fds[1]);

        protocol::data_device::offer_receive(
            &mut self.socket,
            offer_id,
            mime.as_str(),
            write_end.as_raw_fd(),
        )?;
        self.socket.flush()?;

        // Drop our write end; the compositor keeps its own via SCM_RIGHTS, so
        // the pipe reaches EOF once the owner finishes writing.
        drop(write_end);

        clipboard::read_text(read_end)
    }

    // --- Wayland protocol methods ---

    /// Bind to essential global interfaces.
    pub fn bind_globals(&mut self) -> Result<()> {
        let registry_id = self
            .bindings
            .registry
            .ok_or_else(|| Error::msg("registry not initialized"))?;

        for global in self.globals.iter() {
            match global.interface.as_str() {
                interface::WL_COMPOSITOR => {
                    let id = self.ids.allocate();
                    protocol::display::registry_bind(
                        &mut self.socket,
                        registry_id,
                        global.name,
                        &global.interface,
                        global.version.min(4),
                        id,
                    )?;
                    self.bindings.compositor = Some(id);
                }
                interface::WL_SHM => {
                    let id = self.ids.allocate();
                    protocol::display::registry_bind(
                        &mut self.socket,
                        registry_id,
                        global.name,
                        &global.interface,
                        global.version.min(1),
                        id,
                    )?;
                    self.bindings.shm = Some(id);
                }
                interface::WL_SEAT if self.bindings.seat.is_none() => {
                    let id = self.ids.allocate();
                    protocol::display::registry_bind(
                        &mut self.socket,
                        registry_id,
                        global.name,
                        &global.interface,
                        global.version.min(5),
                        id,
                    )?;
                    self.bindings.seat = Some(id);
                }
                interface::ZWLR_LAYER_SHELL_V1 => {
                    let id = self.ids.allocate();
                    protocol::display::registry_bind(
                        &mut self.socket,
                        registry_id,
                        global.name,
                        &global.interface,
                        global.version.min(4),
                        id,
                    )?;
                    self.bindings.layer_shell = Some(id);
                }
                interface::WL_DATA_DEVICE_MANAGER => {
                    let id = self.ids.allocate();
                    protocol::display::registry_bind(
                        &mut self.socket,
                        registry_id,
                        global.name,
                        &global.interface,
                        global.version.min(3),
                        id,
                    )?;
                    self.bindings.data_device_manager = Some(id);
                }
                _ => {}
            }
        }

        // Get a data device for clipboard operations
        if let (Some(manager_id), Some(seat_id)) =
            (self.bindings.data_device_manager, self.bindings.seat)
        {
            let device_id = self.ids.allocate();
            protocol::data_device::get_data_device(
                &mut self.socket,
                manager_id,
                device_id,
                seat_id,
            )?;
            self.clipboard.device_id = Some(device_id);
        }

        self.socket.flush()?;
        Ok(())
    }

    /// Create a layer shell surface (overlay).
    pub fn create_layer_surface(&mut self, width: u32, height: u32) -> Result<()> {
        let compositor_id = self
            .bindings
            .compositor
            .ok_or_else(|| Error::msg("wl_compositor not bound"))?;
        let layer_shell_id = self
            .bindings
            .layer_shell
            .ok_or_else(|| Error::msg("zwlr_layer_shell_v1 not bound"))?;

        let surface_id = self.ids.allocate();
        protocol::compositor::create_surface(&mut self.socket, compositor_id, surface_id)?;

        let layer_surface_id = self.ids.allocate();
        layer_shell::get_layer_surface(
            &mut self.socket,
            layer_shell_id,
            layer_surface_id,
            surface_id,
            0,
            Layer::Overlay,
            "bnklaunch",
        )?;

        layer_shell::set_size(&mut self.socket, layer_surface_id, width, height)?;
        layer_shell::set_anchor(&mut self.socket, layer_surface_id, layer_shell::anchor::TOP)?;
        layer_shell::set_margin(&mut self.socket, layer_surface_id, 350, 0, 0, 0)?;
        layer_shell::set_exclusive_zone(&mut self.socket, layer_surface_id, -1)?;

        // on_demand (layer-shell v4+) yields keyboard focus when the user
        // switches windows, which fires wl_keyboard.leave and dismisses the
        // launcher. Pre-v4 only supports the exclusive grab.
        let layer_shell_version = self
            .globals
            .iter()
            .find(|g| g.interface == interface::ZWLR_LAYER_SHELL_V1)
            .map(|g| g.version)
            .unwrap_or(1);
        let interactivity = if layer_shell_version >= 4 {
            KeyboardInteractivity::OnDemand
        } else {
            KeyboardInteractivity::Exclusive
        };
        layer_shell::set_keyboard_interactivity(&mut self.socket, layer_surface_id, interactivity)?;

        protocol::compositor::surface_commit(&mut self.socket, surface_id)?;

        self.surface = Some(Surface {
            id: surface_id,
            layer_surface_id,
            configured: false,
            width,
            height,
        });

        self.socket.flush()?;
        Ok(())
    }

    /// Create a shared memory buffer for the current surface.
    pub fn create_buffer(&mut self) -> Result<()> {
        let shm_id = self
            .bindings
            .shm
            .ok_or_else(|| Error::msg("wl_shm not bound"))?;

        let (width, height) = {
            let surface = self
                .surface
                .as_ref()
                .ok_or_else(|| Error::msg("no surface"))?;
            (surface.width, surface.height)
        };

        let buffer = PixelBuffer::new(width, height)?;

        let pool_id = self.ids.allocate();
        let pool_size = i32::try_from(buffer.size())
            .map_err(|_| Error::msg("buffer too large for wl_shm pool"))?;
        protocol::shm::create_pool(&mut self.socket, shm_id, pool_id, buffer.fd(), pool_size)?;

        let buffer_id = self.ids.allocate();
        protocol::shm::pool_create_buffer(
            &mut self.socket,
            pool_id,
            buffer_id,
            0,
            width as i32,
            height as i32,
            buffer.stride as i32,
            ShmFormat::Argb8888,
        )?;

        self.buffer = Some(buffer);
        self.pool_id = Some(pool_id);
        self.buffer_id = Some(buffer_id);

        self.socket.flush()?;
        Ok(())
    }

    /// Resize the layer surface to new dimensions.
    pub fn resize_surface(&mut self, new_width: u32, new_height: u32) -> Result<bool> {
        let surface = self
            .surface
            .as_mut()
            .ok_or_else(|| Error::msg("no surface"))?;

        if surface.width == new_width && surface.height == new_height {
            return Ok(false);
        }

        layer_shell::set_size(
            &mut self.socket,
            surface.layer_surface_id,
            new_width,
            new_height,
        )?;
        protocol::compositor::surface_commit(&mut self.socket, surface.id)?;
        self.socket.flush()?;

        surface.configured = false;
        self.socket.set_nonblocking(false)?;
        // Stop if the compositor closes the surface mid-resize (it sets running
        // false) instead of acking, so this never blocks on a configure that
        // will never arrive.
        while self.running && !self.surface.as_ref().map(|s| s.configured).unwrap_or(false) {
            self.dispatch()?;
        }
        self.socket.set_nonblocking(true)?;
        if !self.running {
            return Ok(false);
        }

        // Retire the in-use buffer rather than tearing it down now; the
        // compositor releases it once the new frame is committed, and it is
        // unmapped then (see handle_message).
        if let (Some(buffer), Some(buffer_id), Some(pool_id)) = (
            self.buffer.take(),
            self.buffer_id.take(),
            self.pool_id.take(),
        ) {
            let _ = self.retired_buffers.push(RetiredBuffer {
                buffer_id,
                pool_id,
                _buffer: buffer,
            });
        }
        self.create_buffer()?;

        Ok(true)
    }

    /// Render and commit the current frame.
    pub fn render(&mut self) -> Result<()> {
        let (surface_id, width, height) = {
            let surface = self
                .surface
                .as_ref()
                .ok_or_else(|| Error::msg("no surface"))?;
            (surface.id, surface.width as i32, surface.height as i32)
        };
        let buffer_id = self.buffer_id.ok_or_else(|| Error::msg("no buffer"))?;

        protocol::compositor::surface_attach(&mut self.socket, surface_id, buffer_id, 0, 0)?;
        protocol::compositor::surface_damage(&mut self.socket, surface_id, 0, 0, width, height)?;
        protocol::compositor::surface_commit(&mut self.socket, surface_id)?;
        self.socket.flush()?;
        Ok(())
    }

    /// Get mutable access to the pixel buffer for drawing.
    pub fn pixels(&mut self) -> Option<&mut PixelBuffer> {
        self.buffer.as_mut()
    }
}

/// Find word boundaries around a char offset.
/// Returns (start, end) char offsets for the word.
fn word_boundaries(text: &str, char_offset: usize) -> (usize, usize) {
    let mut chars: ArrayVec<char, INPUT_CAP> = ArrayVec::new();
    for c in text.chars() {
        if chars.push(c).is_err() {
            break;
        }
    }
    let len = chars.len();
    let pos = char_offset.min(len.saturating_sub(1));

    if chars.is_empty() {
        return (0, 0);
    }

    // Scan backward to word start
    let mut start = pos;
    while start > 0 && chars[start - 1].is_alphanumeric() {
        start -= 1;
    }

    // Scan forward to word end
    let mut end = pos;
    while end < len && chars[end].is_alphanumeric() {
        end += 1;
    }

    // If we didn't find a word (clicked on whitespace), select the whitespace
    if start == end {
        while start > 0 && !chars[start - 1].is_alphanumeric() {
            start -= 1;
        }
        while end < len && !chars[end].is_alphanumeric() {
            end += 1;
        }
    }

    (start, end)
}

/// Write all bytes to a file descriptor, handling partial writes.
fn write_all_to_fd(fd: crate::syscall::RawFd, data: &[u8]) {
    let mut offset = 0;
    while offset < data.len() {
        let n = crate::syscall::write_fd(fd, &data[offset..]);
        if n <= 0 {
            break;
        }
        offset += n as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_allocator_reuses_recycled_ids() {
        let mut ids = IdAllocator::new();
        let a = ids.allocate();
        let b = ids.allocate();
        assert_eq!((a, b), (2, 3));

        // A recycled id is handed back before next_id is bumped again.
        ids.recycle(a);
        assert_eq!(ids.allocate(), a);
        assert_eq!(ids.allocate(), 4);
    }
}
