//! wl_compositor and wl_surface protocol implementation.

use crate::error::Result;

use crate::protocol::{wl_compositor_request, wl_surface_request};
use crate::socket::WaylandSocket;
use crate::wire::MessageBuilder;

/// Send wl_compositor.create_surface request.
pub fn create_surface(
    socket: &mut WaylandSocket,
    compositor_id: u32,
    surface_id: u32,
) -> Result<()> {
    let mut msg = MessageBuilder::new(compositor_id, wl_compositor_request::CREATE_SURFACE);
    msg.put_new_id(surface_id);
    socket.send(&msg.finish()?, &[])
}

/// Send wl_surface.attach request.
/// Attaches a buffer to the surface.
pub fn surface_attach(
    socket: &mut WaylandSocket,
    surface_id: u32,
    buffer_id: u32,
    x: i32,
    y: i32,
) -> Result<()> {
    let mut msg = MessageBuilder::new(surface_id, wl_surface_request::ATTACH);
    msg.put_object(buffer_id);
    msg.put_i32(x);
    msg.put_i32(y);
    socket.send(&msg.finish()?, &[])
}

/// Send wl_surface.damage request.
/// Marks a region as damaged (needs redraw).
pub fn surface_damage(
    socket: &mut WaylandSocket,
    surface_id: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<()> {
    let mut msg = MessageBuilder::new(surface_id, wl_surface_request::DAMAGE);
    msg.put_i32(x);
    msg.put_i32(y);
    msg.put_i32(width);
    msg.put_i32(height);
    socket.send(&msg.finish()?, &[])
}

/// Send wl_surface.commit request.
/// Commits pending surface state.
pub fn surface_commit(socket: &mut WaylandSocket, surface_id: u32) -> Result<()> {
    let msg = MessageBuilder::new(surface_id, wl_surface_request::COMMIT);
    socket.send(&msg.finish()?, &[])
}
