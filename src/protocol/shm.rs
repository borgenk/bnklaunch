//! wl_shm, wl_shm_pool, and wl_buffer protocol implementation.

use crate::error::Result;
use crate::syscall::RawFd;

use crate::protocol::{wl_buffer_request, wl_shm_pool_request, wl_shm_request, ShmFormat};
use crate::socket::WaylandSocket;
use crate::wire::MessageBuilder;

/// Send wl_shm.create_pool request.
/// Creates a shared memory pool from a file descriptor.
pub fn create_pool(
    socket: &mut WaylandSocket,
    shm_id: u32,
    pool_id: u32,
    fd: RawFd,
    size: i32,
) -> Result<()> {
    let mut msg = MessageBuilder::new(shm_id, wl_shm_request::CREATE_POOL);
    msg.put_new_id(pool_id);
    msg.put_i32(size);
    socket.send(&msg.finish()?, &[fd])
}

/// Send wl_shm_pool.create_buffer request.
/// Creates a buffer from the pool.
#[allow(clippy::too_many_arguments)]
pub fn pool_create_buffer(
    socket: &mut WaylandSocket,
    pool_id: u32,
    buffer_id: u32,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: ShmFormat,
) -> Result<()> {
    let mut msg = MessageBuilder::new(pool_id, wl_shm_pool_request::CREATE_BUFFER);
    msg.put_new_id(buffer_id);
    msg.put_i32(offset);
    msg.put_i32(width);
    msg.put_i32(height);
    msg.put_i32(stride);
    msg.put_u32(format as u32);
    socket.send(&msg.finish()?, &[])
}

/// Send wl_shm_pool.destroy request.
/// Destroys the shared memory pool.
pub fn pool_destroy(socket: &mut WaylandSocket, pool_id: u32) -> Result<()> {
    let msg = MessageBuilder::new(pool_id, wl_shm_pool_request::DESTROY);
    socket.send(&msg.finish()?, &[])
}

/// Send wl_buffer.destroy request.
/// Destroys the buffer.
pub fn buffer_destroy(socket: &mut WaylandSocket, buffer_id: u32) -> Result<()> {
    let msg = MessageBuilder::new(buffer_id, wl_buffer_request::DESTROY);
    socket.send(&msg.finish()?, &[])
}
