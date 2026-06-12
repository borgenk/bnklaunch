//! wl_seat and wl_keyboard protocol implementation.

use crate::error::Result;

use crate::protocol::wl_seat_request;
use crate::socket::WaylandSocket;
use crate::wire::MessageBuilder;

/// Send wl_seat.get_pointer request.
pub fn get_pointer(socket: &mut WaylandSocket, seat_id: u32, pointer_id: u32) -> Result<()> {
    let mut msg = MessageBuilder::new(seat_id, wl_seat_request::GET_POINTER);
    msg.put_new_id(pointer_id);
    socket.send(&msg.finish()?, &[])
}

/// Send wl_seat.get_keyboard request.
pub fn get_keyboard(socket: &mut WaylandSocket, seat_id: u32, keyboard_id: u32) -> Result<()> {
    let mut msg = MessageBuilder::new(seat_id, wl_seat_request::GET_KEYBOARD);
    msg.put_new_id(keyboard_id);
    socket.send(&msg.finish()?, &[])
}
