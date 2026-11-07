//! High-level Wayland client state machine.

use crate::clipboard::{self, Clipboard, Paste, Received};
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
/// Longest a frame holds for the configure that answers a resize.
const CONFIGURE_GRACE_MS: u64 = 100;
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
    /// Milliseconds until the held key is next due to repeat, zero when it is
    /// due now. None when no key is held, or when the rate says never. The
    /// event loop parks for this long, and should_repeat reads the same answer.
    pub fn time_until_due(&self) -> Option<u32> {
        let press_time = self.press_time?;
        let now = Instant::now();
        self.due_in(
            now.ms_since(press_time) as u32,
            self.last_repeat.map(|last| now.ms_since(last) as u32),
        )
    }

    /// The same answer from elapsed times, so a test can walk exact moments.
    pub fn due_in(&self, since_press: u32, since_last: Option<u32>) -> Option<u32> {
        self.held_key?;
        if !self.repeating {
            return Some(self.delay_ms.saturating_sub(since_press));
        }
        let Some(since_last) = since_last else {
            return Some(0);
        };
        if self.rate == 0 {
            return None;
        }
        // rate is repeats per second, so the gap between repeats is
        // 1000 / rate milliseconds.
        let interval_ms = 1000 / self.rate;
        Some(interval_ms.saturating_sub(since_last))
    }

    /// Check if it's time to emit a repeat.
    pub fn should_repeat(&self) -> bool {
        self.time_until_due() == Some(0)
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
    /// The frame callback of the last frame shown, until the compositor says
    /// it is time for the next one.
    frame_callback: Option<u32>,
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
    /// A paste waiting on the clipboard's owner. paste_serial counts every paste
    /// started, so the event loop can tell this one's poll from an earlier one's.
    pub paste: Option<Paste>,
    pub paste_serial: u64,
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
        Self::over(Connection::connect()?)
    }

    /// A client on an open connection that has sent nothing yet.
    pub fn over(socket: Connection) -> Result<Self> {
        Ok(Self {
            socket,
            ids: IdAllocator::new(),
            globals: ArrayVec::new(),
            bindings: Bindings::default(),
            present: Present::new(),
            running: true,
            sync_callback: None,
            sync_done: false,
            frame_callback: None,
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
            paste: None,
            paste_serial: 0,
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

        // wl_callback.done for the last frame: the compositor is ready for the
        // next one.
        if Some(msg.object) == self.frame_callback {
            if msg.opcode == proto::wl_callback::EV_DONE {
                self.frame_callback = None;
            }
            return Ok(());
        }

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
                        self.clipboard.release_id(id);
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
                        surface.awaiting_configure = None;
                    }
                    proto::zwlr_layer_surface_v1::EV_CLOSED => {
                        self.running = false;
                    }
                    _ => {}
                }
                return Ok(());
            }
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
                    } else if self.key_repeat.held_key == Some(key) {
                        self.key_repeat.held_key = None;
                        self.key_repeat.press_time = None;
                        self.key_repeat.repeating = false;
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

        // A wl_buffer.release says the compositor has finished with a frame: it
        // is free to draw into again, or, if it was retired by a resize, free to
        // unmap.
        //
        // This is the one test that starts from an opcode rather than an object,
        // and opcode 0 is crowded: wl_seat.capabilities, wl_pointer.enter,
        // wl_registry.global and wl_data_offer.offer all share it. So it runs
        // last, once every message belonging to a known object has been claimed
        // by that object, and only ever sees what is left over.
        if msg.opcode == proto::wl_buffer::EV_RELEASE {
            self.present.release(&mut self.socket, msg.object)?;
        }

        Ok(())
    }

    /// Turn a key press into editor calls plus whatever the launcher has to do
    /// about it: a clipboard transfer, a move in the result list, a launch.
    ///
    /// Each arm answers one question, whether the input now looks different, and
    /// the caller records it. The clipboard and the launch answer no: a copy
    /// changes nothing on screen, and a paste redraws once the event loop has
    /// actually fetched the text.
    fn handle_key_action(&mut self, key: u32) {
        let action = key_action(&self.keyboard.xkb, key);
        let shift = self.keyboard.xkb.shift_active();

        let changed = match action {
            KeyAction::Char(ch) => self.editor.insert(ch),
            KeyAction::Backspace => {
                self.editor.backspace();
                true
            }
            KeyAction::Delete => {
                self.editor.delete();
                true
            }
            KeyAction::Left => {
                self.editor.left(shift);
                true
            }
            KeyAction::Right => {
                self.editor.right(shift);
                true
            }
            KeyAction::Home => {
                self.editor.home(shift);
                true
            }
            KeyAction::End => {
                self.editor.end(shift);
                true
            }
            KeyAction::SelectAll => {
                self.editor.select_all();
                true
            }
            KeyAction::Copy => {
                if let Some(text) = self.editor.selected_text() {
                    self.clipboard_op = clipboard::Op::Copy(text);
                }
                false
            }
            KeyAction::Cut => {
                if let Some(text) = self.editor.selected_text() {
                    self.clipboard_op = clipboard::Op::Cut(text);
                }
                false
            }
            KeyAction::Paste => {
                self.clipboard_op = clipboard::Op::Paste;
                false
            }
            KeyAction::Enter => {
                self.pending_action = PendingAction::Launch;
                self.running = false;
                false
            }
            KeyAction::Escape => {
                self.dismiss();
                false
            }
            // Shift turns the arrows into a select-all; without it they move
            // through the result list, which leaves the text caret alone.
            KeyAction::Up | KeyAction::Down => {
                if shift {
                    self.editor.select_all();
                    true
                } else {
                    self.pending_action = if action == KeyAction::Up {
                        PendingAction::SelectUp
                    } else {
                        PendingAction::SelectDown
                    };
                    false
                }
            }
            KeyAction::Tab | KeyAction::None => false,
        };
        self.input_changed |= changed;
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

    /// Close without launching, as Escape and a second press of the hotkey do.
    /// That holds after Enter too, while the launch waits for a paste.
    pub fn dismiss(&mut self) {
        self.editor.clear();
        self.pending_action = PendingAction::None;
        self.running = false;
    }

    /// Take what the pending paste's pipe holds, and finish the paste once the
    /// owner is done or its deadline has passed.
    pub fn advance_paste(&mut self) {
        let Some(paste) = self.paste.as_mut() else {
            return;
        };
        match paste.advance() {
            Ok(false) => return,
            Ok(true) => {
                let text = paste.text();
                self.insert_paste(text.as_str());
            }
            Err(e) => elog!("bnklaunch: paste failed: {e}"),
        }
        self.paste = None;
    }

    fn insert_paste(&mut self, text: &str) {
        if !self.editor.paste(text) {
            elog!("bnklaunch: the pasted text does not fit the input");
        }
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
            // One paste at a time: another while the first is still arriving
            // is dropped.
            clipboard::Op::Paste if self.paste.is_some() => {}
            clipboard::Op::Paste => match self.clipboard.receive(&mut self.socket) {
                Ok(Received::Text(text)) => self.insert_paste(text.as_str()),
                Ok(Received::Pending(paste)) => {
                    self.paste = Some(paste);
                    self.paste_serial += 1;
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

    /// Ask for a new window size. The compositor answers with a configure in
    /// its own time; frames keep the size they have until then, and the next
    /// one drawn after it is allocated at the configured size.
    pub fn request_size(&mut self, width: u32, height: u32) -> Result<()> {
        let surface = self
            .present
            .surface
            .as_mut()
            .ok_or_else(|| Error::msg("no surface"))?;
        if surface.requested == (width, height) {
            return Ok(());
        }
        surface.requested = (width, height);
        surface.awaiting_configure = Some(Instant::now());
        let (layer_surface_id, surface_id) = (surface.layer_surface_id, surface.id);
        self.socket.request(
            layer_surface_id,
            proto::zwlr_layer_surface_v1::SET_SIZE,
            &[Arg::Uint(width), Arg::Uint(height)],
        )?;
        self.socket
            .request(surface_id, proto::wl_surface::COMMIT, &[])?;
        self.socket.flush()
    }

    pub fn surface_configured(&self) -> bool {
        self.present
            .surface
            .as_ref()
            .map(|s| s.configured)
            .unwrap_or(false)
    }

    /// While a resize waits for its configure, how much longer a frame holds
    /// for it. A compositor answers a set_size at once, so the frame that goes
    /// out is drawn at the new size; one that takes longer than the grace gets
    /// the frame at the old size, so typing still shows. None once the
    /// configure has come or the grace has run out.
    pub fn configure_wait_ms(&self) -> Option<u64> {
        let since = self.present.surface.as_ref()?.awaiting_configure?;
        CONFIGURE_GRACE_MS
            .checked_sub(since.elapsed_ms())
            .filter(|&ms| ms > 0)
    }

    /// Draw the next frame and show it, if one can go now. It waits while the
    /// compositor has not asked for the next frame yet, or holds both buffers;
    /// the caller keeps the window marked for a repaint and tries again on a
    /// later turn, once the frame callback or a release has come in. Returns
    /// whether a frame went out.
    pub fn paint(&mut self, draw: impl FnOnce(&mut Client)) -> Result<bool> {
        if !self.running || self.frame_callback.is_some() || self.configure_wait_ms().is_some() {
            return Ok(false);
        }
        let shm_id = self
            .bindings
            .shm
            .ok_or_else(|| Error::msg("wl_shm not bound"))?;
        let ids = &mut self.ids;
        let ready = self
            .present
            .prepare_frame(&mut self.socket, &mut || ids.allocate(), shm_id)?;
        if !ready {
            return Ok(false);
        }
        draw(self);
        let callback = self.ids.allocate();
        self.present.commit(&mut self.socket, callback)?;
        self.frame_callback = Some(callback);
        Ok(true)
    }

    /// The pixels of the frame being drawn into.
    pub fn pixels(&mut self) -> Option<&mut PixelBuffer> {
        self.present.pixels()
    }
}

#[cfg(test)]
mod tests {
    use crate::client::*;
    use crate::platform::syscall::Fd;
    use crate::platform::wire::{self, MSG_BODY_CAP};

    /// A client talking to one end of a socketpair, with the other end handed
    /// back so the test can play compositor.
    fn test_client() -> (Client, Fd) {
        let (socket, peer) = Connection::pair();
        (Client::over(socket).expect("client"), peer)
    }

    /// An event for an object, carrying the given u32 arguments.
    fn event(object: u32, opcode: u16, args: &[u32]) -> Message {
        let mut body: ArrayVec<u8, MSG_BODY_CAP> = ArrayVec::new();
        for a in args {
            body.extend_from_slice(&a.to_ne_bytes()).expect("argument");
        }
        Message {
            object,
            opcode,
            body,
        }
    }

    /// Every request the client has flushed to the peer, as object, opcode and
    /// body.
    fn sent_requests(peer: &Fd) -> Vec<(u32, u16, Vec<u8>)> {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let mut pfd = syscall::pollfd {
                fd: peer.as_raw_fd(),
                events: syscall::POLLIN,
                revents: 0,
            };
            if syscall::poll(core::slice::from_mut(&mut pfd), 0) <= 0 {
                break;
            }
            let n = syscall::read_fd(peer.as_raw_fd(), &mut chunk);
            if n <= 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..n as usize]);
        }
        let mut out = Vec::new();
        let mut rest = &bytes[..];
        while !rest.is_empty() {
            let (object, opcode, size) = wire::parse_header(rest).expect("header");
            out.push((object, opcode, rest[8..size].to_vec()));
            rest = &rest[size..];
        }
        out
    }

    /// A client holding a data device, ready to copy.
    fn clipboard_client() -> (Client, Fd) {
        let (mut client, peer) = test_client();
        client.bindings.data_device_manager = Some(client.ids.allocate());
        client.clipboard.device_id = Some(client.ids.allocate());
        (client, peer)
    }

    /// Copy text the way Ctrl+C does, and return the id of the data source the
    /// copy created.
    fn copy(client: &mut Client, peer: &Fd, text: &str) -> u32 {
        let mut stored = ArrayString::new();
        stored.push_str(text).expect("text fits");
        client.clipboard_op = clipboard::Op::Copy(stored);
        client.process_clipboard();
        let manager = client.bindings.data_device_manager.expect("manager");
        sent_requests(peer)
            .iter()
            .find(|(object, opcode, _)| {
                *object == manager && *opcode == proto::wl_data_device_manager::CREATE_DATA_SOURCE
            })
            .map(|(_, _, body)| wire::Reader::new(body).u32().expect("new id"))
            .expect("the copy created a data source")
    }

    /// A pipe for a send event: the read end stays with the test, the write end
    /// goes to the client.
    fn pipe() -> (Fd, Fd) {
        let mut fds = [0i32; 2];
        assert_eq!(syscall::pipe2(&mut fds, syscall::O_CLOEXEC), 0);
        (Fd::new(fds[0]), Fd::new(fds[1]))
    }

    /// Everything written into a pipe whose writers have all closed.
    fn drained(read_end: Fd) -> String {
        let mut paste = Paste::new(read_end).expect("paste");
        assert!(paste.advance().expect("read"), "a writer is still open");
        paste.text().as_str().to_string()
    }

    /// Have the peer ask a data source for its text, passing the write end of a
    /// pipe the way the compositor does.
    fn send_to_source(peer: &Fd, source: u32, write_end: Fd) {
        let mut msg: ArrayVec<u8, 64> = ArrayVec::new();
        wire::encode(
            &mut msg,
            source,
            proto::wl_data_source::EV_SEND,
            &[Arg::Str("text/plain;charset=utf-8")],
        )
        .expect("encode");
        let sent = syscall::send_with_fds(peer.as_raw_fd(), &msg, &[write_end.as_raw_fd()]);
        assert_eq!(sent as usize, msg.len());
    }

    /// A clipboard manager reads every new selection, so a held Ctrl+C has the
    /// compositor send to a source the client already replaced. Each send
    /// carries a pipe, and the fds queue in arrival order, so the stale send's
    /// fd has to be taken off the queue even though nothing is written to it.
    #[test]
    fn a_send_to_a_replaced_source_takes_its_own_fd() {
        let (mut client, peer) = clipboard_client();
        let first = copy(&mut client, &peer, "first");
        let second = copy(&mut client, &peer, "second");

        let (stale_read, stale_write) = pipe();
        let (live_read, live_write) = pipe();
        send_to_source(&peer, first, stale_write);
        send_to_source(&peer, second, live_write);
        client.dispatch().expect("the stale send");
        client.dispatch().expect("the live send");

        // The stale pipe closes with nothing in it. Read first, because if its
        // fd stayed queued, the live send took it and wrote here instead.
        assert_eq!(drained(stale_read), "");
        assert_eq!(drained(live_read), "second");
    }

    /// The compositor cancels a source when another client takes the
    /// clipboard. The client destroys it, and once the compositor confirms the
    /// id is gone, the clipboard stops claiming messages for it: that id is
    /// handed out again, and its next owner's events must reach it.
    #[test]
    fn a_cancelled_source_is_destroyed_and_released_with_its_id() {
        let (mut client, peer) = clipboard_client();
        let source = copy(&mut client, &peer, "text");

        client
            .handle_message(event(source, proto::wl_data_source::EV_CANCELLED, &[]))
            .expect("cancelled");
        client.socket.flush().expect("flush");
        assert!(
            sent_requests(&peer)
                .iter()
                .any(|(object, opcode, _)| *object == source
                    && *opcode == proto::wl_data_source::DESTROY),
            "the cancelled source was destroyed"
        );

        client
            .handle_message(event(
                proto::WL_DISPLAY,
                proto::wl_display::EV_DELETE_ID,
                &[source],
            ))
            .expect("delete_id");
        let claimed = client
            .clipboard
            .handle(
                &mut client.socket,
                source,
                proto::wl_buffer::EV_RELEASE,
                &[],
            )
            .expect("handle");
        assert!(!claimed, "a deleted id belongs to whoever gets it next");
    }

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

    /// Opcode 0 means five different things: wl_buffer.release,
    /// wl_seat.capabilities, wl_pointer.enter, wl_registry.global and
    /// wl_data_offer.offer. Only the object id tells them apart, so every event
    /// here carries the same opcode and differs only in who it is addressed to.
    #[test]
    fn opcode_zero_is_routed_by_object_and_never_by_opcode() {
        let (mut client, _peer) = test_client();

        // A surface with two frames, both committed, so the compositor holds
        // each of them and neither is free to draw into.
        client.bindings.compositor = Some(client.ids.allocate());
        client.bindings.layer_shell = Some(client.ids.allocate());
        client.bindings.shm = Some(client.ids.allocate());
        client.create_layer_surface(64, 64).expect("layer surface");
        client.create_buffer().expect("frames");
        for frame in ["first frame", "second frame"] {
            let callback = client.ids.allocate();
            client
                .present
                .commit(&mut client.socket, callback)
                .expect(frame);
        }
        assert!(!client.present.ready(), "the compositor holds both frames");

        let seat = client.ids.allocate();
        client.bindings.seat = Some(seat);

        // The seat's capabilities. Asking for the input devices is what proves
        // this reached the seat rather than the buffer-release check.
        let caps = proto::wl_seat::CAP_KEYBOARD | proto::wl_seat::CAP_POINTER;
        client
            .handle_message(event(seat, proto::wl_seat::EV_CAPABILITIES, &[caps]))
            .expect("capabilities");
        assert!(
            client.keyboard.id.is_some(),
            "the seat asked for a keyboard"
        );
        assert!(client.pointer.id.is_some(), "the seat asked for a pointer");

        // A buffer release naming a frame, which frees it to draw into again.
        let frame = client.present.current_buffer_id().expect("a frame");
        client
            .handle_message(event(frame, proto::wl_buffer::EV_RELEASE, &[]))
            .expect("release");
        assert!(client.present.ready(), "the released frame came free");

        // The pointer entering the surface. Its arguments are a serial and a
        // position, and reading them back is what proves it landed here.
        let pointer = client.pointer.id.expect("pointer");
        client
            .handle_message(event(
                pointer,
                proto::wl_pointer::EV_ENTER,
                &[9, 0, 256, 512],
            ))
            .expect("enter");
        assert_eq!(client.last_serial, 9);
        assert_eq!((client.pointer.x, client.pointer.y), (1.0, 2.0));
    }
}
