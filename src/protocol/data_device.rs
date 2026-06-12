//! Core wl_data_device clipboard protocol.
//!
//! wl_data_device_manager, wl_data_device, wl_data_source, and wl_data_offer
//! together implement the standard Wayland clipboard. Setting and reading the
//! selection both require keyboard focus, which the launcher holds while open.

use crate::error::Result;
use crate::syscall::RawFd;

use crate::socket::WaylandSocket;
use crate::wire::MessageBuilder;

pub mod manager_request {
    pub const CREATE_DATA_SOURCE: u16 = 0;
    pub const GET_DATA_DEVICE: u16 = 1;
}

pub mod device_request {
    pub const SET_SELECTION: u16 = 1;
}

pub mod device_event {
    pub const DATA_OFFER: u16 = 0;
    pub const SELECTION: u16 = 5;
}

pub mod source_request {
    pub const OFFER: u16 = 0;
    pub const DESTROY: u16 = 1;
}

pub mod source_event {
    pub const SEND: u16 = 1;
    pub const CANCELLED: u16 = 2;
}

pub mod offer_request {
    pub const RECEIVE: u16 = 1;
    pub const DESTROY: u16 = 2;
}

pub mod offer_event {
    pub const OFFER: u16 = 0;
}

/// Create a data source for offering clipboard content.
pub fn create_data_source(
    socket: &mut WaylandSocket,
    manager_id: u32,
    source_id: u32,
) -> Result<()> {
    let mut msg = MessageBuilder::new(manager_id, manager_request::CREATE_DATA_SOURCE);
    msg.put_new_id(source_id);
    socket.send(&msg.finish()?, &[])
}

/// Get a data device for the given seat.
pub fn get_data_device(
    socket: &mut WaylandSocket,
    manager_id: u32,
    device_id: u32,
    seat_id: u32,
) -> Result<()> {
    let mut msg = MessageBuilder::new(manager_id, manager_request::GET_DATA_DEVICE);
    msg.put_new_id(device_id);
    msg.put_object(seat_id);
    socket.send(&msg.finish()?, &[])
}

/// Offer a MIME type on a data source.
pub fn source_offer(socket: &mut WaylandSocket, source_id: u32, mime_type: &str) -> Result<()> {
    let mut msg = MessageBuilder::new(source_id, source_request::OFFER);
    msg.put_string(mime_type);
    socket.send(&msg.finish()?, &[])
}

/// Destroy a data source.
pub fn source_destroy(socket: &mut WaylandSocket, source_id: u32) -> Result<()> {
    let msg = MessageBuilder::new(source_id, source_request::DESTROY);
    socket.send(&msg.finish()?, &[])
}

/// Set the clipboard selection to a data source. The serial must come from the
/// input event that triggered the copy; the compositor rejects a stale one.
pub fn set_selection(
    socket: &mut WaylandSocket,
    device_id: u32,
    source_id: u32,
    serial: u32,
) -> Result<()> {
    let mut msg = MessageBuilder::new(device_id, device_request::SET_SELECTION);
    msg.put_object(source_id);
    msg.put_u32(serial);
    socket.send(&msg.finish()?, &[])
}

/// Request clipboard data from an offer, passing the write end of a pipe.
pub fn offer_receive(
    socket: &mut WaylandSocket,
    offer_id: u32,
    mime_type: &str,
    fd: RawFd,
) -> Result<()> {
    let mut msg = MessageBuilder::new(offer_id, offer_request::RECEIVE);
    msg.put_string(mime_type);
    socket.send(&msg.finish()?, &[fd])
}

/// Destroy an offer.
pub fn offer_destroy(socket: &mut WaylandSocket, offer_id: u32) -> Result<()> {
    let msg = MessageBuilder::new(offer_id, offer_request::DESTROY);
    socket.send(&msg.finish()?, &[])
}
