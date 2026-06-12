//! wl_display and wl_registry protocol implementation.

use crate::error::Result;

use crate::protocol::{object, wl_display_request, wl_registry_request};
use crate::socket::WaylandSocket;
use crate::wire::MessageBuilder;

/// Send wl_display.get_registry request.
/// Creates a new wl_registry object with the given ID.
pub fn get_registry(socket: &mut WaylandSocket, registry_id: u32) -> Result<()> {
    let mut msg = MessageBuilder::new(object::WL_DISPLAY, wl_display_request::GET_REGISTRY);
    msg.put_new_id(registry_id);
    socket.send(&msg.finish()?, &[])
}

/// Send wl_display.sync request.
/// Creates a callback that fires when all previous requests are processed.
pub fn sync(socket: &mut WaylandSocket, callback_id: u32) -> Result<()> {
    let mut msg = MessageBuilder::new(object::WL_DISPLAY, wl_display_request::SYNC);
    msg.put_new_id(callback_id);
    socket.send(&msg.finish()?, &[])
}

/// Send wl_registry.bind request.
/// Binds to a global object, creating a client-side proxy.
pub fn registry_bind(
    socket: &mut WaylandSocket,
    registry_id: u32,
    name: u32,
    interface: &str,
    version: u32,
    new_id: u32,
) -> Result<()> {
    let mut msg = MessageBuilder::new(registry_id, wl_registry_request::BIND);
    msg.put_u32(name);
    // For wl_registry.bind, new_id is preceded by interface name and version
    msg.put_string(interface);
    msg.put_u32(version);
    msg.put_new_id(new_id);
    socket.send(&msg.finish()?, &[])
}
