//! The launcher application: single-instance lock, desktop-entry resolution,
//! the io_uring event loop, and input handling.

use crate::client::{Client, PendingAction};
use crate::clipboard;
use crate::config::{MAX_RESULTS, WINDOW_WIDTH};
use crate::desktop::{self, DesktopEntry};
use crate::platform::error::elog;
use crate::platform::error::{Error, Result};
use crate::platform::time::Instant;
use crate::platform::xkb::{keycode, Xkb};
use crate::platform::{arena, env, fs, syscall, uring};
use crate::ui::{calculate_height, draw_ui};
use crate::{cache, config, denylist, font, launch, ui};

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
        KEY_ENTER => return KeyAction::Enter,
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

    // Caret blink state. The caret is solid for one interval then toggles; any
    // input activity resets it to solid in the event loop below.
    let mut cursor_visible = true;
    let mut last_blink = Instant::now();

    // The query the current result list was built from, so an edit to the text
    // can be told apart from a cursor move or a click, which leave it standing.
    let mut last_query = client.editor.text_owned();

    // Initial draw of the empty-input view (recents). The result list borrows
    // state, so scope it to this block: it must not hold that borrow across the
    // loop, where state is mutated.
    let mut current_results_len = cache::resolve(&state.recents, &state.entries)
        .len()
        .min(MAX_RESULTS);
    repaint(&mut client, &state, &font, search_enabled, cursor_visible)?;

    // Non-blocking socket: a poll readiness wakeup then drains every queued
    // message in one pass without blocking on the final partial read.
    client.socket.set_nonblocking(true)?;

    // Drive the loop with io_uring. A one-shot poll on the Wayland socket wakes
    // the loop the instant the compositor sends events; a one-shot timer paces
    // key repeat and the caret blink. Each is re-armed after it fires. Between
    // wakeups the thread is parked in io_uring_enter rather than sleep-polling.
    let wl_fd = client.socket.fd();
    // tick is declared ahead of the ring so it is dropped after it: the kernel
    // reads the timespec asynchronously, and the ring must be gone (its ops
    // cancelled) before the memory behind that pointer goes away.
    let tick = syscall::kernel_timespec {
        tv_sec: 0,
        tv_nsec: TICK_NANOS,
    };
    let mut ring = uring::Ring::new(RING_ENTRIES).map_err(Error::from_errno)?;
    // Both submissions are one-shot and re-armed as they complete. A failed
    // re-arm is fatal: the loop parks in io_uring_enter waiting for a
    // completion that nothing will ever produce.
    let arm_failed = || Error::msg("io_uring submission queue full");
    ring.prep_poll_add(wl_fd, syscall::POLLIN as u32, K_WAYLAND)
        .map_err(|_| arm_failed())?;
    // SAFETY: tick outlives the ring (see above), so the timespec the kernel
    // reads stays valid for as long as any timeout op can be in flight.
    unsafe { ring.prep_timeout(&tick, K_TIMER) }.map_err(|_| arm_failed())?;

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
                    ring.prep_poll_add(wl_fd, syscall::POLLIN as u32, K_WAYLAND)
                        .map_err(|_| arm_failed())?;
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
                    // SAFETY: tick outlives the ring, as above.
                    unsafe { ring.prep_timeout(&tick, K_TIMER) }.map_err(|_| arm_failed())?;
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
        if !matches!(client.clipboard_op, clipboard::Op::None) {
            client.process_clipboard();
        }

        // Handle pointer click/drag in the input box
        handle_pointer_input(&mut client, &font);

        // Repaint on any of three triggers: the input changed, the selection
        // moved, or the caret blinked. They differ only in what they settle
        // first; the drawing itself is one path.
        if client.input_changed {
            client.input_changed = false;
            // Activity keeps the caret solid; restart the blink from now.
            cursor_visible = true;
            last_blink = Instant::now();

            let text = client.editor.text_owned();
            let query_changed = text != last_query;
            last_query = text;
            let total_rows =
                resolve_results(&text, search_enabled, &state.entries, &state.recents).1;
            current_results_len = total_rows;

            // A new query is a new list. An index carried over from the previous
            // one points at a row the user never scanned, and Enter would launch
            // it. The bounds check covers the rest (a cursor move or a click
            // leaves the query, and so the selection, alone).
            if query_changed || state.selected >= total_rows {
                state.selected = 0;
            }

            // The window grows and shrinks with the row count.
            client.resize_surface(WINDOW_WIDTH, calculate_height(total_rows))?;
            repaint(&mut client, &state, &font, search_enabled, cursor_visible)?;
        } else if selection_moved {
            // The highlight moved. The caret keeps its phase and the window its
            // size, so navigation does not disturb the blink.
            repaint(&mut client, &state, &font, search_enabled, cursor_visible)?;
        } else if last_blink.elapsed_ms() >= CURSOR_BLINK_MS {
            cursor_visible = !cursor_visible;
            last_blink = Instant::now();
            repaint(&mut client, &state, &font, search_enabled, cursor_visible)?;
        }
    }

    // Check if we should launch something
    if client.pending_action == PendingAction::Launch {
        match parse_action(client.editor.text(), search_enabled) {
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

/// Draw the current state into the next frame and show it.
///
/// The result list borrows the catalog, so it is built inside the draw and
/// dropped with it; nothing holds that borrow across the event loop, where the
/// state is mutated.
fn repaint(
    client: &mut Client,
    state: &AppState,
    font: &font::Font,
    search_enabled: bool,
    cursor_visible: bool,
) -> Result<()> {
    let text = client.editor.text_owned();
    client.render(|client| {
        let (results, _) = resolve_results(&text, search_enabled, &state.entries, &state.recents);
        draw_ui(
            client,
            &text,
            state,
            &results,
            font,
            search_enabled,
            cursor_visible,
        );
    })
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
    use super::*;

    fn entries(names: &[(&str, &str)]) -> desktop::Catalog {
        let mut out = desktop::Catalog::new();
        for (name, exec) in names {
            let _ = out.push(DesktopEntry::new(name, exec, "").expect("entry"));
        }
        out
    }

    fn norm(url: &str) -> String {
        let mut out: arena::ArrayString<URL_CAP> = arena::ArrayString::new();
        normalize_url(url, &mut out);
        out.as_str().to_string()
    }

    /// The editing keys and the modifiers are layout-independent, so these read
    /// the same with no keymap loaded, which is the launcher's startup state.
    #[test]
    fn the_editing_keys_are_read_off_the_physical_keycode() {
        let xkb = Xkb::new().expect("xkb context");
        assert_eq!(key_action(&xkb, keycode::KEY_ENTER), KeyAction::Enter);
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
