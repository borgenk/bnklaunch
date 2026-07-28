//! Hand-transcribed opcodes and constants for the Wayland interfaces this
//! launcher uses.
//!
//! The values come from the protocol XML, where a request's or event's opcode is
//! its position in the interface's list. Only what the launcher actually sends
//! and receives is transcribed, so a gap in the numbering means an opcode nobody
//! here needs, not a mistake. Request opcodes are bare names; event opcodes are
//! prefixed EV_, since the two share a numbering space and would otherwise
//! collide (wl_surface.ATTACH is 1, and so is wl_keyboard's EV_ENTER).

/// The wl_display object always has id 1.
pub const WL_DISPLAY: u32 = 1;

pub mod wl_display {
    pub const SYNC: u16 = 0;
    pub const GET_REGISTRY: u16 = 1;
    pub const EV_ERROR: u16 = 0;
    pub const EV_DELETE_ID: u16 = 1;
}

pub mod wl_registry {
    pub const BIND: u16 = 0;
    pub const EV_GLOBAL: u16 = 0;
    pub const EV_GLOBAL_REMOVE: u16 = 1;
}

pub mod wl_callback {
    pub const EV_DONE: u16 = 0;
}

pub mod wl_compositor {
    pub const CREATE_SURFACE: u16 = 0;
}

pub mod wl_surface {
    pub const ATTACH: u16 = 1;
    pub const DAMAGE: u16 = 2;
    pub const COMMIT: u16 = 6;
}

pub mod wl_seat {
    pub const GET_POINTER: u16 = 0;
    pub const GET_KEYBOARD: u16 = 1;
    pub const EV_CAPABILITIES: u16 = 0;
    pub const EV_NAME: u16 = 1;
    /// Capability bits reported by EV_CAPABILITIES.
    pub const CAP_POINTER: u32 = 1;
    pub const CAP_KEYBOARD: u32 = 2;
}

pub mod wl_keyboard {
    pub const EV_KEYMAP: u16 = 0;
    pub const EV_ENTER: u16 = 1;
    pub const EV_LEAVE: u16 = 2;
    pub const EV_KEY: u16 = 3;
    pub const EV_MODIFIERS: u16 = 4;
    pub const EV_REPEAT_INFO: u16 = 5;
    /// The key state argument of EV_KEY.
    pub const KEY_PRESSED: u32 = 1;
}

pub mod wl_pointer {
    pub const EV_ENTER: u16 = 0;
    pub const EV_LEAVE: u16 = 1;
    pub const EV_MOTION: u16 = 2;
    pub const EV_BUTTON: u16 = 3;
    /// The state argument of EV_BUTTON.
    pub const BUTTON_PRESSED: u32 = 1;
}

/// Linux input button codes, as they arrive in wl_pointer EV_BUTTON.
pub const BTN_LEFT: u32 = 0x110;

// The clipboard: wl_data_device_manager, wl_data_device, wl_data_source, and
// wl_data_offer together. Setting and reading the selection both require
// keyboard focus, which the launcher holds while it is open.

pub mod wl_data_device_manager {
    pub const CREATE_DATA_SOURCE: u16 = 0;
    pub const GET_DATA_DEVICE: u16 = 1;
}

pub mod wl_data_device {
    pub const SET_SELECTION: u16 = 1;
    pub const EV_DATA_OFFER: u16 = 0;
    pub const EV_SELECTION: u16 = 5;
}

pub mod wl_data_source {
    pub const OFFER: u16 = 0;
    pub const DESTROY: u16 = 1;
    pub const EV_SEND: u16 = 1;
    pub const EV_CANCELLED: u16 = 2;
}

pub mod wl_data_offer {
    pub const RECEIVE: u16 = 1;
    pub const DESTROY: u16 = 2;
    pub const EV_OFFER: u16 = 0;
}

// Shared-memory buffers. The launcher renders in software and posts pixels
// through wl_shm.

pub mod wl_shm {
    pub const CREATE_POOL: u16 = 0;
}

pub mod wl_shm_pool {
    pub const CREATE_BUFFER: u16 = 0;
    pub const DESTROY: u16 = 1;
}

pub mod wl_buffer {
    pub const DESTROY: u16 = 0;
    pub const EV_RELEASE: u16 = 0;
}

/// wl_shm pixel formats. Only the one the renderer writes is listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ShmFormat {
    Argb8888 = 0,
}

// The layer shell (wlr-layer-shell-unstable-v1): surfaces that sit above
// ordinary windows, which is what makes the launcher an overlay.

pub mod zwlr_layer_shell_v1 {
    pub const GET_LAYER_SURFACE: u16 = 0;
}

pub mod zwlr_layer_surface_v1 {
    pub const SET_SIZE: u16 = 0;
    pub const SET_ANCHOR: u16 = 1;
    pub const SET_EXCLUSIVE_ZONE: u16 = 2;
    pub const SET_MARGIN: u16 = 3;
    pub const SET_KEYBOARD_INTERACTIVITY: u16 = 4;
    pub const ACK_CONFIGURE: u16 = 6;
    pub const EV_CONFIGURE: u16 = 0;
    pub const EV_CLOSED: u16 = 1;
}

/// Which layer a layer surface sits in. The launcher is an overlay, above
/// everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Layer {
    Overlay = 3,
}

/// How a layer surface takes keyboard input.
///
/// OnDemand needs layer-shell v4; on v1 through v3 the field is a 0/1 boolean.
/// Exclusive holds a keyboard grab, OnDemand can yield focus to other windows,
/// which is what lets a click elsewhere dismiss the launcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum KeyboardInteractivity {
    Exclusive = 1,
    OnDemand = 2,
}

/// Anchor edges for a layer surface. Anchoring to none of them centres it. The
/// launcher only ever anchors to the top and offsets with a margin, so the other
/// three edges are left out until something needs them.
pub mod anchor {
    pub const TOP: u32 = 1;
}

// Interface names and the versions bound against them. A bind asks for a
// version and the compositor answers with an object speaking it, so the number
// is a promise about which requests and events exist: raise one only alongside
// the code that uses what the newer version adds.

pub const IFACE_COMPOSITOR: &str = "wl_compositor";
pub const IFACE_SHM: &str = "wl_shm";
pub const IFACE_SEAT: &str = "wl_seat";
pub const IFACE_LAYER_SHELL: &str = "zwlr_layer_shell_v1";
pub const IFACE_DATA_DEVICE_MANAGER: &str = "wl_data_device_manager";

pub const VERSION_COMPOSITOR: u32 = 4;
pub const VERSION_SHM: u32 = 1;
pub const VERSION_SEAT: u32 = 5;
/// v4 is what KeyboardInteractivity::OnDemand needs.
pub const VERSION_LAYER_SHELL: u32 = 4;
pub const VERSION_DATA_DEVICE_MANAGER: u32 = 3;
