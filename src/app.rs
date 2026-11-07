//! The launcher application: desktop-entry resolution, the io_uring event
//! loop, and input handling.

use crate::client::{Client, PendingAction};
use crate::clipboard;
use crate::desktop::{self, DesktopEntry};
use crate::instance::{self, Claim, Instance};
use crate::platform::error::elog;
use crate::platform::error::{Error, Result};
use crate::platform::time::Instant;
use crate::platform::xkb::{keycode, Xkb};
use crate::platform::{arena, env, syscall, uring};
use crate::ui::{calculate_height, draw_ui, Theme, MAX_RESULTS, WINDOW_WIDTH};
use crate::{cache, config, denylist, font, launch, ui};

/// Caret blink half-period in milliseconds: solid this long, then hidden this
/// long.
const CURSOR_BLINK_MS: u64 = 530;

/// Submission-queue depth for the event-loop ring. A turn queues at most four
/// polls, each one-shot: the socket for reading and, while requests are backed
/// up, for writing, and a paste's pipe, each re-armed as it fires; and the
/// listener a second instance connects to, armed once.
const RING_ENTRIES: u32 = 32;

/// Most messages one loop turn handles. A burst bigger than this is finished
/// over the next turns, with deadlines and painting in between, so a flood of
/// events cannot hold either off. Real input is a few messages a turn.
const DISPATCH_BUDGET: usize = 64;

/// io_uring user_data of the socket's read poll.
const K_WAYLAND: u64 = 1;
/// io_uring user_data of the socket's write poll.
const K_OUTPUT: u64 = 2;
/// io_uring user_data of the poll on the listener a second instance connects to.
const K_CLOSE: u64 = 3;
/// io_uring user_data of a paste pipe's poll: this tag, ored with the paste's
/// serial, so a poll left over from a finished paste is told apart from the
/// current one's.
const K_PASTE: u64 = 1 << 32;

/// What a key press means to the launcher.
///
/// The platform layer answers only machine questions (which character does this
/// key type under these modifiers), so the meaning is assigned here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyAction {
    Char(char),
    Backspace,
    Delete,
    Enter,
    Escape,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    SelectAll,
    Copy,
    Cut,
    Paste,
    None,
}

/// Read a key press as the action it stands for.
///
/// The editing keys and the Ctrl shortcuts dispatch on the physical evdev
/// keycode, not on the character the key would type. That is the convention
/// every other Linux app follows: Ctrl+V is the V-position key, whatever the
/// user's layout makes that key type. Only ordinary text input goes through the
/// layout, by asking xkb what the key produces.
pub(crate) fn key_action(xkb: &Xkb, keycode: u32) -> KeyAction {
    use keycode::*;

    match keycode {
        // A modifier on its own is not an action.
        KEY_LEFTSHIFT | KEY_RIGHTSHIFT | KEY_LEFTCTRL | KEY_RIGHTCTRL | KEY_LEFTALT
        | KEY_RIGHTALT | KEY_CAPSLOCK => return KeyAction::None,
        KEY_ENTER | KEY_KPENTER => return KeyAction::Enter,
        KEY_ESC => return KeyAction::Escape,
        KEY_TAB => return KeyAction::Tab,
        KEY_BACKSPACE => return KeyAction::Backspace,
        KEY_DELETE => return KeyAction::Delete,
        KEY_UP => return KeyAction::Up,
        KEY_DOWN => return KeyAction::Down,
        KEY_LEFT => return KeyAction::Left,
        KEY_RIGHT => return KeyAction::Right,
        KEY_HOME => return KeyAction::Home,
        KEY_END => return KeyAction::End,
        _ => {}
    }

    if xkb.ctrl_active() {
        return match keycode {
            KEY_A => KeyAction::SelectAll,
            KEY_C => KeyAction::Copy,
            KEY_X => KeyAction::Cut,
            KEY_V => KeyAction::Paste,
            _ => KeyAction::None,
        };
    }

    match xkb.key_char(keycode) {
        Some(c) => KeyAction::Char(c),
        None => KeyAction::None,
    }
}

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

use crate::launch::URL_CAP;

/// Prepend https:// if the URL has no scheme, into out. mailto: carries no
/// slashes, so a scheme the launcher opens counts as one too.
pub(crate) fn normalize_url(url: &str, out: &mut arena::ArrayString<URL_CAP>) {
    if !url.contains("://") && !launch::has_allowed_scheme(url) {
        let _ = out.push_str("https://");
    }
    let _ = out.push_str(url);
}

/// Application state
pub(crate) struct AppState {
    entries: desktop::Catalog,
    /// Recent launches in MRU order. Shown when the input is empty.
    recents: cache::Recents,
    pub(crate) selected: usize,
}

impl AppState {
    pub(crate) fn new(entries: desktop::Catalog, recents: cache::Recents) -> Self {
        Self {
            entries,
            recents,
            selected: 0,
        }
    }
}

/// The launcher's entry logic. Wrapped by the C entry point in main.rs in the
/// real build; called directly by tests through the std harness.
pub(crate) fn run() -> Result<()> {
    // Writing to a pipe or socket whose peer has hung up raises SIGPIPE, and its
    // default action kills the process before any error path here runs. The
    // compositor socket takes MSG_NOSIGNAL on every send, but the clipboard
    // pipe is a pipe: an app that asks for the selection and then goes away
    // mid-transfer would take the launcher down with it. Ignoring the signal
    // turns both into an EPIPE the code already handles.
    //
    // The disposition survives exec, so launch.rs puts it back to default in the
    // forked child; a launched application must keep the SIGPIPE its own
    // pipelines rely on.
    syscall::signal_disposition(syscall::SIGPIPE, syscall::SIG_IGN);

    // One launcher at a time: while one is open, a second hotkey press closes
    // it. A lock that cannot be taken at all is reported and the launcher runs
    // without it, since refusing to start would leave a hotkey that silently
    // does nothing.
    let instance = match instance::claim() {
        Ok(Claim::First(instance)) => Some(instance),
        Ok(Claim::Second) => return Ok(()),
        Err(e) => {
            elog!("bnklaunch: running without the single-instance lock: {e}");
            None
        }
    };

    // Load system font
    let font = font::Font::load()?;

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
            // The catalog stays empty here. A missing cache has no fingerprint
            // to match, so the refresh below always discovers; scanning here as
            // well would walk every application directory twice on a cold start.
            entries = desktop::Catalog::new();
            recents = cache::Recents::new();
            recents_offset = None;
        }
    }

    // Refresh against the application directories. When their fingerprint matches
    // the cache this is skipped; otherwise rediscover and persist the fresh set.
    // The cache keeps the unfiltered set, so toggling the denylist or installing
    // a terminal runner needs no rescan; both filters apply afterward to what
    // gets shown.
    let fingerprint = desktop::dirs_fingerprint();
    if Some(fingerprint) != cache_fingerprint {
        desktop::discover_entries(&mut entries);
        if let Ok(offset) = cache::save(&entries, fingerprint, &recents) {
            recents_offset = Some(offset);
        }
    }
    denylist::apply(&mut entries, &cfg.denied);
    launch::drop_unlaunchable(&mut entries, env::in_flatpak(), env::var("PATH"));

    let mut state = AppState::new(entries, recents);

    let mut client = Client::connect()?;
    let close_requests = instance.as_ref().and_then(Instance::close_requests);
    run_window(
        &mut client,
        &mut state,
        &font,
        search_enabled,
        &cfg.theme,
        close_requests,
    )?;

    if client.pending_action == PendingAction::Launch {
        dispatch_launch(&client, &mut state, &cfg, recents_offset, search_enabled);
    }
    Ok(())
}

/// Bring the launcher window up on a connected client and run it until
/// something dismisses it. close_requests is the listener a second instance
/// connects to, when there is one.
pub(crate) fn run_window(
    client: &mut Client,
    state: &mut AppState,
    font: &font::Font,
    search_enabled: bool,
    theme: &Theme,
    close_requests: Option<&syscall::Fd>,
) -> Result<()> {
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
    let initial_rows = cache::resolve(&state.recents, &state.entries)
        .len()
        .min(MAX_RESULTS);
    client.create_layer_surface(WINDOW_WIDTH, calculate_height(initial_rows, theme.rows))?;

    // Wait for the configure event. The compositor may send closed instead
    // (no output available, or an output destroyed mid-resize), which sets
    // running false; bail out then rather than blocking on a configure that
    // will never come.
    while client.running && !client.surface_configured() {
        client.dispatch()?;
    }
    if !client.running {
        return Ok(());
    }

    // Create buffer
    client.create_buffer()?;

    // Wait for input devices
    client.roundtrip()?;

    event_loop(client, state, font, search_enabled, theme, close_requests)
}

/// How long the loop may park before something is due: the held key's next
/// repeat or the caret's next blink, whichever comes first. Parking for exactly
/// that long is what lands each repeat on its interval, and nothing else wakes
/// an idle launcher.
fn park_ms(repeat_due_ms: Option<u32>, since_blink_ms: u64) -> u64 {
    let blink = CURSOR_BLINK_MS.saturating_sub(since_blink_ms);
    repeat_due_ms.map_or(blink, |ms| blink.min(u64::from(ms)))
}

/// Run until something dismisses the launcher: Enter, Escape, focus loss, the
/// hotkey pressed again, or the compositor closing the surface.
fn event_loop(
    client: &mut Client,
    state: &mut AppState,
    font: &font::Font,
    search_enabled: bool,
    theme: &Theme,
    close_requests: Option<&syscall::Fd>,
) -> Result<()> {
    // Caret blink state. The caret is solid for one interval then toggles; any
    // input activity resets it to solid below.
    let mut cursor_visible = true;
    let mut last_blink = Instant::now();

    // The query the current result list was built from, so an edit to the text
    // can be told apart from a cursor move or a click, which leave it standing.
    let mut last_query = client.editor.text_owned();

    // The rows on screen, rebuilt only when the input changes. A selection move
    // and a caret blink redraw the list that is already in hand, so neither
    // searches the catalog again.
    let (mut results, mut total_rows) =
        resolve_results(&last_query, search_enabled, &state.entries, &state.recents);

    // Whatever changes the picture marks it here, and the next turn paints it
    // once, as soon as the compositor is ready for a frame. Until then the loop
    // keeps handling input; the frame that goes out shows the latest state.
    let mut needs_paint = true;

    // Non-blocking socket: a readiness wakeup reads what has arrived without
    // blocking on the final partial read, and a flush sends what fits.
    client.socket.set_nonblocking(true)?;

    // Drive the loop with io_uring. A one-shot poll on the Wayland socket wakes
    // the loop the instant the compositor sends events, and the wait's own
    // timeout wakes it when the next key repeat or caret blink is due. The poll
    // is re-armed after it fires. Between wakeups the thread is parked in
    // io_uring_enter rather than sleep-polling.
    let wl_fd = client.socket.fd();
    let mut ring = uring::Ring::new(RING_ENTRIES).map_err(Error::from_errno)?;
    if !ring.supports_wait_timeout() {
        return Err(Error::msg(
            "io_uring cannot bound a wait with a timeout here (needs Linux 5.11)",
        ));
    }
    // The poll is one-shot and re-armed as it completes. A failed re-arm is
    // fatal: the socket would never wake the loop again.
    let arm_failed = || Error::msg("io_uring submission queue full");
    ring.prep_poll_add(wl_fd, syscall::POLLIN as u32, K_WAYLAND)
        .map_err(|_| arm_failed())?;
    if let Some(listener) = close_requests {
        ring.prep_poll_add(listener.as_raw_fd(), syscall::POLLIN as u32, K_CLOSE)
            .map_err(|_| arm_failed())?;
    }
    // The serial of the paste whose pipe has a poll in flight.
    let mut paste_armed: Option<u64> = None;
    let mut output_armed = false;
    // A burst ran past the dispatch budget, and the rest is waiting.
    let mut input_pending = false;

    // Event loop. Enter or a click on a row ends it, except that a launch waits
    // for a paste still arriving, until it lands or runs out of time, so the
    // launch sees the pasted text the way it was typed. Escape and a second
    // press of the hotkey end that wait too, without launching.
    while client.running || launch_awaits_paste(client) {
        // Paint what the last turn changed, if the compositor is ready for a
        // frame, then send what is queued, as far as the socket takes it. The
        // first turn paints the opening frame this way, before any wait.
        if needs_paint
            && repaint(
                client,
                state,
                &results,
                font,
                search_enabled,
                theme,
                cursor_visible,
            )?
        {
            needs_paint = false;
        }
        client.socket.flush()?;

        // Requests the socket could not take wait for it to have room. The loop
        // goes on handling input meanwhile; nothing waits on the compositor.
        if client.socket.has_pending_output() && !output_armed {
            ring.prep_poll_add(wl_fd, syscall::POLLOUT as u32, K_OUTPUT)
                .map_err(|_| arm_failed())?;
            output_armed = true;
        }

        // Watch a pending paste's pipe. A poll is armed once per readiness, and
        // its completion clears the mark so the next turn arms it again.
        if let Some(paste) = &client.paste {
            if paste_armed != Some(client.paste_serial) {
                ring.prep_poll_add(
                    paste.fd(),
                    syscall::POLLIN as u32,
                    K_PASTE | client.paste_serial,
                )
                .map_err(|_| arm_failed())?;
                paste_armed = Some(client.paste_serial);
            }
        }

        // Park until the socket or a paste's pipe is readable, or something is
        // due: a key repeat, the caret blink, or a paste's deadline.
        let mut park = park_ms(client.key_repeat.time_until_due(), last_blink.elapsed_ms());
        if let Some(paste) = &client.paste {
            park = park.min(paste.remaining_ms());
        }
        // A frame held for a resize's configure is due when the grace runs out.
        if needs_paint {
            if let Some(ms) = client.configure_wait_ms() {
                park = park.min(ms);
            }
        }
        // Messages left over from a burst are handled first, without parking.
        if input_pending {
            park = 0;
        }
        ring.submit_and_wait_timeout(park * 1_000_000)
            .map_err(Error::from_errno)?;
        let mut readable = input_pending;
        let mut close_requested = false;
        while let Some(cqe) = ring.next_cqe() {
            // A poll completes with the events that are ready, or a negated
            // errno when the poll itself failed.
            match cqe.user_data {
                K_WAYLAND => {
                    if cqe.res < 0 {
                        return Err(Error::from_errno(-cqe.res));
                    }
                    ring.prep_poll_add(wl_fd, syscall::POLLIN as u32, K_WAYLAND)
                        .map_err(|_| arm_failed())?;
                    readable = true;
                }
                K_OUTPUT => {
                    output_armed = false;
                    if cqe.res < 0 {
                        return Err(Error::from_errno(-cqe.res));
                    }
                    client.socket.flush()?;
                }
                // A second instance connected: the hotkey was pressed again. A
                // failed poll leaves the launcher open, as without a listener.
                K_CLOSE => close_requested = cqe.res > 0,
                // The paste's pipe is read below, every turn; its poll only
                // wakes the loop.
                tag if tag == K_PASTE | client.paste_serial => paste_armed = None,
                // An earlier paste's poll, completing after its paste ended.
                _ => {}
            }
        }
        if readable {
            input_pending = dispatch_some(client)?;
        }
        client.process_key_repeat();
        client.advance_paste();

        // Handle pending actions. Up and Down wrap around the result list. A
        // move only repaints the highlight; it must not touch the caret blink.
        let mut selection_moved = false;
        match client.pending_action {
            PendingAction::SelectUp => {
                client.pending_action = PendingAction::None;
                if total_rows > 0 {
                    state.selected = if state.selected == 0 {
                        total_rows - 1
                    } else {
                        state.selected - 1
                    };
                    selection_moved = true;
                }
            }
            PendingAction::SelectDown => {
                client.pending_action = PendingAction::None;
                if total_rows > 0 {
                    state.selected = (state.selected + 1) % total_rows;
                    selection_moved = true;
                }
            }
            _ => {}
        }

        // Handle clipboard operations
        if !matches!(client.clipboard_op, clipboard::Op::None) {
            client.process_clipboard();
        }

        // Handle pointer clicks on a result row, and click/drag in the input box
        handle_pointer_input(client, &mut state.selected, font, total_rows, theme.rows);

        // The second press closes the launcher after this turn's input is
        // handled, so an Enter or a click in the same turn cannot undo it.
        if close_requested {
            client.dismiss();
        }

        // Three things change the picture: the input, the selection, and the
        // caret blink. They differ only in what they settle first.
        if client.input_changed {
            client.input_changed = false;
            // Activity keeps the caret solid; restart the blink from now.
            cursor_visible = true;
            last_blink = Instant::now();

            let text = client.editor.text_owned();
            let query_changed = text != last_query;
            last_query = text;
            (results, total_rows) =
                resolve_results(&text, search_enabled, &state.entries, &state.recents);

            // A new query is a new list. An index carried over from the previous
            // one points at a row the user never scanned, and Enter would launch
            // it. The bounds check covers the rest (a cursor move or a click
            // leaves the query, and so the selection, alone).
            if query_changed || state.selected >= total_rows {
                state.selected = 0;
            }

            // The window grows and shrinks with the row count.
            client.request_size(WINDOW_WIDTH, calculate_height(total_rows, theme.rows))?;
            needs_paint = true;
        } else if selection_moved {
            // The highlight moved. The caret keeps its phase and the window its
            // size, so navigation does not disturb the blink.
            needs_paint = true;
        } else if last_blink.elapsed_ms() >= CURSOR_BLINK_MS {
            cursor_visible = !cursor_visible;
            last_blink = Instant::now();
            needs_paint = true;
        }
        // A configure that changed the size wants the picture at the new one.
        if client.present.frames_stale() {
            needs_paint = true;
        }
    }
    Ok(())
}

/// Whether the loop has ended on a launch while a paste is still arriving.
fn launch_awaits_paste(client: &Client) -> bool {
    client.pending_action == PendingAction::Launch && client.paste.is_some()
}

/// Handle the messages that have arrived, up to DISPATCH_BUDGET of them. True
/// when the budget ran out with more waiting.
fn dispatch_some(client: &mut Client) -> Result<bool> {
    for _ in 0..DISPATCH_BUDGET {
        match client.dispatch() {
            Ok(()) => {}
            Err(ref e) if e.would_block() => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Launch whatever the input resolved to, and record an application launch as
/// the most recent. Reached only when the loop ended on Enter.
fn dispatch_launch(
    client: &Client,
    state: &mut AppState,
    cfg: &config::Config,
    recents_offset: Option<u64>,
    search_enabled: bool,
) {
    let text = client.editor.text();
    match parse_action(text, search_enabled) {
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
        // A query and an empty input both land on a row of the same list; which
        // list is resolve_results' business, not this one's.
        InputAction::AppSearch(_) => {
            // Scope the borrowed row so it drops before the &mut state below.
            let launched = {
                let (results, _) =
                    resolve_results(text, search_enabled, &state.entries, &state.recents);
                match results.get(state.selected) {
                    Some(entry) if launch::launch(entry).is_ok() => Some(entry.name),
                    _ => None,
                }
            };
            if let (Some(name), Some(offset)) = (launched, recents_offset) {
                let _ = cache::record(offset, &mut state.recents, name.as_str());
            }
        }
        _ => {}
    }
}

/// Draw the given rows into the next frame and show it, when one can go.
/// Returns whether it did.
fn repaint(
    client: &mut Client,
    state: &AppState,
    results: &[&DesktopEntry],
    font: &font::Font,
    search_enabled: bool,
    theme: &Theme,
    cursor_visible: bool,
) -> Result<bool> {
    let text = client.editor.text_owned();
    let caret = ui::Caret {
        offset: client.editor.cursor(),
        selection: client.editor.selection(),
        visible: cursor_visible,
    };
    client.paint(|client| {
        let Some(pixels) = client.pixels() else {
            return;
        };
        draw_ui(
            pixels,
            &text,
            state,
            results,
            font,
            search_enabled,
            &caret,
            theme,
        );
    })
}

/// Process pointer events: a click on a result row launches it, and a click or
/// drag in the input box moves the caret or extends the selection.
///
/// Takes the selection alone rather than the whole state, which the caller is
/// still holding a borrow of: the rows on screen point into state.entries.
fn handle_pointer_input(
    client: &mut Client,
    selected: &mut usize,
    font: &font::Font,
    total_rows: usize,
    rows: ui::Rows,
) {
    let px = client.pointer.x;
    let py = client.pointer.y;

    // A click on a result row selects it and ends the loop on a launch, which
    // is where Enter arrives too. Clearing the redraw flag on the way out keeps
    // the last pass from resizing a window that is about to go away.
    if client.pointer.clicked {
        if let Some(row) = ui::row_at(px, py, total_rows, rows) {
            *selected = row;
            client.pointer.clicked = false;
            client.input_changed = false;
            client.pending_action = PendingAction::Launch;
            client.running = false;
            return;
        }
    }

    let in_input_box = ui::input_box_contains(px, py);

    if client.pointer.clicked && in_input_box {
        let text_offset_px = (px - ui::TEXT_X as f64).max(0.0) as f32;
        let char_offset =
            font.char_offset_at_x(client.editor.text(), text_offset_px, font::INPUT_SIZE);
        client.handle_pointer_click(char_offset);
    } else if client.pointer.clicked {
        // Click outside input box: clear click flag
        client.pointer.clicked = false;
    }

    if client.pointer.dragging && in_input_box {
        let text_offset_px = (px - ui::TEXT_X as f64).max(0.0) as f32;
        let char_offset =
            font.char_offset_at_x(client.editor.text(), text_offset_px, font::INPUT_SIZE);
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

#[cfg(test)]
mod tests {
    use crate::app::*;
    use crate::client::KeyRepeat;

    fn entries(names: &[(&str, &str)]) -> desktop::Catalog {
        let mut out = desktop::Catalog::new();
        for (name, exec) in names {
            let _ = out.push(DesktopEntry::new(name, exec).expect("entry"));
        }
        out
    }

    fn norm(url: &str) -> String {
        let mut out: arena::ArrayString<URL_CAP> = arena::ArrayString::new();
        normalize_url(url, &mut out);
        out.as_str().to_string()
    }

    /// Step the event loop: park for what park_ms asks, then see whether the
    /// key is due. The caret's blink is held at zero elapsed, as typing keeps
    /// resetting it. Returns the gap between each repeat and the one before it,
    /// in milliseconds.
    fn repeat_gaps(repeat: &KeyRepeat, rounds: usize) -> Vec<u32> {
        let mut since_last = 0;
        let mut gaps = Vec::new();
        for _ in 0..rounds {
            let waited = park_ms(repeat.due_in(0, Some(since_last)), 0) as u32;
            since_last += waited;
            if repeat.due_in(0, Some(since_last)) == Some(0) {
                gaps.push(since_last);
                since_last = 0;
            }
        }
        gaps
    }

    #[test]
    fn a_repeat_lands_on_its_interval() {
        let repeat = KeyRepeat {
            held_key: Some(1),
            repeating: true,
            ..Default::default()
        };
        assert_eq!(1000 / repeat.rate, 40);
        let gaps = repeat_gaps(&repeat, 60);
        assert!(gaps.len() >= 5, "not enough repeats to judge: {gaps:?}");
        assert!(
            gaps.iter().all(|&g| g == 40),
            "every gap should be the interval: {gaps:?}"
        );

        let slower = KeyRepeat { rate: 20, ..repeat };
        assert!(repeat_gaps(&slower, 60).iter().all(|&g| g == 50));
    }

    #[test]
    fn an_idle_loop_parks_until_the_caret_blinks() {
        let idle = KeyRepeat::default();
        assert_eq!(park_ms(idle.time_until_due(), 0), CURSOR_BLINK_MS);
        assert_eq!(park_ms(idle.time_until_due(), 500), 30);
        // A blink already due parks for nothing; handling it moves the deadline.
        assert_eq!(park_ms(idle.time_until_due(), 900), 0);

        // A held key due sooner than the blink wins.
        let held = KeyRepeat {
            held_key: Some(1),
            repeating: true,
            ..Default::default()
        };
        assert_eq!(park_ms(held.due_in(0, Some(0)), 0), 40);
        assert_eq!(park_ms(held.due_in(0, Some(0)), 510), 20);
    }

    #[test]
    fn a_held_key_waits_out_its_delay_before_the_first_repeat() {
        let repeat = KeyRepeat {
            held_key: Some(1),
            ..Default::default()
        };
        assert_eq!(repeat.delay_ms, 400);
        assert_eq!(park_ms(repeat.due_in(0, None), 0), 400);
        assert_eq!(park_ms(repeat.due_in(390, None), 0), 10);
        assert_eq!(repeat.due_in(400, None), Some(0));
    }

    /// The editing keys and the modifiers are layout-independent, so these read
    /// the same with no keymap loaded, which is the launcher's startup state.
    #[test]
    fn the_editing_keys_are_read_off_the_physical_keycode() {
        let xkb = Xkb::new().expect("xkb context");
        assert_eq!(key_action(&xkb, keycode::KEY_ENTER), KeyAction::Enter);
        // The keypad's Enter launches too; through the layout it would type a
        // carriage return, which text input drops.
        assert_eq!(key_action(&xkb, keycode::KEY_KPENTER), KeyAction::Enter);
        assert_eq!(key_action(&xkb, keycode::KEY_ESC), KeyAction::Escape);
        assert_eq!(
            key_action(&xkb, keycode::KEY_BACKSPACE),
            KeyAction::Backspace
        );
        assert_eq!(key_action(&xkb, keycode::KEY_UP), KeyAction::Up);
        assert_eq!(key_action(&xkb, keycode::KEY_HOME), KeyAction::Home);
    }

    #[test]
    fn a_modifier_on_its_own_is_not_an_action() {
        let xkb = Xkb::new().expect("xkb context");
        for key in [
            keycode::KEY_LEFTSHIFT,
            keycode::KEY_LEFTCTRL,
            keycode::KEY_CAPSLOCK,
            keycode::KEY_LEFTALT,
        ] {
            assert_eq!(key_action(&xkb, key), KeyAction::None);
        }
    }

    #[test]
    fn a_letter_types_nothing_until_a_keymap_arrives() {
        // Character input is the one thing that needs the layout.
        let xkb = Xkb::new().expect("xkb context");
        assert_eq!(key_action(&xkb, keycode::KEY_A), KeyAction::None);
    }

    #[test]
    fn parse_action_reads_the_search_prefix_only_when_enabled() {
        assert!(matches!(
            parse_action("s: cats ", true),
            InputAction::Search("cats")
        ));
        // Without a configured search URL the prefix is ordinary text.
        assert!(matches!(
            parse_action("s: cats ", false),
            InputAction::AppSearch("s: cats ")
        ));
    }

    #[test]
    fn parse_action_reads_the_url_prefix_regardless_of_search() {
        assert!(matches!(
            parse_action("u: example.com ", false),
            InputAction::OpenUrl("example.com")
        ));
    }

    #[test]
    fn parse_action_leaves_plain_input_untrimmed() {
        // The app query is passed through as typed: a trailing space is part of
        // what the user is searching for.
        assert!(matches!(
            parse_action(" fire ", true),
            InputAction::AppSearch(" fire ")
        ));
    }

    #[test]
    fn normalize_url_adds_a_scheme_only_when_missing() {
        assert_eq!(norm("example.com"), "https://example.com");
        assert_eq!(norm("http://example.com"), "http://example.com");
        assert_eq!(norm("file:///tmp/x"), "file:///tmp/x");
        // mailto has no slashes, and is still a scheme the launcher opens.
        assert_eq!(norm("mailto:a@b.c"), "mailto:a@b.c");
        // A host and port is not a scheme.
        assert_eq!(norm("localhost:3000"), "https://localhost:3000");
    }

    #[test]
    fn resolve_results_caps_rows_at_what_the_list_shows() {
        // More matches than the UI paints: the row count stops at MAX_RESULTS so
        // selection cannot land on a row the user never sees.
        let names: Vec<(String, String)> = (0..MAX_RESULTS + 5)
            .map(|i| (format!("App{}", i), "app".to_string()))
            .collect();
        let refs: Vec<(&str, &str)> = names
            .iter()
            .map(|(n, e)| (n.as_str(), e.as_str()))
            .collect();
        let catalog = entries(&refs);
        let (results, rows) = resolve_results("app", true, &catalog, &[]);
        assert!(results.len() > MAX_RESULTS);
        assert_eq!(rows, MAX_RESULTS);
    }

    #[test]
    fn resolve_results_counts_one_row_for_a_non_empty_url() {
        let catalog = entries(&[("Firefox", "firefox")]);
        let (results, rows) = resolve_results("u:example.com", true, &catalog, &[]);
        assert!(results.is_empty());
        assert_eq!(rows, 1);
        // An empty target has nothing to launch, so it offers no row.
        let (_, rows) = resolve_results("u:", true, &catalog, &[]);
        assert_eq!(rows, 0);
    }
}
