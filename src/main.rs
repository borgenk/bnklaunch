#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]

mod app;
mod arena;
mod cache;
mod client;
mod clipboard;
mod config;
mod denylist;
mod desktop;
mod env;
mod error;
mod font;
mod freetype;
mod fs;
mod launch;
mod protocol;
mod shm;
mod socket;
mod syscall;
mod time;
mod ui;
mod uring;
mod wire;
mod xkb;

/// Maximum number of results to display
const MAX_RESULTS: usize = 5;

/// Window width
const WINDOW_WIDTH: u32 = 800;

/// C entry point for the no_std build. The C runtime (crt0, pulled in with
/// libc) calls this; it runs the launcher and maps the result to an exit code.
/// Absent under `cargo test`, where the harness provides main.
#[cfg(not(test))]
#[no_mangle]
pub extern "C" fn main(_argc: i32, _argv: *const *const u8) -> i32 {
    match app::run() {
        Ok(()) => 0,
        Err(e) => {
            // Print the cause before exiting: a missing font, a compositor with
            // no layer shell, or io_uring disabled should say so rather than
            // exit silently with code 1.
            elog!("bnklaunch: {e}");
            1
        }
    }
}

/// Panic handler for the no_std build. Panics abort immediately under the build
/// profile, so this exists to satisfy the language and never runs in practice.
#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    syscall::exit_group(101)
}

/// Under the test harness the C entry point is cfg'd out, so reference the real
/// entry here to keep app::run and the UI code it calls from reading as dead.
#[cfg(test)]
fn main() {
    let _ = app::run;
}
