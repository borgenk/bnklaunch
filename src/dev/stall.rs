//! Ways the launcher's UI can stall, provoked one at a time.
//!
//! A fake compositor holds the far end of a socketpair, and the real launcher
//! runs against it on a thread of its own: the bring-up, the io_uring event
//! loop, the clipboard and the frames, unchanged. Each scenario withholds or
//! floods one thing the launcher depends on, then sends input that has nothing
//! to do with it and times how long the launcher takes to act on it.
//!
//! The scenarios measure and print; they do not judge. stall_report runs them
//! all, and it needs a kernel that allows io_uring.

use std::collections::{HashMap, VecDeque};
use std::os::fd::IntoRawFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::app::{self, AppState};
use crate::client::Client;
use crate::desktop::{Catalog, DesktopEntry};
use crate::font::Font;
use crate::platform::arena::ArrayVec;
use crate::platform::conn::Connection;
use crate::platform::protocol as proto;
use crate::platform::syscall::{self, Fd, RawFd};
use crate::platform::wire::{self, Arg};
use crate::platform::xkb::keycode::{KEY_A, KEY_C, KEY_ENTER, KEY_RIGHT, KEY_V};
use crate::ui::Theme;

const FIXTURE_FONT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test-font.ttf");

/// evdev code of the B key; xkb::keycode names only the keys the app matches.
const KEY_B: u32 = 48;

/// The two letters the scenarios type, at their xkb codes (evdev plus 8).
const KEYMAP: &str = r#"xkb_keymap {
    xkb_keycodes "stall" { <A> = 38; <B> = 56; };
    xkb_types "stall" { include "basic" };
    xkb_compat "stall" { include "basic" };
    xkb_symbols "stall" { key <A> { [ a, A ] }; key <B> { [ b, B ] }; };
};"#;

/// The xkb real modifier mask for Control.
const CTRL_MASK: u32 = 1 << 2;

const TEXT_MIME: &str = "text/plain;charset=utf-8";

/// Ids the compositor allocates start here.
const SERVER_ID_BASE: u32 = 0xff00_0000;

/// A display refresh, for frame callbacks.
const REFRESH: Duration = Duration::from_micros(16_667);

/// Interface, version and global name, in the order a compositor lists them.
const GLOBALS: [(u32, &str, u32); 5] = [
    (1, proto::IFACE_COMPOSITOR, 4),
    (2, proto::IFACE_SHM, 1),
    (3, proto::IFACE_SEAT, 5),
    (4, proto::IFACE_LAYER_SHELL, 4),
    (5, proto::IFACE_DATA_DEVICE_MANAGER, 3),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Registry,
    Compositor,
    Shm,
    Seat,
    LayerShell,
    DataDeviceManager,
    Surface,
    LayerSurface,
    Pool,
    Buffer,
    Keyboard,
    Pointer,
    DataDevice,
    DataSource,
    DataOffer,
}

/// The compositor's side of the socket. It answers what a compositor answers,
/// and each switch withholds one answer.
struct Peer {
    fd: Fd,
    objects: HashMap<u32, Kind>,
    inbox: Vec<u8>,
    fds: VecDeque<Fd>,
    outbox: Vec<u8>,
    closed: bool,
    serial: u32,
    next_server_id: u32,

    surface: u32,
    layer_surface: u32,
    keyboard: u32,
    pointer: u32,
    data_device: u32,
    focused: bool,

    /// The size a set_size asked for, configured at the next commit.
    requested: Option<(u32, u32)>,
    /// A configure the compositor is sitting on.
    withheld: Option<(u32, u32)>,
    attached: Option<u32>,
    held: Vec<u32>,
    frame_callbacks: Vec<u32>,
    frame_due: Option<Instant>,

    /// Off: a commit's configure is withheld.
    answer_configure: bool,
    /// Off: no buffer is ever released.
    release_buffers: bool,
    /// Off: nothing the launcher sends is read.
    reading: bool,

    /// Write ends of the pipes a paste handed over.
    paste_pipes: Vec<Fd>,
    /// The data source a copy made the selection.
    selection: Option<u32>,
    /// When each frame was committed.
    commits: Vec<Instant>,
}

impl Peer {
    fn new(fd: Fd) -> Self {
        let raw = fd.as_raw_fd();
        let flags = syscall::fcntl(raw, syscall::F_GETFL, 0);
        assert!(syscall::fcntl(raw, syscall::F_SETFL, flags | syscall::O_NONBLOCK) >= 0);
        Peer {
            fd,
            objects: HashMap::new(),
            inbox: Vec::new(),
            fds: VecDeque::new(),
            outbox: Vec::new(),
            closed: false,
            serial: 0,
            next_server_id: SERVER_ID_BASE,
            surface: 0,
            layer_surface: 0,
            keyboard: 0,
            pointer: 0,
            data_device: 0,
            focused: false,
            requested: None,
            withheld: None,
            attached: None,
            held: Vec::new(),
            frame_callbacks: Vec::new(),
            frame_due: None,
            answer_configure: true,
            release_buffers: true,
            reading: true,
            paste_pipes: Vec::new(),
            selection: None,
            commits: Vec::new(),
        }
    }

    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    // -- Sending events --

    /// Queue an event behind whatever is already queued, and send what fits.
    fn send(&mut self, object: u32, opcode: u16, args: &[Arg]) {
        let mut msg: ArrayVec<u8, 4096> = ArrayVec::new();
        wire::encode(&mut msg, object, opcode, args).expect("event");
        self.outbox.extend_from_slice(msg.as_slice());
        self.flush_some();
    }

    /// An event carrying one fd. Everything queued goes first, so the fd rides
    /// with the event that declares it.
    fn send_fd(&mut self, object: u32, opcode: u16, args: &[Arg], fd: RawFd) {
        self.flush_all();
        let mut msg: ArrayVec<u8, 4096> = ArrayVec::new();
        wire::encode(&mut msg, object, opcode, args).expect("event");
        let bytes = msg.as_slice();
        let sent = loop {
            let r = syscall::send_with_fds(self.fd.as_raw_fd(), bytes, &[fd]);
            if r >= 0 {
                break r as usize;
            }
            assert_eq!(-r as i32, syscall::EAGAIN, "event with fd failed: {r}");
            self.wait_writable();
        };
        self.outbox.extend_from_slice(&bytes[sent..]);
        self.flush_all();
    }

    /// Send as much of the queue as the socket takes right now.
    fn flush_some(&mut self) {
        while !self.outbox.is_empty() && !self.closed {
            let r = syscall::send_with_fds(self.fd.as_raw_fd(), &self.outbox, &[]);
            if r > 0 {
                self.outbox.drain(..r as usize);
                continue;
            }
            if -r as i32 != syscall::EAGAIN {
                self.closed = true;
            }
            return;
        }
    }

    /// Send the whole queue, waiting on the socket as long as it takes.
    fn flush_all(&mut self) {
        while !self.outbox.is_empty() && !self.closed {
            self.flush_some();
            if !self.outbox.is_empty() {
                self.wait_writable();
            }
        }
    }

    fn wait_writable(&self) {
        let mut pfd = syscall::pollfd {
            fd: self.fd.as_raw_fd(),
            events: syscall::POLLOUT,
            revents: 0,
        };
        syscall::poll(core::slice::from_mut(&mut pfd), 100);
    }

    // -- Serving requests --

    /// Answer the launcher for a while, then return.
    fn pump_for(&mut self, d: Duration) {
        self.pump_until(Instant::now() + d, |_| false);
    }

    /// Answer the launcher until done says so or the deadline passes. True when
    /// done said so.
    fn pump_until(&mut self, deadline: Instant, mut done: impl FnMut(&Peer) -> bool) -> bool {
        loop {
            if done(self) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            self.flush_some();
            let mut wait = (deadline - now).min(Duration::from_millis(5));
            if let Some(due) = self.frame_due {
                wait = wait.min(due.saturating_duration_since(now));
            }
            if self.reading && !self.closed {
                let mut pfd = syscall::pollfd {
                    fd: self.fd.as_raw_fd(),
                    events: syscall::POLLIN,
                    revents: 0,
                };
                syscall::poll(core::slice::from_mut(&mut pfd), wait.as_millis() as i32);
                self.read_available();
                self.handle_requests();
            } else {
                std::thread::sleep(wait);
            }
            self.fire_frame_callbacks();
        }
    }

    fn read_available(&mut self) {
        let mut chunk = [0u8; 4096];
        while !self.closed {
            let mut fds: ArrayVec<Fd, 16> = ArrayVec::new();
            match syscall::recv_with_fds(self.fd.as_raw_fd(), &mut chunk, &mut fds) {
                Ok(n) if n > 0 => {
                    self.inbox.extend_from_slice(&chunk[..n as usize]);
                    while let Some(fd) = fds.remove(0) {
                        self.fds.push_back(fd);
                    }
                }
                Ok(n) if -n as i32 == syscall::EAGAIN => return,
                Ok(_) | Err(_) => self.closed = true,
            }
        }
    }

    fn handle_requests(&mut self) {
        while self.inbox.len() >= 8 {
            let (object, opcode, size) = wire::parse_header(&self.inbox).expect("header");
            if self.inbox.len() < size {
                return;
            }
            let body: Vec<u8> = self.inbox[8..size].to_vec();
            self.inbox.drain(..size);
            self.request(object, opcode, &body);
        }
    }

    fn request(&mut self, object: u32, opcode: u16, body: &[u8]) {
        let mut r = wire::Reader::new(body);
        if object == proto::WL_DISPLAY {
            match opcode {
                proto::wl_display::SYNC => {
                    let callback = r.u32().expect("callback");
                    self.callback_done(callback);
                }
                proto::wl_display::GET_REGISTRY => {
                    let registry = r.u32().expect("registry");
                    self.objects.insert(registry, Kind::Registry);
                    for (name, iface, version) in GLOBALS {
                        self.send(
                            registry,
                            proto::wl_registry::EV_GLOBAL,
                            &[Arg::Uint(name), Arg::Str(iface), Arg::Uint(version)],
                        );
                    }
                }
                _ => {}
            }
            return;
        }

        let Some(kind) = self.objects.get(&object).copied() else {
            return;
        };
        match (kind, opcode) {
            (Kind::Registry, proto::wl_registry::BIND) => {
                let _name = r.u32().expect("name");
                let iface = r.string().expect("interface").to_string();
                let _version = r.u32().expect("version");
                let id = r.u32().expect("id");
                let kind = match iface.as_str() {
                    proto::IFACE_COMPOSITOR => Kind::Compositor,
                    proto::IFACE_SHM => Kind::Shm,
                    proto::IFACE_SEAT => Kind::Seat,
                    proto::IFACE_LAYER_SHELL => Kind::LayerShell,
                    proto::IFACE_DATA_DEVICE_MANAGER => Kind::DataDeviceManager,
                    _ => return,
                };
                self.objects.insert(id, kind);
                if kind == Kind::Seat {
                    let caps = proto::wl_seat::CAP_KEYBOARD | proto::wl_seat::CAP_POINTER;
                    self.send(id, proto::wl_seat::EV_CAPABILITIES, &[Arg::Uint(caps)]);
                }
            }
            (Kind::Compositor, proto::wl_compositor::CREATE_SURFACE) => {
                self.surface = r.u32().expect("surface");
                self.objects.insert(self.surface, Kind::Surface);
            }
            (Kind::LayerShell, proto::zwlr_layer_shell_v1::GET_LAYER_SURFACE) => {
                self.layer_surface = r.u32().expect("layer surface");
                self.objects.insert(self.layer_surface, Kind::LayerSurface);
            }
            (Kind::LayerSurface, proto::zwlr_layer_surface_v1::SET_SIZE) => {
                let width = r.u32().expect("width");
                let height = r.u32().expect("height");
                self.requested = Some((width, height));
            }
            (Kind::Surface, proto::wl_surface::ATTACH) => {
                let buffer = r.u32().expect("buffer");
                self.attached = (buffer != 0).then_some(buffer);
            }
            (Kind::Surface, proto::wl_surface::FRAME) => {
                self.frame_callbacks.push(r.u32().expect("callback"));
            }
            (Kind::Surface, proto::wl_surface::COMMIT) => self.commit(),
            (Kind::Shm, proto::wl_shm::CREATE_POOL) => {
                let pool = r.u32().expect("pool");
                self.objects.insert(pool, Kind::Pool);
                // The memfd behind the pool; nothing here reads the pixels.
                drop(self.fds.pop_front());
            }
            (Kind::Pool, proto::wl_shm_pool::CREATE_BUFFER) => {
                let buffer = r.u32().expect("buffer");
                self.objects.insert(buffer, Kind::Buffer);
            }
            (Kind::Pool, proto::wl_shm_pool::DESTROY)
            | (Kind::Buffer, proto::wl_buffer::DESTROY)
            | (Kind::DataSource, proto::wl_data_source::DESTROY) => {
                self.objects.remove(&object);
                self.held.retain(|&b| b != object);
                self.send(
                    proto::WL_DISPLAY,
                    proto::wl_display::EV_DELETE_ID,
                    &[Arg::Uint(object)],
                );
            }
            (Kind::Seat, proto::wl_seat::GET_KEYBOARD) => {
                self.keyboard = r.u32().expect("keyboard");
                self.objects.insert(self.keyboard, Kind::Keyboard);
                self.send_keymap();
                self.send(
                    self.keyboard,
                    proto::wl_keyboard::EV_REPEAT_INFO,
                    &[Arg::Int(25), Arg::Int(400)],
                );
            }
            (Kind::Seat, proto::wl_seat::GET_POINTER) => {
                self.pointer = r.u32().expect("pointer");
                self.objects.insert(self.pointer, Kind::Pointer);
            }
            (Kind::DataDeviceManager, proto::wl_data_device_manager::CREATE_DATA_SOURCE) => {
                let source = r.u32().expect("source");
                self.objects.insert(source, Kind::DataSource);
            }
            (Kind::DataDeviceManager, proto::wl_data_device_manager::GET_DATA_DEVICE) => {
                self.data_device = r.u32().expect("device");
                self.objects.insert(self.data_device, Kind::DataDevice);
            }
            (Kind::DataDevice, proto::wl_data_device::SET_SELECTION) => {
                let source = r.u32().expect("source");
                self.selection = (source != 0).then_some(source);
            }
            (Kind::DataOffer, proto::wl_data_offer::RECEIVE) => {
                if let Some(fd) = self.fds.pop_front() {
                    self.paste_pipes.push(fd);
                }
            }
            _ => {}
        }
    }

    fn callback_done(&mut self, callback: u32) {
        self.send(callback, proto::wl_callback::EV_DONE, &[Arg::Uint(0)]);
        self.send(
            proto::WL_DISPLAY,
            proto::wl_display::EV_DELETE_ID,
            &[Arg::Uint(callback)],
        );
    }

    fn commit(&mut self) {
        if let Some(buffer) = self.attached.take() {
            self.commits.push(Instant::now());
            // Latching a new buffer is what frees the one shown before it.
            if self.release_buffers {
                self.release_held();
            }
            self.held.push(buffer);
            if !self.frame_callbacks.is_empty() && self.frame_due.is_none() {
                self.frame_due = Some(Instant::now() + REFRESH);
            }
            if !self.focused {
                // A layer surface with keyboard interactivity gets focus once
                // it is mapped.
                self.focused = true;
                let serial = self.next_serial();
                self.send(
                    self.keyboard,
                    proto::wl_keyboard::EV_ENTER,
                    &[Arg::Uint(serial), Arg::Object(self.surface), Arg::Uint(0)],
                );
            }
        }
        if let Some(size) = self.requested.take() {
            if self.answer_configure {
                self.configure(size);
            } else {
                self.withheld = Some(size);
            }
        }
    }

    fn configure(&mut self, (width, height): (u32, u32)) {
        let serial = self.next_serial();
        self.send(
            self.layer_surface,
            proto::zwlr_layer_surface_v1::EV_CONFIGURE,
            &[Arg::Uint(serial), Arg::Uint(width), Arg::Uint(height)],
        );
    }

    fn release_held(&mut self) {
        for buffer in std::mem::take(&mut self.held) {
            self.send(buffer, proto::wl_buffer::EV_RELEASE, &[]);
        }
    }

    fn fire_frame_callbacks(&mut self) {
        let Some(due) = self.frame_due else {
            return;
        };
        if Instant::now() < due {
            return;
        }
        self.frame_due = None;
        for callback in std::mem::take(&mut self.frame_callbacks) {
            self.callback_done(callback);
        }
    }

    fn send_keymap(&mut self) {
        let path = std::env::temp_dir().join(format!(
            "bnk_stall_keymap_{}_{}",
            std::process::id(),
            self.keyboard
        ));
        let mut bytes = KEYMAP.as_bytes().to_vec();
        bytes.push(0);
        std::fs::write(&path, &bytes).expect("keymap file");
        let file = std::fs::File::open(&path).expect("keymap open");
        let _ = std::fs::remove_file(&path);
        let fd = Fd::new(file.into_raw_fd());
        self.send_fd(
            self.keyboard,
            proto::wl_keyboard::EV_KEYMAP,
            &[Arg::Uint(1), Arg::Uint(bytes.len() as u32)],
            fd.as_raw_fd(),
        );
    }

    // -- Input --

    fn key(&mut self, key: u32, pressed: bool) {
        let serial = self.next_serial();
        let state = u32::from(pressed);
        self.send(
            self.keyboard,
            proto::wl_keyboard::EV_KEY,
            &[
                Arg::Uint(serial),
                Arg::Uint(0),
                Arg::Uint(key),
                Arg::Uint(state),
            ],
        );
    }

    fn modifiers(&mut self, depressed: u32) {
        let serial = self.next_serial();
        self.send(
            self.keyboard,
            proto::wl_keyboard::EV_MODIFIERS,
            &[
                Arg::Uint(serial),
                Arg::Uint(depressed),
                Arg::Uint(0),
                Arg::Uint(0),
                Arg::Uint(0),
            ],
        );
    }

    fn pointer_enter(&mut self, x: i32, y: i32) {
        let serial = self.next_serial();
        self.send(
            self.pointer,
            proto::wl_pointer::EV_ENTER,
            &[
                Arg::Uint(serial),
                Arg::Object(self.surface),
                Arg::Int(x * 256),
                Arg::Int(y * 256),
            ],
        );
    }

    fn motion_bytes(x: i32, y: i32, out: &mut Vec<u8>, pointer: u32) {
        let mut msg: ArrayVec<u8, 64> = ArrayVec::new();
        wire::encode(
            &mut msg,
            pointer,
            proto::wl_pointer::EV_MOTION,
            &[Arg::Uint(0), Arg::Int(x * 256), Arg::Int(y * 256)],
        )
        .expect("motion");
        out.extend_from_slice(msg.as_slice());
    }

    fn button(&mut self, pressed: bool) {
        let serial = self.next_serial();
        self.send(
            self.pointer,
            proto::wl_pointer::EV_BUTTON,
            &[
                Arg::Uint(serial),
                Arg::Uint(0),
                Arg::Uint(proto::BTN_LEFT),
                Arg::Uint(u32::from(pressed)),
            ],
        );
    }

    /// Put text on the clipboard from another application.
    fn offer_selection(&mut self) {
        self.next_server_id += 1;
        let offer = self.next_server_id;
        self.objects.insert(offer, Kind::DataOffer);
        self.send(
            self.data_device,
            proto::wl_data_device::EV_DATA_OFFER,
            &[Arg::NewId(offer)],
        );
        self.send(
            offer,
            proto::wl_data_offer::EV_OFFER,
            &[Arg::Str(TEXT_MIME)],
        );
        self.send(
            self.data_device,
            proto::wl_data_device::EV_SELECTION,
            &[Arg::Object(offer)],
        );
    }

    /// Bytes the launcher has not read yet.
    fn unread_by_launcher(&self) -> i32 {
        let mut queued: i32 = 0;
        // SAFETY: TIOCOUTQ writes one int through the pointer, which points at
        // a live local.
        let r = unsafe { ioctl(self.fd.as_raw_fd(), TIOCOUTQ, &mut queued) };
        assert_eq!(r, 0, "TIOCOUTQ");
        queued
    }
}

const TIOCOUTQ: u64 = 0x5411;
const SOL_SOCKET: i32 = 1;
const SO_SNDBUF: i32 = 7;

unsafe extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
    fn setsockopt(fd: i32, level: i32, name: i32, value: *const i32, len: u32) -> i32;
}

/// What the launcher left behind when its loop ended.
struct Outcome {
    result: Result<(), String>,
    text: String,
}

/// The launcher on one end of a socketpair and the fake compositor on the
/// other, with bring-up done and the first frame on screen.
struct Harness {
    peer: Peer,
    outcome: mpsc::Receiver<Outcome>,
    thread: &'static str,
    /// From starting the launcher to its first frame.
    first_frame: Duration,
}

impl Harness {
    fn start(thread: &'static str) -> Harness {
        Self::start_with(thread, |_| {})
    }

    fn start_with(thread: &'static str, prepare: impl FnOnce(&Connection)) -> Harness {
        let (socket, peer_fd) = Connection::pair();
        prepare(&socket);
        let (tx, outcome) = mpsc::channel();
        let started = Instant::now();
        std::thread::Builder::new()
            .name(thread.to_string())
            .spawn(move || {
                let outcome = match Client::over(socket) {
                    Ok(mut client) => {
                        let font = Font::from_path(FIXTURE_FONT).expect("fixture font");
                        let mut state = AppState::new(catalog(), Default::default());
                        let result =
                            app::run_window(&mut client, &mut state, &font, false, &Theme::DEFAULT)
                                .map_err(|e| e.to_string());
                        Outcome {
                            result,
                            text: client.editor.text_owned().as_str().to_string(),
                        }
                    }
                    Err(e) => Outcome {
                        result: Err(e.to_string()),
                        text: String::new(),
                    },
                };
                let _ = tx.send(outcome);
            })
            .expect("launcher thread");

        let mut peer = Peer::new(peer_fd);
        let up = peer.pump_until(Instant::now() + Duration::from_secs(5), |p| {
            !p.commits.is_empty() && p.focused
        });
        if !up {
            let ended = outcome.try_recv().ok().map(|o| o.result);
            panic!(
                "{thread}: the launcher never drew its first frame (commits {}, focused {}, \
                 surface {}, keyboard {}, closed {}, loop {ended:?})",
                peer.commits.len(),
                peer.focused,
                peer.surface,
                peer.keyboard,
                peer.closed
            );
        }
        let first_frame = peer.commits[0] - started;
        peer.pump_for(Duration::from_millis(50));
        Harness {
            peer,
            outcome,
            thread,
            first_frame,
        }
    }

    fn tap(&mut self, key: u32) {
        self.peer.key(key, true);
        self.peer.key(key, false);
    }

    fn ctrl_tap(&mut self, key: u32) {
        self.peer.modifiers(CTRL_MASK);
        self.tap(key);
        self.peer.modifiers(0);
    }

    /// Tap a key and time how long until the next frame is committed. None
    /// when none came within the timeout.
    fn tap_to_frame(&mut self, key: u32, timeout: Duration) -> Option<Duration> {
        let before = self.peer.commits.len();
        let sent = Instant::now();
        self.tap(key);
        self.frame_after(before, sent, timeout)
    }

    fn frame_after(
        &mut self,
        before: usize,
        since: Instant,
        timeout: Duration,
    ) -> Option<Duration> {
        self.peer
            .pump_until(since + timeout, |p| p.commits.len() > before)
            .then(|| since.elapsed())
    }

    /// The launcher thread's context switches so far: every time it parked and
    /// woke again.
    fn wakeups(&self) -> u64 {
        let tasks = std::fs::read_dir("/proc/self/task").expect("tasks");
        for task in tasks.flatten() {
            let dir = task.path();
            let comm = std::fs::read_to_string(dir.join("comm")).unwrap_or_default();
            if comm.trim() != self.thread {
                continue;
            }
            let status = std::fs::read_to_string(dir.join("status")).expect("status");
            return status
                .lines()
                .filter(|l| l.contains("ctxt_switches"))
                .filter_map(|l| l.split_whitespace().last()?.parse::<u64>().ok())
                .sum();
        }
        panic!("{}: no launcher thread", self.thread);
    }

    /// Dismiss the launcher the way losing focus does, and collect what it
    /// left. None when it did not end within five seconds.
    fn finish(mut self) -> Option<Outcome> {
        // The launcher may not be reading; anything still unread goes first.
        self.peer.reading = true;
        let serial = self.peer.next_serial();
        self.peer.send(
            self.peer.keyboard,
            proto::wl_keyboard::EV_LEAVE,
            &[Arg::Uint(serial), Arg::Object(self.peer.surface)],
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.outcome.try_recv() {
                Ok(outcome) => return Some(outcome),
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!("{}: launcher panicked", self.thread)
                }
                Err(mpsc::TryRecvError::Empty) if Instant::now() > deadline => return None,
                Err(mpsc::TryRecvError::Empty) => self.peer.pump_for(Duration::from_millis(10)),
            }
        }
    }
}

/// Enough entries that every query the scenarios type fills the list, so only
/// the first letter changes the window's height.
fn catalog() -> Catalog {
    let mut entries = Catalog::new();
    for i in 0..12 {
        let name = format!("{}b tool {i:02}", "a".repeat(40));
        let entry = DesktopEntry::new(&name, "true").expect("entry");
        let _ = entries.push(entry);
    }
    entries
}

/// A pipe already full, with its write end back in blocking mode: what a
/// requester that never drains its pipe hands over.
fn full_pipe() -> (Fd, Fd) {
    let mut fds = [0i32; 2];
    assert_eq!(syscall::pipe2(&mut fds, syscall::O_CLOEXEC), 0);
    let (read_end, write_end) = (Fd::new(fds[0]), Fd::new(fds[1]));
    let w = write_end.as_raw_fd();
    let flags = syscall::fcntl(w, syscall::F_GETFL, 0);
    syscall::fcntl(w, syscall::F_SETFL, flags | syscall::O_NONBLOCK);
    let chunk = [b'x'; 4096];
    while syscall::write_fd(w, &chunk) > 0 {}
    while syscall::write_fd(w, &chunk[..1]) > 0 {}
    syscall::fcntl(w, syscall::F_SETFL, flags);
    (read_end, write_end)
}

fn drain(fd: &Fd) {
    let raw = fd.as_raw_fd();
    let flags = syscall::fcntl(raw, syscall::F_GETFL, 0);
    syscall::fcntl(raw, syscall::F_SETFL, flags | syscall::O_NONBLOCK);
    let mut buf = [0u8; 4096];
    while syscall::read_fd(raw, &mut buf) > 0 {}
}

fn ms(d: Option<Duration>) -> String {
    match d {
        Some(d) => format!("{} ms", d.as_millis()),
        None => "never".to_string(),
    }
}

/// When a key reached the screen, as a phrase.
fn shown(d: Option<Duration>) -> String {
    match d {
        Some(d) => format!("showed after {} ms", d.as_millis()),
        None => "never showed".to_string(),
    }
}

fn ended(outcome: &Option<Outcome>) -> &'static str {
    match outcome {
        Some(Outcome { result: Ok(()), .. }) => "",
        Some(Outcome { result: Err(_), .. }) => " (the loop ended in an error)",
        None => " (the launcher did not exit)",
    }
}

// -- Scenarios --

/// A paste whose owner takes the pipe and never writes to it.
fn paste_from_a_silent_owner() -> String {
    let mut h = Harness::start("stall-paste");
    h.peer.offer_selection();
    h.ctrl_tap(KEY_V);
    let asked = h
        .peer
        .pump_until(Instant::now() + Duration::from_secs(1), |p| {
            !p.paste_pipes.is_empty()
        });
    assert!(asked, "the paste never asked for the data");
    h.peer.pump_for(Duration::from_millis(100));
    let key = h.tap_to_frame(KEY_A, Duration::from_secs(4));
    let out = h.finish();
    format!("a key typed meanwhile {}{}", shown(key), ended(&out))
}

/// A paste whose owner sends a byte every 150 ms and never finishes.
fn paste_from_a_trickling_owner() -> String {
    let mut h = Harness::start("stall-trickle");
    h.peer.offer_selection();
    h.ctrl_tap(KEY_V);
    let asked = h
        .peer
        .pump_until(Instant::now() + Duration::from_secs(1), |p| {
            !p.paste_pipes.is_empty()
        });
    assert!(asked, "the paste never asked for the data");
    let pipe = h.peer.paste_pipes.pop().expect("pipe");
    let before = h.peer.commits.len();
    let started = Instant::now();
    let mut sent = 0;
    let mut first_frame = None;
    while started.elapsed() < Duration::from_millis(3000) {
        if syscall::write_fd(pipe.as_raw_fd(), b"x") == 1 {
            sent += 1;
        }
        h.peer.pump_for(Duration::from_millis(150));
        if first_frame.is_none() && h.peer.commits.len() > before {
            first_frame = Some(started.elapsed());
        }
    }
    drop(pipe);
    let out = h.finish();
    let pasted = out
        .as_ref()
        .map(|o| o.text.matches('x').count())
        .unwrap_or(0);
    format!(
        "no frame for {}; {pasted} of the {sent} bytes sent were pasted{}",
        ms(first_frame),
        ended(&out)
    )
}

/// Enter pressed right after Ctrl+V, before the owner has written.
fn enter_during_a_paste() -> String {
    let mut h = Harness::start("stall-enter");
    h.peer.offer_selection();
    h.ctrl_tap(KEY_V);
    let asked = h
        .peer
        .pump_until(Instant::now() + Duration::from_secs(1), |p| {
            !p.paste_pipes.is_empty()
        });
    assert!(asked, "the paste never asked for the data");
    h.tap(KEY_ENTER);
    h.peer.pump_for(Duration::from_millis(50));
    let pipe = h.peer.paste_pipes.pop().expect("pipe");
    // EPIPE when the launcher has already gone and closed its end.
    let wrote = syscall::write_fd(pipe.as_raw_fd(), b"xyz") == 3;
    drop(pipe);
    h.peer.pump_for(Duration::from_millis(100));
    let out = h.finish();
    let text = out.as_ref().map(|o| o.text.clone()).unwrap_or_default();
    let pipe = if wrote {
        ""
    } else {
        ", its pipe already closed"
    };
    format!(
        "Enter 50 ms before the owner wrote launched with {text:?}{pipe}{}",
        ended(&out)
    )
}

/// A copy served to a requester whose pipe is already full.
fn copy_to_a_full_pipe() -> String {
    let mut h = Harness::start("stall-copy");
    h.tap_to_frame(KEY_A, Duration::from_secs(1));
    h.ctrl_tap(KEY_A);
    h.ctrl_tap(KEY_C);
    let copied = h
        .peer
        .pump_until(Instant::now() + Duration::from_secs(1), |p| {
            p.selection.is_some()
        });
    assert!(copied, "the copy never set the selection");
    let source = h.peer.selection.expect("source");
    let (read_end, write_end) = full_pipe();
    h.peer.send_fd(
        source,
        proto::wl_data_source::EV_SEND,
        &[Arg::Str(TEXT_MIME)],
        write_end.as_raw_fd(),
    );
    drop(write_end);
    h.peer.pump_for(Duration::from_millis(100));
    let key = h.tap_to_frame(KEY_RIGHT, Duration::from_millis(1500));
    // The requester finally reads.
    let before = h.peer.commits.len();
    let freed = Instant::now();
    drain(&read_end);
    let after = h.frame_after(before, freed, Duration::from_secs(1));
    drop(read_end);
    let out = h.finish();
    format!(
        "a key typed meanwhile {}; frames resumed {} after the requester read{}",
        shown(key),
        ms(after),
        ended(&out)
    )
}

/// A resize the compositor never answers with a configure.
fn resize_without_a_configure() -> String {
    let mut h = Harness::start("stall-resize");
    h.peer.answer_configure = false;
    // The first letter fills the list, so the window asks for a new height.
    h.tap(KEY_A);
    let asked = h
        .peer
        .pump_until(Instant::now() + Duration::from_secs(1), |p| {
            p.withheld.is_some()
        });
    assert!(asked, "the first letter never resized the window");
    h.peer.pump_for(Duration::from_millis(100));
    let key = h.tap_to_frame(KEY_B, Duration::from_millis(1500));
    let before = h.peer.commits.len();
    let answered = Instant::now();
    h.peer.answer_configure = true;
    let size = h.peer.withheld.take().expect("withheld");
    h.peer.configure(size);
    let after = h.frame_after(before, answered, Duration::from_secs(1));
    let out = h.finish();
    format!(
        "a key typed meanwhile {}; frames resumed {} after the configure came{}",
        shown(key),
        ms(after),
        ended(&out)
    )
}

/// A held key while the compositor keeps both frames.
fn key_repeat_while_frames_are_held() -> String {
    let mut h = Harness::start("stall-frames");
    h.tap_to_frame(KEY_A, Duration::from_secs(1));
    h.peer.pump_for(Duration::from_millis(100));
    h.peer.release_buffers = false;
    h.peer.key(KEY_A, true);
    h.peer.pump_for(Duration::from_millis(1200));
    h.peer.release_buffers = true;
    h.peer.release_held();
    h.peer.pump_for(Duration::from_millis(50));
    h.peer.key(KEY_A, false);
    h.peer.pump_for(Duration::from_millis(100));
    let out = h.finish();
    // One from the press, then one every 40 ms after the 400 ms delay.
    let expected = 1 + (1200 - 400) / 40;
    let typed = out
        .as_ref()
        .map(|o| o.text.matches('a').count().saturating_sub(1))
        .unwrap_or(0);
    format!(
        "a 1.2 s hold typed {typed} letters, {expected} expected{}",
        ended(&out)
    )
}

/// Copies while the compositor stops reading the socket. A copy sends requests
/// without drawing, so it is output that can pile up: a repaint would wait for a
/// frame the compositor never releases first.
fn output_while_the_compositor_stops_reading() -> String {
    let mut h = Harness::start_with("stall-output", |socket| {
        let size: i32 = 4096;
        // SAFETY: setsockopt reads one int through the pointer, which points at
        // a live local.
        let r = unsafe { setsockopt(socket.fd(), SOL_SOCKET, SO_SNDBUF, &size, 4) };
        assert_eq!(r, 0, "SO_SNDBUF");
    });
    h.tap_to_frame(KEY_A, Duration::from_secs(1));
    h.ctrl_tap(KEY_A);
    h.peer.pump_for(Duration::from_millis(100));
    h.peer.reading = false;
    let started = Instant::now();
    let mut stuck_since: Option<Instant> = None;
    let mut longest = Duration::ZERO;
    while started.elapsed() < Duration::from_millis(2000) {
        h.ctrl_tap(KEY_C);
        h.peer.pump_for(Duration::from_millis(20));
        if h.peer.unread_by_launcher() > 0 {
            let since = *stuck_since.get_or_insert_with(Instant::now);
            longest = longest.max(since.elapsed());
        } else {
            stuck_since = None;
        }
    }
    h.peer.reading = true;
    let resumed = Instant::now();
    h.peer.pump_until(resumed + Duration::from_secs(1), |p| {
        p.unread_by_launcher() == 0
    });
    let caught_up = resumed.elapsed();
    let out = h.finish();
    format!(
        "stopped reading input for {} ms of a 2 s stall; caught up {} ms after the compositor read again{}",
        longest.as_millis(),
        caught_up.as_millis(),
        ended(&out)
    )
}

/// A key in the middle of a second of pointer motion sent as fast as the
/// socket takes it.
fn input_during_a_pointer_flood() -> String {
    let mut h = Harness::start("stall-flood");
    h.peer.pointer_enter(300, 200);
    let pointer = h.peer.pointer;
    let mut batch = Vec::new();
    for i in 0..200 {
        Peer::motion_bytes(300 + i % 2, 200, &mut batch, pointer);
    }
    let started = Instant::now();
    let mut key_sent: Option<(Instant, usize)> = None;
    let mut key_latency = None;
    let mut frames = 0;
    let mut last_commits = h.peer.commits.len();
    while started.elapsed() < Duration::from_millis(1500) {
        if h.peer.outbox.is_empty() {
            h.peer.outbox.extend_from_slice(&batch);
        }
        h.peer.flush_some();
        if key_sent.is_none() && started.elapsed() > Duration::from_millis(300) {
            key_sent = Some((Instant::now(), h.peer.commits.len()));
            h.tap(KEY_RIGHT);
        }
        let mut pfd = syscall::pollfd {
            fd: h.peer.fd.as_raw_fd(),
            events: syscall::POLLIN,
            revents: 0,
        };
        syscall::poll(core::slice::from_mut(&mut pfd), 0);
        h.peer.read_available();
        h.peer.handle_requests();
        h.peer.fire_frame_callbacks();
        if let Some((at, before)) = key_sent {
            if key_latency.is_none() && h.peer.commits.len() > before {
                key_latency = Some(at.elapsed());
            }
        }
        if h.peer.commits.len() > last_commits {
            frames += h.peer.commits.len() - last_commits;
            last_commits = h.peer.commits.len();
        }
    }
    h.peer.flush_all();
    let flood_end = Instant::now();
    if key_latency.is_none() {
        if let Some((at, before)) = key_sent {
            h.peer.pump_until(flood_end + Duration::from_secs(2), |p| {
                p.commits.len() > before
            });
            if h.peer.commits.len() > before {
                key_latency = Some(at.elapsed());
            }
        }
    }
    let out = h.finish();
    format!(
        "a key 300 ms into the flood showed after {} (the flood ran 1500 ms); {frames} frames during it{}",
        ms(key_latency),
        ended(&out)
    )
}

/// Frames committed while dragging a selection with the pointer moving every
/// millisecond.
fn frames_during_a_drag() -> String {
    let mut h = Harness::start("stall-drag");
    h.tap_to_frame(KEY_A, Duration::from_secs(1));
    h.peer.pump_for(Duration::from_millis(100));
    h.peer.pointer_enter(60, 27);
    h.peer.button(true);
    let pointer = h.peer.pointer;
    let before = h.peer.commits.len();
    let started = Instant::now();
    let mut i = 0;
    while started.elapsed() < Duration::from_secs(1) {
        let mut msg = Vec::new();
        Peer::motion_bytes(60 + (i % 40), 27, &mut msg, pointer);
        i += 1;
        h.peer.outbox.extend_from_slice(&msg);
        h.peer.pump_for(Duration::from_millis(1));
    }
    let frames = h.peer.commits.len() - before;
    h.peer.button(false);
    let out = h.finish();
    format!("{frames} frames in one second of dragging{}", ended(&out))
}

/// The first frame, from starting the launcher. Bring-up against the fake
/// compositor takes a few milliseconds, so anything near the caret's blink
/// interval is the loop waiting before it paints.
fn first_frame() -> String {
    let h = Harness::start("stall-first");
    let first = h.first_frame;
    let out = h.finish();
    format!(
        "first frame {} ms after start{}",
        first.as_millis(),
        ended(&out)
    )
}

/// Wakeups while nothing happens.
fn wakeups_while_idle() -> String {
    let mut h = Harness::start("stall-idle");
    h.peer.pump_for(Duration::from_millis(300));
    let before = h.wakeups();
    let frames = h.peer.commits.len();
    h.peer.pump_for(Duration::from_secs(2));
    let woke = h.wakeups() - before;
    let frames = h.peer.commits.len() - frames;
    let out = h.finish();
    format!(
        "{} wakeups a second while idle, {} frames a second (the caret blinks every 530 ms){}",
        woke / 2,
        frames as f64 / 2.0,
        ended(&out)
    )
}

/// A scenario's name in the report, and the run that prints its result.
type Scenario = (&'static str, fn() -> String);

#[test]
#[ignore]
fn stall_report() {
    let scenarios: [Scenario; 11] = [
        ("startup", first_frame),
        ("paste, owner never writes", paste_from_a_silent_owner),
        ("paste, owner trickles", paste_from_a_trickling_owner),
        ("paste, then Enter", enter_during_a_paste),
        ("copy, requester's pipe full", copy_to_a_full_pipe),
        ("resize, configure withheld", resize_without_a_configure),
        (
            "key repeat, both frames held",
            key_repeat_while_frames_are_held,
        ),
        (
            "copies, compositor not reading",
            output_while_the_compositor_stops_reading,
        ),
        ("pointer flood (synthetic)", input_during_a_pointer_flood),
        ("drag", frames_during_a_drag),
        ("idle", wakeups_while_idle),
    ];
    println!();
    for (name, run) in scenarios {
        println!("  {name:<30} {}", run());
    }
    println!();
}
