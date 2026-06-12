//! Wayland protocol definitions.
//!
//! This module contains the core Wayland protocol interfaces implemented
//! from the protocol specification.

pub mod compositor;
pub mod data_device;
pub mod display;
pub mod layer_shell;
pub mod seat;
pub mod shm;

/// Interface names as they appear in wl_registry.global events.
pub mod interface {
    pub const WL_COMPOSITOR: &str = "wl_compositor";
    pub const WL_SHM: &str = "wl_shm";
    pub const WL_SEAT: &str = "wl_seat";
    pub const ZWLR_LAYER_SHELL_V1: &str = "zwlr_layer_shell_v1";
    pub const WL_DATA_DEVICE_MANAGER: &str = "wl_data_device_manager";
}

/// Object IDs for well-known objects.
pub mod object {
    /// wl_display is always object ID 1
    pub const WL_DISPLAY: u32 = 1;
}

/// Opcodes for wl_display requests (client → server).
pub mod wl_display_request {
    pub const SYNC: u16 = 0;
    pub const GET_REGISTRY: u16 = 1;
}

/// Opcodes for wl_display events (server → client).
pub mod wl_display_event {
    pub const ERROR: u16 = 0;
    pub const DELETE_ID: u16 = 1;
}

/// Opcodes for wl_registry requests.
pub mod wl_registry_request {
    pub const BIND: u16 = 0;
}

/// Opcodes for wl_registry events.
pub mod wl_registry_event {
    pub const GLOBAL: u16 = 0;
    pub const GLOBAL_REMOVE: u16 = 1;
}

/// Opcodes for wl_callback events.
pub mod wl_callback_event {
    pub const DONE: u16 = 0;
}

/// Opcodes for wl_compositor requests.
pub mod wl_compositor_request {
    pub const CREATE_SURFACE: u16 = 0;
}

/// Opcodes for wl_surface requests.
pub mod wl_surface_request {
    pub const ATTACH: u16 = 1;
    pub const DAMAGE: u16 = 2;
    pub const COMMIT: u16 = 6;
}

/// Opcodes for wl_shm requests.
pub mod wl_shm_request {
    pub const CREATE_POOL: u16 = 0;
}

/// Opcodes for wl_shm_pool requests.
pub mod wl_shm_pool_request {
    pub const CREATE_BUFFER: u16 = 0;
    pub const DESTROY: u16 = 1;
}

/// Opcodes for wl_buffer requests.
pub mod wl_buffer_request {
    pub const DESTROY: u16 = 0;
}

/// Opcodes for wl_buffer events.
pub mod wl_buffer_event {
    pub const RELEASE: u16 = 0;
}

/// wl_shm pixel formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ShmFormat {
    Argb8888 = 0,
}

/// Opcodes for wl_seat requests.
pub mod wl_seat_request {
    pub const GET_POINTER: u16 = 0;
    pub const GET_KEYBOARD: u16 = 1;
}

/// Opcodes for wl_seat events.
pub mod wl_seat_event {
    pub const CAPABILITIES: u16 = 0;
    pub const NAME: u16 = 1;
}

/// wl_seat capability flags.
pub mod wl_seat_capability {
    pub const POINTER: u32 = 1;
    pub const KEYBOARD: u32 = 2;
}

/// Opcodes for wl_pointer events.
pub mod wl_pointer_event {
    pub const ENTER: u16 = 0;
    pub const LEAVE: u16 = 1;
    pub const MOTION: u16 = 2;
    pub const BUTTON: u16 = 3;
}

/// Button state values for wl_pointer.button.
pub mod wl_pointer_button_state {
    pub const PRESSED: u32 = 1;
}

/// Linux input button codes.
pub mod button {
    pub const BTN_LEFT: u32 = 0x110;
}

/// Opcodes for wl_keyboard events.
pub mod wl_keyboard_event {
    pub const KEYMAP: u16 = 0;
    pub const ENTER: u16 = 1;
    pub const LEAVE: u16 = 2;
    pub const KEY: u16 = 3;
    pub const MODIFIERS: u16 = 4;
    pub const REPEAT_INFO: u16 = 5;
}

/// Key state values.
pub mod wl_keyboard_key_state {
    pub const PRESSED: u32 = 1;
}
