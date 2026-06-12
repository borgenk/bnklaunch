//! zwlr_layer_shell_v1 protocol implementation.
//!
//! The layer shell protocol allows creating surfaces that appear as
//! overlays above all windows, perfect for launchers, panels, etc.
//!
//! Protocol: https://wayland.app/protocols/wlr-layer-shell-unstable-v1

use crate::error::Result;

use crate::socket::WaylandSocket;
use crate::wire::MessageBuilder;

/// Opcodes for zwlr_layer_shell_v1 requests.
mod zwlr_layer_shell_v1_request {
    pub const GET_LAYER_SURFACE: u16 = 0;
}

/// Opcodes for zwlr_layer_surface_v1 requests.
mod zwlr_layer_surface_v1_request {
    pub const SET_SIZE: u16 = 0;
    pub const SET_ANCHOR: u16 = 1;
    pub const SET_EXCLUSIVE_ZONE: u16 = 2;
    pub const SET_MARGIN: u16 = 3;
    pub const SET_KEYBOARD_INTERACTIVITY: u16 = 4;
    pub const ACK_CONFIGURE: u16 = 6;
}

/// Opcodes for zwlr_layer_surface_v1 events.
pub mod zwlr_layer_surface_v1_event {
    pub const CONFIGURE: u16 = 0;
    pub const CLOSED: u16 = 1;
}

/// Layer values for layer surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Layer {
    Overlay = 3,
}

/// Keyboard interactivity for a layer surface: none=0, exclusive=1, on_demand=2.
///
/// on_demand requires layer-shell v4; on v1-v3 the field is a 0/1 boolean.
/// exclusive holds a keyboard grab; on_demand can yield focus to other windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum KeyboardInteractivity {
    /// No keyboard focus.
    #[allow(dead_code)]
    None = 0,
    /// Exclusive keyboard focus (grab); never yields to other windows.
    Exclusive = 1,
    /// Focusable like a normal window; loses focus on click-away or alt-tab.
    OnDemand = 2,
}

/// Anchor edge flags for layer surfaces.
#[allow(dead_code)]
pub mod anchor {
    pub const TOP: u32 = 1;
    pub const BOTTOM: u32 = 2;
    pub const LEFT: u32 = 4;
    pub const RIGHT: u32 = 8;
}

/// Send zwlr_layer_shell_v1.get_layer_surface request.
///
/// Creates a layer surface from a wl_surface.
pub fn get_layer_surface(
    socket: &mut WaylandSocket,
    layer_shell_id: u32,
    layer_surface_id: u32,
    surface_id: u32,
    output_id: u32, // 0 for default output
    layer: Layer,
    namespace: &str,
) -> Result<()> {
    let mut msg = MessageBuilder::new(
        layer_shell_id,
        zwlr_layer_shell_v1_request::GET_LAYER_SURFACE,
    );
    msg.put_new_id(layer_surface_id);
    msg.put_object(surface_id);
    msg.put_object(output_id); // null (0) = let compositor choose
    msg.put_u32(layer as u32);
    msg.put_string(namespace);
    socket.send(&msg.finish()?, &[])
}

/// Send zwlr_layer_surface_v1.set_size request.
///
/// Sets the desired size of the surface. If either value is 0,
/// the compositor will assign a size based on anchoring.
pub fn set_size(
    socket: &mut WaylandSocket,
    layer_surface_id: u32,
    width: u32,
    height: u32,
) -> Result<()> {
    let mut msg = MessageBuilder::new(layer_surface_id, zwlr_layer_surface_v1_request::SET_SIZE);
    msg.put_u32(width);
    msg.put_u32(height);
    socket.send(&msg.finish()?, &[])
}

/// Send zwlr_layer_surface_v1.set_anchor request.
///
/// Anchor edges determine where the surface is placed on the output.
pub fn set_anchor(
    socket: &mut WaylandSocket,
    layer_surface_id: u32,
    anchor_flags: u32,
) -> Result<()> {
    let mut msg = MessageBuilder::new(layer_surface_id, zwlr_layer_surface_v1_request::SET_ANCHOR);
    msg.put_u32(anchor_flags);
    socket.send(&msg.finish()?, &[])
}

/// Send zwlr_layer_surface_v1.set_exclusive_zone request.
///
/// Exclusive zone reserves space for the surface (like a panel).
/// Set to -1 to ignore exclusive zones from other surfaces.
/// Set to 0 to not reserve any space.
pub fn set_exclusive_zone(
    socket: &mut WaylandSocket,
    layer_surface_id: u32,
    zone: i32,
) -> Result<()> {
    let mut msg = MessageBuilder::new(
        layer_surface_id,
        zwlr_layer_surface_v1_request::SET_EXCLUSIVE_ZONE,
    );
    msg.put_i32(zone);
    socket.send(&msg.finish()?, &[])
}

/// Send zwlr_layer_surface_v1.set_margin request.
///
/// Sets the margin from the anchor edges.
pub fn set_margin(
    socket: &mut WaylandSocket,
    layer_surface_id: u32,
    top: i32,
    right: i32,
    bottom: i32,
    left: i32,
) -> Result<()> {
    let mut msg = MessageBuilder::new(layer_surface_id, zwlr_layer_surface_v1_request::SET_MARGIN);
    msg.put_i32(top);
    msg.put_i32(right);
    msg.put_i32(bottom);
    msg.put_i32(left);
    socket.send(&msg.finish()?, &[])
}

/// Send zwlr_layer_surface_v1.set_keyboard_interactivity request.
///
/// Sets how the surface interacts with keyboard input.
pub fn set_keyboard_interactivity(
    socket: &mut WaylandSocket,
    layer_surface_id: u32,
    interactivity: KeyboardInteractivity,
) -> Result<()> {
    let mut msg = MessageBuilder::new(
        layer_surface_id,
        zwlr_layer_surface_v1_request::SET_KEYBOARD_INTERACTIVITY,
    );
    msg.put_u32(interactivity as u32);
    socket.send(&msg.finish()?, &[])
}

/// Send zwlr_layer_surface_v1.ack_configure request.
///
/// Acknowledges a configure event.
pub fn ack_configure(socket: &mut WaylandSocket, layer_surface_id: u32, serial: u32) -> Result<()> {
    let mut msg = MessageBuilder::new(
        layer_surface_id,
        zwlr_layer_surface_v1_request::ACK_CONFIGURE,
    );
    msg.put_u32(serial);
    socket.send(&msg.finish()?, &[])
}
