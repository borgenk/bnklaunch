//! The launcher application: single-instance lock, desktop-entry resolution,
//! the io_uring event loop, and input handling.

use crate::client::{Client, ClipboardOp, PendingAction};
use crate::desktop::{self, DesktopEntry};
use crate::elog;
use crate::error::{Error, Result};
use crate::time::Instant;
use crate::ui::{calculate_height, draw_ui};
use crate::{arena, cache, config, denylist, env, font, fs, launch, syscall, ui, uring};
use crate::{MAX_RESULTS, WINDOW_WIDTH};

/// Caret blink half-period in milliseconds: solid this long, then hidden this
/// long.
const CURSOR_BLINK_MS: u64 = 530;

/// Submission-queue depth for the event-loop ring. A handful of ops are in
/// flight at once (the socket poll, the timer, later the rescan reads).
const RING_ENTRIES: u32 = 32;

/// Event-loop timer period. Paces key repeat and the caret blink; the socket
/// poll wakes the loop independently the moment events arrive.
const TICK_NANOS: i64 = 33_000_000;

/// io_uring user_data tags identifying which submission a completion belongs to.
const K_WAYLAND: u64 = 1;
const K_TIMER: u64 = 2;

/// What the current input text resolves to when launched.
pub(crate) enum InputAction<'a> {
    /// s:<query> runs a web search for the trimmed query via the configured
    /// search URL. Only produced when a search URL is set.
    Search(&'a str),
    /// u:<url> opens the trimmed URL (https:// prepended if no scheme).
    OpenUrl(&'a str),
    /// Anything else searches desktop entries by the raw input.
    AppSearch(&'a str),
}

pub(crate) fn parse_action(input: &str, search_enabled: bool) -> InputAction<'_> {
    if search_enabled {
        if let Some(rest) = input.strip_prefix("s:") {
            return InputAction::Search(rest.trim());
        }
    }
    if let Some(rest) = input.strip_prefix("u:") {
        InputAction::OpenUrl(rest.trim())
    } else {
        InputAction::AppSearch(input)
    }
}

/// Byte capacity of a URL the launcher handles.
pub(crate) const URL_CAP: usize = 2048;

/// Prepend https:// if the URL has no scheme, into out.
pub(crate) fn normalize_url(url: &str, out: &mut arena::ArrayString<URL_CAP>) {
    if !url.contains("://") {
        let _ = out.push_str("https://");
    }
    let _ = out.push_str(url);
}

/// Try to acquire an exclusive lock. Returns the owned lock fd on success; the
/// lock is held until the fd is dropped (and closed).
fn try_acquire_lock() -> Result<syscall::Fd> {
    let runtime_dir = env::var("XDG_RUNTIME_DIR").unwrap_or("/tmp");
    let mut path: arena::ArrayString<{ fs::PATH_CAP }> = arena::ArrayString::new();
    path.push_str(runtime_dir)
        .and_then(|_| path.push_str("/bnklaunch.lock"))
        .map_err(|_| Error::msg("lock path too long"))?;
    let cp = fs::cpath(path.as_str())?;

    // O_CLOEXEC so a launched app does not inherit the lock fd and hold the
    // single-instance flock for its whole lifetime.
    let fd = syscall::openat(
        syscall::AT_FDCWD,
        &cp,
        syscall::O_RDWR | syscall::O_CREAT | syscall::O_CLOEXEC,
        0o644,
    );
    if fd < 0 {
        return Err(Error::from_errno(-fd));
    }
    let fd = syscall::Fd::new(fd);

    let result = syscall::flock(fd.as_raw_fd(), syscall::LOCK_EX | syscall::LOCK_NB);
    if result == 0 {
        Ok(fd)
    } else {
        Err(Error::msg("another instance is already running"))
    }
}

/// Application state
pub(crate) struct AppState {
    entries: desktop::Catalog,
    /// Recent launches in MRU order. Shown when the input is empty.
    recents: cache::Recents,
    pub(crate) selected: usize,
}

/// Record a successful launch as the most recent, overwriting the recents tail
/// of the cache in place. A no-op when nothing launched or the cache offset is
/// unknown (no cache dir).
fn record_launch(
    state: &mut AppState,
    recents_offset: Option<u64>,
    launched: Option<arena::ArrayString<{ desktop::NAME_CAP }>>,
) {
    if let (Some(name), Some(offset)) = (launched, recents_offset) {
        let _ = cache::record(offset, &mut state.recents, name.as_str());
    }
}

/// The launcher's entry logic. Wrapped by the C entry point in main.rs in the
/// real build; called directly by tests through the std harness.
pub(crate) fn run() -> Result<()> {
    // Single instance check - exit silently if another instance is running
    let _lock = match try_acquire_lock() {
        Ok(lock) => lock,
        Err(_) => return Ok(()),
    };

    // Load system font
    let font = font::Font::load()?;

    // User configuration: hidden app names plus the optional search URL. The
    // s: search prefix works only when a search URL is set; without one the
    // prefix is left as literal text.
    let cfg = config::load();
    let search_enabled = cfg.search_url.is_some();

    // Load the cache: entries to show, the recents for an empty query, and the
    // offset record overwrites in place.
    let cached = cache::load();
    let cache_fingerprint = cached.as_ref().map(|c| c.fingerprint);
    let mut entries: desktop::Catalog;
    let recents;
    let mut recents_offset;
    match cached {
        Some(c) => {
            entries = c.entries;
            recents = c.recents;
            recents_offset = Some(c.recents_offset);
        }
        None => {
            entries = desktop::Catalog::new();
            desktop::discover_entries(&mut entries);
            recents = cache::Recents::new();
            recents_offset = None;
        }
    }

    // Refresh against the application directories. When their fingerprint matches
    // the cache this is skipped; otherwise rediscover and persist the fresh set.
    // The cache keeps the unfiltered set (so toggling the denylist needs no
    // rescan), and the denylist is applied afterward to what gets shown.
    let fingerprint = desktop::dirs_fingerprint();
    if Some(fingerprint) != cache_fingerprint {
        desktop::discover_entries(&mut entries);
        if let Ok(offset) = cache::save(&entries, fingerprint, &recents) {
            recents_offset = Some(offset);
        }
    }
    denylist::apply(&mut entries, &cfg.denied);

    let mut state = AppState {
        entries,
        recents,
        selected: 0,
    };

    // Connect to compositor
    let mut client = Client::connect()?;

    // Initialize and get registry
    client.init()?;
    client.roundtrip()?;

    // Require layer shell for overlay behavior
    if !client.has_layer_shell() {
        return Err(Error::msg(
            "compositor does not support zwlr_layer_shell_v1",
        ));
    }

    // Bind to required globals
    client.bind_globals()?;
    client.roundtrip()?;

    // Create layer surface sized for the initial empty-input view (recents).
    let initial_total = cache::resolve(&state.recents, &state.entries)
        .len()
        .min(MAX_RESULTS);
    client.create_layer_surface(WINDOW_WIDTH, calculate_height(initial_total))?;

    // Wait for the configure event. The compositor may send closed instead
    // (no output available, or an output destroyed mid-resize), which sets
    // running false; bail out then rather than blocking on a configure that
    // will never come.
    while client.running
        && !client
            .surface
            .as_ref()
            .map(|s| s.configured)
            .unwrap_or(false)
    {
        client.dispatch()?;
    }
    if !client.running {
        return Ok(());
    }

    // Create buffer
    client.create_buffer()?;

    // Wait for input devices
    client.roundtrip()?;

    // Caret blink state. The caret is solid for one interval then toggles; any
    // input activity resets it to solid in the event loop below.
    let mut cursor_visible = true;
    let mut last_blink = Instant::now();

    // Initial draw of the empty-input view (recents). The result list borrows
    // state, so scope it to this block: it must not hold that borrow across the
    // loop, where state is mutated.
    let mut current_results_len = {
        let initial_results = cache::resolve(&state.recents, &state.entries);
        let len = initial_results.len().min(MAX_RESULTS);
        draw_ui(
            &mut client,
            "",
            &state,
            &initial_results,
            &font,
            search_enabled,
            cursor_visible,
        );
        client.render()?;
        len
    };

    // Non-blocking socket: a poll readiness wakeup then drains every queued
    // message in one pass without blocking on the final partial read.
    client.socket.set_nonblocking(true)?;

    // Drive the loop with io_uring. A one-shot poll on the Wayland socket wakes
    // the loop the instant the compositor sends events; a one-shot timer paces
    // key repeat and the caret blink. Each is re-armed after it fires. Between
    // wakeups the thread is parked in io_uring_enter rather than sleep-polling.
    let wl_fd = client.socket.as_raw_fd();
    let mut ring = uring::Ring::new(RING_ENTRIES).map_err(Error::from_errno)?;
    let tick = syscall::kernel_timespec {
        tv_sec: 0,
        tv_nsec: TICK_NANOS,
    };
    let _ = ring.prep_poll_add(wl_fd, syscall::POLLIN as u32, K_WAYLAND);
    let _ = ring.prep_timeout(&tick, K_TIMER);

    // Event loop
    while client.running {
        // Park until the socket is readable or the timer fires.
        if ring.submit_and_wait(1).is_err() {
            break;
        }
        let mut timer_fired = false;
        while let Some(cqe) = ring.next_cqe() {
            match cqe.user_data {
                K_WAYLAND => {
                    // Re-arm the one-shot poll for the next readiness.
                    let _ = ring.prep_poll_add(wl_fd, syscall::POLLIN as u32, K_WAYLAND);
                    // Readiness means at least one message; drain them all.
                    loop {
                        match client.dispatch() {
                            Ok(()) => {}
                            Err(ref e) if e.would_block() => break,
                            Err(e) => return Err(e),
                        }
                    }
                }
                K_TIMER => {
                    // Re-arm the one-shot timer for the next tick.
                    let _ = ring.prep_timeout(&tick, K_TIMER);
                    timer_fired = true;
                }
                _ => {}
            }
        }
        if timer_fired {
            client.process_key_repeat();
        }

        // Handle pending actions. Up and Down wrap around the result list. A
        // move only repaints the highlight; it must not touch the caret blink.
        let mut selection_moved = false;
        match client.pending_action {
            PendingAction::SelectUp => {
                client.pending_action = PendingAction::None;
                if current_results_len > 0 {
                    state.selected = if state.selected == 0 {
                        current_results_len - 1
                    } else {
                        state.selected - 1
                    };
                    selection_moved = true;
                }
            }
            PendingAction::SelectDown => {
                client.pending_action = PendingAction::None;
                if current_results_len > 0 {
                    state.selected = (state.selected + 1) % current_results_len;
                    selection_moved = true;
                }
            }
            _ => {}
        }

        // Handle clipboard operations
        if !matches!(client.clipboard_op, ClipboardOp::None) {
            client.process_clipboard();
        }

        // Handle pointer click/drag in the input box
        handle_pointer_input(&mut client, &font);

        // Handle input changes
        if client.input_changed {
            client.input_changed = false;
            // Activity keeps the caret solid; restart the blink from now.
            cursor_visible = true;
            last_blink = Instant::now();

            let text = client.input_text;
            let (results, total_rows) =
                resolve_results(&text, search_enabled, &state.entries, &state.recents);
            current_results_len = total_rows;

            // Reset selection if it's out of bounds
            if state.selected >= total_rows && total_rows > 0 {
                state.selected = total_rows - 1;
            }
            if total_rows == 0 {
                state.selected = 0;
            }

            // Resize window based on row count
            let new_height = calculate_height(total_rows);
            client.resize_surface(WINDOW_WIDTH, new_height)?;

            // Redraw
            draw_ui(
                &mut client,
                &text,
                &state,
                &results,
                &font,
                search_enabled,
                cursor_visible,
            );
            client.render()?;
        } else if selection_moved {
            // The result-list selection moved. Repaint the highlight at the
            // current caret phase, leaving the blink timer and window size as
            // they are so the caret keeps blinking through navigation.
            let text = client.input_text;
            let (results, _) =
                resolve_results(&text, search_enabled, &state.entries, &state.recents);
            draw_ui(
                &mut client,
                &text,
                &state,
                &results,
                &font,
                search_enabled,
                cursor_visible,
            );
            client.render()?;
        } else if last_blink.elapsed_ms() >= CURSOR_BLINK_MS {
            // Idle: flip the caret and repaint. The row set is unchanged, so
            // this recomputes the same results purely to composite the caret.
            cursor_visible = !cursor_visible;
            last_blink = Instant::now();

            let text = client.input_text;
            let (results, _) =
                resolve_results(&text, search_enabled, &state.entries, &state.recents);
            draw_ui(
                &mut client,
                &text,
                &state,
                &results,
                &font,
                search_enabled,
                cursor_visible,
            );
            client.render()?;
        }
    }

    // Check if we should launch something
    if client.pending_action == PendingAction::Launch {
        match parse_action(&client.input_text, search_enabled) {
            InputAction::Search(q) if !q.is_empty() => {
                if let Some(url) = &cfg.search_url {
                    if let Err(e) = launch::launch_search(url, q) {
                        elog!("bnklaunch: web search failed: {e}");
                    }
                }
            }
            InputAction::OpenUrl(u) if !u.is_empty() => {
                let mut url: arena::ArrayString<URL_CAP> = arena::ArrayString::new();
                normalize_url(u, &mut url);
                if let Err(e) = launch::launch_url(&url) {
                    elog!("bnklaunch: failed to open URL: {e}");
                }
            }
            InputAction::AppSearch(text) if !text.is_empty() => {
                // Scope the borrowed result list so it drops before the &mut
                // state in record_launch.
                let launched = {
                    let results = desktop::search(&state.entries, text);
                    match results.get(state.selected) {
                        Some(entry) if launch::launch(entry).is_ok() => Some(entry.name),
                        _ => None,
                    }
                };
                record_launch(&mut state, recents_offset, launched);
            }
            InputAction::AppSearch(_) => {
                let launched = {
                    let results = cache::resolve(&state.recents, &state.entries);
                    match results.get(state.selected) {
                        Some(entry) if launch::launch(entry).is_ok() => Some(entry.name),
                        _ => None,
                    }
                };
                record_launch(&mut state, recents_offset, launched);
            }
            _ => {}
        }
    }

    Ok(())
}

/// Process pointer events and translate to cursor/selection changes.
fn handle_pointer_input(client: &mut Client, font: &font::Font) {
    let px = client.pointer.x;
    let py = client.pointer.y;

    // Check if pointer is in the input box region
    let in_input_box = py >= ui::INPUT_START_Y as f64
        && py < (ui::INPUT_START_Y + ui::INPUT_BOX_H) as f64
        && px >= ui::TEXT_X as f64
        && px < (WINDOW_WIDTH - ui::MARGIN_X) as f64;

    if client.pointer.clicked && in_input_box {
        let text_offset_px = (px - ui::TEXT_X as f64).max(0.0) as f32;
        let char_offset =
            font.char_offset_at_x(&client.input_text, text_offset_px, font::INPUT_SIZE);
        client.handle_pointer_click(char_offset);
    } else if client.pointer.clicked {
        // Click outside input box: clear click flag
        client.pointer.clicked = false;
    }

    if client.pointer.dragging && in_input_box {
        let text_offset_px = (px - ui::TEXT_X as f64).max(0.0) as f32;
        let char_offset =
            font.char_offset_at_x(&client.input_text, text_offset_px, font::INPUT_SIZE);
        client.handle_pointer_drag(char_offset);
    } else if client.pointer.dragging {
        client.pointer.dragging = false;
    }
}

/// Resolve the current input to the rows to display plus the total row count.
/// The count can exceed the rendered rows: draw_ui only paints MAX_RESULTS, so
/// capping it keeps selection (and Enter to launch) off a row the user can't see.
/// Borrows entries (not the whole state) so the caller can still touch selection.
fn resolve_results<'a>(
    text: &str,
    search_enabled: bool,
    entries: &'a [DesktopEntry],
    recents: &[arena::ArrayString<{ desktop::NAME_CAP }>],
) -> (
    arena::ArrayVec<&'a DesktopEntry, { desktop::RESULT_CAP }>,
    usize,
) {
    match parse_action(text, search_enabled) {
        InputAction::AppSearch(q) if !q.is_empty() => {
            let r = desktop::search(entries, q);
            let len = r.len().min(MAX_RESULTS);
            (r, len)
        }
        InputAction::AppSearch(_) => {
            let r = cache::resolve(recents, entries);
            let len = r.len().min(MAX_RESULTS);
            (r, len)
        }
        InputAction::Search(q) | InputAction::OpenUrl(q) => {
            (arena::ArrayVec::new(), if q.is_empty() { 0 } else { 1 })
        }
    }
}
