//! High-level Wayland client state machine.

use crate::clipboard::{self, Clipboard};
use crate::platform::arena::{ArrayString, ArrayVec};
use crate::platform::error::{elog, Error, Result};
use crate::platform::syscall;
use crate::platform::time::Instant;

use crate::app::{key_action, KeyAction};
use crate::editor::Editor;
use crate::platform::conn::Connection;
use crate::platform::protocol::{self as proto, KeyboardInteractivity};
use crate::platform::wire::Arg;
use crate::platform::wire::Message;
use crate::platform::xkb::Xkb;
use crate::present::{Present, MAX_SURFACE_DIM};
use crate::shm::PixelBuffer;

/// Most recycled ids held at once.
const MAX_IDS: usize = 256;
/// Most globals the compositor advertises that we track.
const MAX_GLOBALS: usize = 64;
/// Byte capacity of a stored interface name. The reader hands out a borrowed
/// str, so this bounds only what a tracked global keeps, and every name it
/// keeps is one of the five the launcher binds.
const IFACE_CAP: usize = 64;

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
    pub interface: ArrayString<IFACE_CAP>,
    pub version: u32,
}

/// Whether the launcher binds this interface, so the global is worth tracking.
/// A multi-monitor session advertises dozens of wl_output and per-output
/// globals; filtering at advertise time keeps that flood from pushing wl_seat
/// or the layer shell past the bounded globals array and dropping it.
fn is_bindable_interface(iface: &str) -> bool {
    iface == proto::IFACE_COMPOSITOR
        || iface == proto::IFACE_SHM
        || iface == proto::IFACE_SEAT
        || iface == proto::IFACE_LAYER_SHELL
        || iface == proto::IFACE_DATA_DEVICE_MANAGER
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

/// Keyboard state. Owns the Xkb that handles layout-aware translation;
/// the compositor populates the keymap on the first wl_keyboard.keymap event.
pub struct Keyboard {
    pub id: Option<u32>,
    pub focused: bool,
    pub xkb: Xkb,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingAction {
    None,
    SelectUp,
    SelectDown,
    Launch,
}

/// Client state.
pub struct Client {
    pub socket: Connection,
    pub ids: IdAllocator,
    pub globals: ArrayVec<Global, MAX_GLOBALS>,
    pub bindings: Bindings,
    /// The surface and the frames drawn into it.
    pub present: Present,
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
    /// The search input: text, caret, and selection.
    pub editor: Editor,
    /// What the user asked the clipboard to do, serviced by the event loop.
    pub clipboard_op: clipboard::Op,
    /// The clipboard's protocol state.
    pub clipboard: Clipboard,
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
        let socket = Connection::connect()?;
        Ok(Self {
            socket,
            ids: IdAllocator::new(),
            globals: ArrayVec::new(),
            bindings: Bindings::default(),
            present: Present::new(),
            running: true,
            sync_callback: None,
            sync_done: false,
            keyboard: Keyboard {
                id: None,
                focused: false,
                xkb: Xkb::new()?,
            },
            key_repeat: KeyRepeat::default(),
            pointer: Pointer::default(),
            editor: Editor::new(),
            clipboard_op: clipboard::Op::None,
            clipboard: Clipboard::default(),
            last_serial: 0,
            input_changed: false,
            pending_action: PendingAction::None,
        })
    }

    /// Check if layer shell is available.
    pub fn has_layer_shell(&self) -> bool {
        self.globals
            .iter()
            .any(|g| g.interface == proto::IFACE_LAYER_SHELL)
    }

    /// Send a sync request and wait for the callback.
    pub fn roundtrip(&mut self) -> Result<()> {
        let callback_id = self.ids.allocate();
        self.sync_callback = Some(callback_id);
        self.sync_done = false;
        self.socket.request(
            proto::WL_DISPLAY,
            proto::wl_display::SYNC,
            &[Arg::NewId(callback_id)],
        )?;
        self.socket.flush()?;

        // A wl_display.error or a closed layer surface clears running, and the
        // done event will never arrive after either. Waiting on sync_done alone
        // would block here forever.
        while !self.sync_done && self.running {
            self.dispatch()?;
        }
        self.sync_callback = None;
        if !self.sync_done {
            return Err(Error::msg("connection ended before sync completed"));
        }
        Ok(())
    }

    /// Initialize the connection by getting the registry.
    pub fn init(&mut self) -> Result<()> {
        let registry_id = self.ids.allocate();
        self.bindings.registry = Some(registry_id);
        self.socket.request(
            proto::WL_DISPLAY,
            proto::wl_display::GET_REGISTRY,
            &[Arg::NewId(registry_id)],
        )?;
        self.socket.flush()?;
        Ok(())
    }

    /// Handle one message from the compositor, reading more from the socket only
    /// when none is fully buffered. On the non-blocking socket the event loop
    /// runs, a would-block error is how a drained socket says so, and the loop
    /// reads that as the end of the burst.
    pub fn dispatch(&mut self) -> Result<()> {
        loop {
            if let Some(msg) = self.socket.next_message()? {
                return self.handle_message(msg);
            }
            self.socket.fill()?;
        }
    }

    /// Handle a received message.
    fn handle_message(&mut self, msg: Message) -> Result<()> {
        let registry_id = self.bindings.registry;

        // wl_callback.done (for sync)
        if Some(msg.object) == self.sync_callback {
            if msg.opcode == proto::wl_callback::EV_DONE {
                self.sync_done = true;
            }
            return Ok(());
        }

        // wl_display events
        if msg.object == proto::WL_DISPLAY {
            match msg.opcode {
                proto::wl_display::EV_ERROR => {
                    // Log the error (object id, code, message); otherwise a
                    // protocol error is a silent teardown.
                    let mut r = msg.reader();
                    if let (Ok(object), Ok(code), Ok(message)) = (r.u32(), r.u32(), r.string()) {
                        elog!(
                            "bnklaunch: protocol error from object {object} (code {code}): {message}"
                        );
                    }
                    self.running = false;
                }
                proto::wl_display::EV_DELETE_ID => {
                    let mut r = msg.reader();
                    if let Ok(id) = r.u32() {
                        self.ids.recycle(id);
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        // wl_registry events
        if Some(msg.object) == registry_id {
            match msg.opcode {
                proto::wl_registry::EV_GLOBAL => {
                    let mut r = msg.reader();
                    let name = r.u32()?;
                    let interface = r.string()?;
                    let version = r.u32()?;
                    if is_bindable_interface(interface) {
                        // Only the five bindable names get here, and each fits.
                        let mut stored: ArrayString<IFACE_CAP> = ArrayString::new();
                        if stored.push_str(interface).is_ok() {
                            let _ = self.globals.push(Global {
                                name,
                                interface: stored,
                                version,
                            });
                        }
                    }
                }
                proto::wl_registry::EV_GLOBAL_REMOVE => {
                    let mut r = msg.reader();
                    let name = r.u32()?;
                    if let Some(i) = self.globals.iter().position(|g| g.name == name) {
                        self.globals.swap_remove(i);
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        // Layer surface events
        if let Some(ref mut surface) = self.present.surface {
            if msg.object == surface.layer_surface_id {
                match msg.opcode {
                    proto::zwlr_layer_surface_v1::EV_CONFIGURE => {
                        let mut r = msg.reader();
                        let serial = r.u32()?;
                        let width = r.u32()?;
                        let height = r.u32()?;
                        if (1..=MAX_SURFACE_DIM).contains(&width)
                            && (1..=MAX_SURFACE_DIM).contains(&height)
                        {
                            surface.width = width;
                            surface.height = height;
                        }
                        let layer_surface_id = surface.layer_surface_id;
                        self.socket.request(
                            layer_surface_id,
                            proto::zwlr_layer_surface_v1::ACK_CONFIGURE,
                            &[Arg::Uint(serial)],
                        )?;
                        surface.configured = true;
                    }
                    proto::zwlr_layer_surface_v1::EV_CLOSED => {
                        self.running = false;
                    }
                    _ => {}
                }
                return Ok(());
            }
        }

        // A wl_buffer.release says the compositor has finished with a frame: it
        // is free to draw into again, or, if it was retired by a resize, free to
        // unmap.
        if msg.opcode == proto::wl_buffer::EV_RELEASE
            && self.present.release(&mut self.socket, msg.object)?
        {
            return Ok(());
        }

        // wl_seat events
        if Some(msg.object) == self.bindings.seat {
            match msg.opcode {
                proto::wl_seat::EV_CAPABILITIES => {
                    let mut r = msg.reader();
                    let caps = r.u32()?;
                    // Request keyboard if available
                    if (caps & proto::wl_seat::CAP_KEYBOARD) != 0 && self.keyboard.id.is_none() {
                        let keyboard_id = self.ids.allocate();
                        self.socket.request(
                            msg.object,
                            proto::wl_seat::GET_KEYBOARD,
                            &[Arg::NewId(keyboard_id)],
                        )?;
                        self.keyboard.id = Some(keyboard_id);
                    }
                    // Request pointer if available
                    if (caps & proto::wl_seat::CAP_POINTER) != 0 && self.pointer.id.is_none() {
                        let pointer_id = self.ids.allocate();
                        self.socket.request(
                            msg.object,
                            proto::wl_seat::GET_POINTER,
                            &[Arg::NewId(pointer_id)],
                        )?;
                        self.pointer.id = Some(pointer_id);
                    }
                    self.socket.flush()?;
                }
                proto::wl_seat::EV_NAME => {}
                _ => {}
            }
            return Ok(());
        }

        // wl_keyboard events
        if Some(msg.object) == self.keyboard.id {
            match msg.opcode {
                proto::wl_keyboard::EV_KEYMAP => {
                    let mut r = msg.reader();
                    let format = r.u32()?;
                    let size = r.u32()?;
                    match self.socket.take_fd() {
                        Some(fd) => {
                            // The keymap is mapped rather than read: the fd
                            // shares its seek position with the compositor's own
                            // file-table entry, and not every compositor rewinds
                            // it. The mapping and the fd both go away at the end
                            // of this arm, once xkb has compiled the bytes.
                            let loaded = syscall::read_mapped(fd.as_raw_fd(), size as usize)
                                .and_then(|map| {
                                    self.keyboard.xkb.load_keymap(map.as_slice(), format)
                                });
                            if let Err(e) = loaded {
                                elog!("bnklaunch: failed to load keymap: {e}");
                            }
                        }
                        // Without a keymap no key types anything, so the keyboard
                        // is dead. Say so rather than look hung.
                        None => elog!("bnklaunch: keymap event carried no file descriptor"),
                    }
                }
                proto::wl_keyboard::EV_ENTER => {
                    let mut r = msg.reader();
                    self.last_serial = r.u32()?;
                    self.keyboard.focused = true;
                }
                proto::wl_keyboard::EV_LEAVE => {
                    self.keyboard.focused = false;
                    self.running = false;
                }
                proto::wl_keyboard::EV_KEY => {
                    let mut r = msg.reader();
                    self.last_serial = r.u32()?;
                    let _time = r.u32()?;
                    let key = r.u32()?;
                    let state = r.u32()?;

                    if state == proto::wl_keyboard::KEY_PRESSED {
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
                proto::wl_keyboard::EV_MODIFIERS => {
                    let mut r = msg.reader();
                    let _serial = r.u32()?;
                    let mods_depressed = r.u32()?;
                    let mods_latched = r.u32()?;
                    let mods_locked = r.u32()?;
                    let group = r.u32()?;
                    self.keyboard.xkb.update_modifiers(
                        mods_depressed,
                        mods_latched,
                        mods_locked,
                        group,
                    );
                }
                proto::wl_keyboard::EV_REPEAT_INFO => {
                    let mut r = msg.reader();
                    let rate = r.i32()?;
                    let delay = r.i32()?;
                    // A rate of zero is the compositor turning key repeat off,
                    // so it has to be stored (should_repeat reads it as
                    // disabled). Only a negative rate or delay, which the
                    // protocol does not define, keeps the current setting.
                    if rate >= 0 {
                        self.key_repeat.rate = rate as u32;
                    }
                    if delay >= 0 {
                        self.key_repeat.delay_ms = delay as u32;
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        // wl_pointer events
        if Some(msg.object) == self.pointer.id {
            match msg.opcode {
                proto::wl_pointer::EV_ENTER => {
                    let mut r = msg.reader();
                    self.last_serial = r.u32()?;
                    let _surface = r.u32()?;
                    self.pointer.x = r.fixed()?;
                    self.pointer.y = r.fixed()?;
                }
                proto::wl_pointer::EV_LEAVE => {}
                proto::wl_pointer::EV_MOTION => {
                    let mut r = msg.reader();
                    let _time = r.u32()?;
                    self.pointer.x = r.fixed()?;
                    self.pointer.y = r.fixed()?;
                    if self.pointer.button_pressed {
                        self.pointer.dragging = true;
                        self.input_changed = true;
                    }
                }
                proto::wl_pointer::EV_BUTTON => {
                    let mut r = msg.reader();
                    self.last_serial = r.u32()?;
                    let _time = r.u32()?;
                    let button = r.u32()?;
                    let state = r.u32()?;

                    if button == proto::BTN_LEFT {
                        if state == proto::wl_pointer::BUTTON_PRESSED {
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

        // The clipboard's own events: the offers the compositor describes, the
        // selection it names, and the requests to serve what we copied.
        if self
            .clipboard
            .handle(&mut self.socket, msg.object, msg.opcode, &msg.body)?
        {
            return Ok(());
        }

        Ok(())
    }

    /// Turn a key press into editor calls plus whatever the launcher has to do
    /// about it: a clipboard transfer, a move in the result list, a launch.
    fn handle_key_action(&mut self, key: u32) {
        let action = key_action(&self.keyboard.xkb, key);
        let shift = self.keyboard.xkb.shift_active();

        match action {
            KeyAction::Char(ch) => {
                if self.editor.insert(ch) {
                    self.input_changed = true;
                }
            }
            KeyAction::Backspace => {
                self.editor.backspace();
                self.input_changed = true;
            }
            KeyAction::Delete => {
                self.editor.delete();
                self.input_changed = true;
            }
            KeyAction::Left => {
                self.editor.left(shift);
                self.input_changed = true;
            }
            KeyAction::Right => {
                self.editor.right(shift);
                self.input_changed = true;
            }
            KeyAction::Home => {
                self.editor.home(shift);
                self.input_changed = true;
            }
            KeyAction::End => {
                self.editor.end(shift);
                self.input_changed = true;
            }
            KeyAction::SelectAll => {
                self.editor.select_all();
                self.input_changed = true;
            }
            KeyAction::Copy => {
                if let Some(text) = self.editor.selected_text() {
                    self.clipboard_op = clipboard::Op::Copy(text);
                }
            }
            KeyAction::Cut => {
                if let Some(text) = self.editor.selected_text() {
                    self.clipboard_op = clipboard::Op::Cut(text);
                }
            }
            KeyAction::Paste => {
                self.clipboard_op = clipboard::Op::Paste;
            }
            KeyAction::Enter => {
                self.pending_action = PendingAction::Launch;
                self.running = false;
            }
            KeyAction::Escape => {
                self.editor.clear();
                self.running = false;
            }
            KeyAction::Up => {
                if shift {
                    self.editor.select_all();
                    self.input_changed = true;
                } else {
                    // Result-list navigation leaves the text caret where it is,
                    // so it moves the selection rather than changing the input.
                    self.pending_action = PendingAction::SelectUp;
                }
            }
            KeyAction::Down => {
                if shift {
                    self.editor.select_all();
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

    /// A pointer click inside the input box, at a char offset in the text.
    pub fn handle_pointer_click(&mut self, char_offset: usize) {
        self.editor.click(char_offset, self.pointer.click_count);
        self.pointer.clicked = false;
        self.input_changed = true;
    }

    /// A pointer drag inside the input box, to a char offset in the text.
    pub fn handle_pointer_drag(&mut self, char_offset: usize) {
        self.editor.drag(char_offset);
        self.pointer.dragging = false;
        self.input_changed = true;
    }

    /// Service the clipboard operation the last key press asked for.
    pub fn process_clipboard(&mut self) {
        let op = core::mem::take(&mut self.clipboard_op);
        let manager_id = self.bindings.data_device_manager;
        let serial = self.last_serial;
        let ids = &mut self.ids;

        match op {
            clipboard::Op::Copy(text) => {
                if let Err(e) = self.clipboard.set(
                    &mut self.socket,
                    manager_id,
                    &mut || ids.allocate(),
                    serial,
                    &text,
                ) {
                    elog!("bnklaunch: copy failed: {e}");
                }
            }
            clipboard::Op::Cut(text) => {
                if let Err(e) = self.clipboard.set(
                    &mut self.socket,
                    manager_id,
                    &mut || ids.allocate(),
                    serial,
                    &text,
                ) {
                    elog!("bnklaunch: cut failed: {e}");
                }
                self.editor.delete_selection();
                self.input_changed = true;
            }
            clipboard::Op::Paste => match self.clipboard.read(&mut self.socket) {
                Ok(text) => {
                    if !self.editor.paste(&text) {
                        elog!("bnklaunch: the pasted text does not fit the input");
                    }
                    self.input_changed = true;
                }
                Err(e) => elog!("bnklaunch: paste failed: {e}"),
            },
            clipboard::Op::None => {}
        }
    }

    // --- Wayland protocol methods ---

    /// Bind to essential global interfaces.
    pub fn bind_globals(&mut self) -> Result<()> {
        let registry_id = self
            .bindings
            .registry
            .ok_or_else(|| Error::msg("registry not initialized"))?;

        // The interface a global advertises decides which binding it fills and
        // which version to ask for. A compositor may advertise the same
        // interface more than once (a seat per input device, say); the first
        // wins, since every binding here is single-valued.
        for i in 0..self.globals.len() {
            let (name, version) = (self.globals[i].name, self.globals[i].version);
            let iface = self.globals[i].interface;

            let (slot, want): (&mut Option<u32>, u32) = match iface.as_str() {
                proto::IFACE_COMPOSITOR => {
                    (&mut self.bindings.compositor, proto::VERSION_COMPOSITOR)
                }
                proto::IFACE_SHM => (&mut self.bindings.shm, proto::VERSION_SHM),
                proto::IFACE_SEAT => (&mut self.bindings.seat, proto::VERSION_SEAT),
                proto::IFACE_LAYER_SHELL => {
                    (&mut self.bindings.layer_shell, proto::VERSION_LAYER_SHELL)
                }
                proto::IFACE_DATA_DEVICE_MANAGER => (
                    &mut self.bindings.data_device_manager,
                    proto::VERSION_DATA_DEVICE_MANAGER,
                ),
                _ => continue,
            };
            if slot.is_some() {
                continue;
            }

            let id = self.ids.allocate();
            *slot = Some(id);
            self.socket.request(
                registry_id,
                proto::wl_registry::BIND,
                &[
                    Arg::Uint(name),
                    Arg::Bind {
                        interface: iface.as_str(),
                        // Never ask for more than the compositor offers.
                        version: version.min(want),
                        new_id: id,
                    },
                ],
            )?;
        }

        // Get a data device for clipboard operations
        if let (Some(manager_id), Some(seat_id)) =
            (self.bindings.data_device_manager, self.bindings.seat)
        {
            let device_id = self.ids.allocate();
            self.socket.request(
                manager_id,
                proto::wl_data_device_manager::GET_DATA_DEVICE,
                &[Arg::NewId(device_id), Arg::Object(seat_id)],
            )?;
            self.clipboard.device_id = Some(device_id);
        }

        self.socket.flush()?;
        Ok(())
    }

    /// Create the layer surface the launcher paints on.
    pub fn create_layer_surface(&mut self, width: u32, height: u32) -> Result<()> {
        let compositor_id = self
            .bindings
            .compositor
            .ok_or_else(|| Error::msg("wl_compositor not bound"))?;
        let layer_shell_id = self
            .bindings
            .layer_shell
            .ok_or_else(|| Error::msg("zwlr_layer_shell_v1 not bound"))?;
        // OnDemand needs layer-shell v4. It yields keyboard focus when the user
        // switches windows, which fires wl_keyboard.leave and dismisses the
        // launcher; older versions only offer the exclusive grab, which does not.
        let version = self
            .globals
            .iter()
            .find(|g| g.interface == proto::IFACE_LAYER_SHELL)
            .map(|g| g.version)
            .unwrap_or(1);
        let interactivity = if version >= 4 {
            KeyboardInteractivity::OnDemand
        } else {
            KeyboardInteractivity::Exclusive
        };

        let ids = &mut self.ids;
        self.present.create_surface(
            &mut self.socket,
            &mut || ids.allocate(),
            compositor_id,
            layer_shell_id,
            interactivity,
            width,
            height,
        )
    }

    /// Allocate the frames for the current surface size.
    pub fn create_buffer(&mut self) -> Result<()> {
        let shm_id = self
            .bindings
            .shm
            .ok_or_else(|| Error::msg("wl_shm not bound"))?;
        let ids = &mut self.ids;
        self.present
            .create_frames(&mut self.socket, &mut || ids.allocate(), shm_id)
    }

    /// Resize the layer surface. False when it was already that size, or when
    /// the compositor closed it instead of acking.
    pub fn resize_surface(&mut self, new_width: u32, new_height: u32) -> Result<bool> {
        let surface = self
            .present
            .surface
            .as_mut()
            .ok_or_else(|| Error::msg("no surface"))?;

        if surface.width == new_width && surface.height == new_height {
            return Ok(false);
        }

        let (layer_surface_id, surface_id) = (surface.layer_surface_id, surface.id);
        surface.configured = false;
        self.socket.request(
            layer_surface_id,
            proto::zwlr_layer_surface_v1::SET_SIZE,
            &[Arg::Uint(new_width), Arg::Uint(new_height)],
        )?;
        self.socket
            .request(surface_id, proto::wl_surface::COMMIT, &[])?;
        self.socket.flush()?;

        // The configure that answers this carries the size to draw at, so wait
        // for it. Stop if the compositor closes the surface instead (which
        // clears running), rather than block on a configure that never comes.
        self.socket.set_nonblocking(false)?;
        while self.running && !self.surface_configured() {
            self.dispatch()?;
        }
        self.socket.set_nonblocking(true)?;
        if !self.running {
            return Ok(false);
        }

        let shm_id = self
            .bindings
            .shm
            .ok_or_else(|| Error::msg("wl_shm not bound"))?;
        let ids = &mut self.ids;
        self.present
            .resize_frames(&mut self.socket, &mut || ids.allocate(), shm_id)?;
        Ok(true)
    }

    pub fn surface_configured(&self) -> bool {
        self.present
            .surface
            .as_ref()
            .map(|s| s.configured)
            .unwrap_or(false)
    }

    /// Draw the next frame and show it.
    ///
    /// The compositor keeps reading a frame after the commit that showed it, so
    /// the renderer draws into the other one. If it is still holding both, wait
    /// for it to let one go rather than paint over what is on screen.
    pub fn render(&mut self, draw: impl FnOnce(&mut Client)) -> Result<()> {
        while self.running && !self.present.ready() {
            self.socket.set_nonblocking(false)?;
            let waited = self.dispatch();
            self.socket.set_nonblocking(true)?;
            waited?;
        }
        if !self.running {
            return Ok(());
        }
        draw(self);
        self.present.commit(&mut self.socket)
    }

    /// The pixels of the frame being drawn into.
    pub fn pixels(&mut self) -> Option<&mut PixelBuffer> {
        self.present.pixels()
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
