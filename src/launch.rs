//! Application launching via fork/exec.

use core::ffi::c_char;
use core::fmt::Write;

use crate::desktop::{exec_argv, Catalog, DesktopEntry, ARG_CAP, MAX_ARGS};
use crate::platform::arena::{ArrayString, ArrayVec};
use crate::platform::error::{Error, Result};
use crate::platform::syscall;
use crate::platform::{env, fs};

extern "C" {
    /// PATH-searching exec; replaces the process image or returns -1 on failure.
    fn execvp(file: *const c_char, argv: *const *const c_char) -> i32;
}

/// The arguments after the program: the tokenized command line, with up to two
/// more in front of it, flatpak-spawn's --host and the terminal runner.
const LAUNCH_ARGS: usize = MAX_ARGS + 2;

/// What a Terminal=true entry starts through. It opens the user's preferred
/// terminal running the command line after it.
const TERMINAL_RUNNER: &str = "xdg-terminal-exec";

/// The host's program directories, seen from inside a Flatpak sandbox: the
/// manifest binds the host's /usr under /run/host.
const HOST_BIN_DIRS: [&str; 2] = ["/run/host/usr/local/bin", "/run/host/usr/bin"];

/// Byte capacity of a URL the launcher builds or opens. Larger than the search
/// URL template it is built from (config::SEARCH_URL_CAP), since the
/// percent-encoded query goes in as well.
pub const URL_CAP: usize = 2048;

/// Launch an application from its desktop entry.
///
/// The Exec value is tokenized per the Desktop Entry spec and exec'd directly,
/// never through a shell, so quoting metacharacters in a .desktop file cannot
/// run subcommands. Spawns in a new process group and returns immediately; the
/// child is detached from the launcher's group and reparents to init on exit.
pub fn launch(entry: &DesktopEntry) -> Result<()> {
    let mut argv: ArrayVec<ArrayString<ARG_CAP>, MAX_ARGS> = ArrayVec::new();
    // A command line that does not tokenize whole is refused, never trimmed to
    // fit: exec'ing a mangled argv is worse than not launching.
    exec_argv(&entry.exec, &mut argv)
        .map_err(|_| Error::msg("exec command has too many arguments"))?;

    let mut args: ArrayVec<&str, LAUNCH_ARGS> = ArrayVec::new();
    let program = exec_target(&argv, entry.terminal, env::in_flatpak(), &mut args)?;
    spawn::<{ ARG_CAP + 1 }>(program, &args)
}

/// The program to exec, with its arguments filled into args. A terminal app
/// starts through the terminal runner. Inside a Flatpak sandbox PATH holds only
/// the runtime's binaries, so flatpak-spawn hands the command line to the host,
/// which runs it under the session's environment.
fn exec_target<'a>(
    argv: &'a [ArrayString<ARG_CAP>],
    terminal: bool,
    in_flatpak: bool,
    args: &mut ArrayVec<&'a str, LAUNCH_ARGS>,
) -> Result<&'a str> {
    let (command, command_args) = argv
        .split_first()
        .ok_or_else(|| Error::msg("empty exec command"))?;
    // What goes in front of the command, program first.
    let mut prefix: ArrayVec<&'static str, 3> = ArrayVec::new();
    if in_flatpak {
        let _ = prefix.push("flatpak-spawn");
        let _ = prefix.push("--host");
    }
    if terminal {
        let _ = prefix.push(TERMINAL_RUNNER);
    }
    // argv holds at most MAX_ARGS tokens, so the prefix still fits beside it.
    let program = match prefix.split_first() {
        Some((&front, prefix_args)) => {
            for &a in prefix_args {
                let _ = args.push(a);
            }
            let _ = args.push(command.as_str());
            front
        }
        None => command.as_str(),
    };
    for a in command_args {
        let _ = args.push(a.as_str());
    }
    Ok(program)
}

/// Drop the entries that cannot start here. A Terminal=true app starts through
/// the terminal runner, and without one it would run with no window at all.
/// in_flatpak and path are the sandbox check and PATH, passed in.
pub fn drop_unlaunchable(entries: &mut Catalog, in_flatpak: bool, path: Option<&str>) {
    if !entries.iter().any(|e| e.terminal) || terminal_runner_found(in_flatpak, path) {
        return;
    }
    entries.retain(|e| !e.terminal);
}

/// Whether the terminal runner is installed: on PATH, or in the host's program
/// directories when sandboxed, since flatpak-spawn runs it on the host.
fn terminal_runner_found(in_flatpak: bool, path: Option<&str>) -> bool {
    let found = |dir: &str| fs::join(dir, TERMINAL_RUNNER).is_some_and(|p| fs::is_executable(&p));
    if in_flatpak {
        return HOST_BIN_DIRS.iter().any(|dir| found(dir));
    }
    path.unwrap_or_default()
        .split(':')
        .filter(|dir| !dir.is_empty())
        .any(found)
}

/// Open a URL in the user's default browser via xdg-open.
pub fn launch_url(url: &str) -> Result<()> {
    // xdg-open is a shell wrapper, so only hand it a vetted URL: reject a
    // leading dash (which it could read as an option) and anything without a
    // scheme we are willing to open.
    //
    // Not sent to the host: the runtime's xdg-open asks the portal, which needs
    // no permission and reaches the same browser.
    if url.starts_with('-') || !has_allowed_scheme(url) {
        return Err(Error::msg("refusing to open URL with no allowed scheme"));
    }

    // A built URL runs up to URL_CAP bytes, well past an exec token's budget, so
    // spawn is sized for the URL here rather than rejecting valid long links.
    spawn::<{ URL_CAP + 1 }>("xdg-open", &[url])
}

/// Fork and exec a program, searching PATH, with stdio sent to /dev/null and
/// the child in its own process group. Returns once the child is started; the
/// child is detached and reparents to init on exit.
fn spawn<const CAP: usize>(program: &str, args: &[&str]) -> Result<()> {
    // Build NUL-terminated C strings (program then args) and a NULL-terminated
    // pointer array. Both live on the stack, alive across the exec below. CAP is
    // the per-argument byte budget including the terminator, set by the caller
    // to fit an exec token or a longer URL.
    let mut cstrings: ArrayVec<ArrayString<CAP>, { LAUNCH_ARGS + 1 }> = ArrayVec::new();
    let mut push_cstr = |s: &str| -> Result<()> {
        let mut c: ArrayString<CAP> = ArrayString::new();
        c.push_str(s)
            .and_then(|_| c.push('\0'))
            .map_err(|_| Error::msg("argument too long"))?;
        cstrings
            .push(c)
            .map_err(|_| Error::msg("too many arguments"))
    };
    push_cstr(program)?;
    for &a in args {
        push_cstr(a)?;
    }

    let mut ptrs: ArrayVec<*const c_char, { LAUNCH_ARGS + 2 }> = ArrayVec::new();
    for c in cstrings.iter() {
        let _ = ptrs.push(c.as_bytes().as_ptr() as *const c_char);
    }
    let _ = ptrs.push(core::ptr::null());

    let devnull_path = syscall::CPath::new("/dev/null");

    // SAFETY: the process is single-threaded, so the child runs only the
    // async-signal-safe setup below before execvp replaces its image.
    let pid = unsafe { syscall::fork() };
    if pid < 0 {
        return Err(Error::from_errno(-pid));
    }
    if pid == 0 {
        // Child: own process group, stdio to /dev/null, then exec.
        syscall::setpgid(0, 0);
        // exec keeps an ignored signal ignored and keeps the mask. The launcher
        // ignores SIGPIPE, and whatever started it may have ignored or blocked
        // more: nohup ignores SIGHUP, and sh -c 'bnklaunch &' ignores SIGINT and
        // SIGQUIT. So the application gets every signal back at its default,
        // with nothing blocked. SIGKILL and SIGSTOP refuse the change, which is
        // harmless.
        for signum in 1..=syscall::SIGRTMAX {
            syscall::signal_disposition(signum, syscall::SIG_DFL);
        }
        syscall::set_signal_mask(0);
        if let Some(ref devnull_path) = devnull_path {
            let devnull = syscall::openat(syscall::AT_FDCWD, devnull_path, syscall::O_RDWR, 0);
            if devnull >= 0 {
                syscall::dup2(devnull, 0);
                syscall::dup2(devnull, 1);
                syscall::dup2(devnull, 2);
                if devnull > 2 {
                    // SAFETY: devnull is the fd just opened; closed once here.
                    unsafe { syscall::close(devnull) };
                }
            }
        }
        // Only stdio goes to the application. The launcher's own fds are
        // close-on-exec, but one it inherited without the flag is not.
        // SAFETY: the child execs or exits next, so none of the Fd values it
        // copied from the parent is ever dropped.
        unsafe { syscall::close_range(3, u32::MAX) };
        // SAFETY: ptrs[0] and ptrs point at the NUL-terminated argv built above,
        // alive on the stack; execvp only reads them.
        unsafe { execvp(ptrs[0], ptrs.as_ptr()) };
        // execvp only returns on failure.
        syscall::exit_group(127);
    }
    // Parent: detached, do not wait.
    Ok(())
}

/// Whether a URL carries a scheme the launcher is willing to open.
fn has_allowed_scheme(url: &str) -> bool {
    const ALLOWED: [&str; 4] = ["http://", "https://", "file://", "mailto:"];
    ALLOWED.iter().any(|scheme| url.starts_with(scheme))
}

/// Open a web search for the given query using the configured search URL.
pub fn launch_search(search_url: &str, query: &str) -> Result<()> {
    let mut url: ArrayString<URL_CAP> = ArrayString::new();
    build_search_url(search_url, query, &mut url)?;
    launch_url(&url)
}

/// The one way these builders fail: the result outgrew URL_CAP.
fn url_too_long() -> Error {
    Error::msg("URL too long")
}

/// Substitute the percent-encoded query into the search URL: replace the first
/// %s, or append it when the template has no placeholder.
///
/// Err when the result does not fit. A truncated URL is not a shorter search,
/// it is a different one: the cut can land mid percent-escape and change the
/// query, or lop the tail off the template.
fn build_search_url(template: &str, query: &str, out: &mut ArrayString<URL_CAP>) -> Result<()> {
    let mut encoded: ArrayString<URL_CAP> = ArrayString::new();
    percent_encode(query, &mut encoded)?;
    if let Some(pos) = template.find("%s") {
        out.push_str(&template[..pos]).map_err(|_| url_too_long())?;
        out.push_str(encoded.as_str()).map_err(|_| url_too_long())?;
        out.push_str(&template[pos + 2..])
            .map_err(|_| url_too_long())?;
    } else {
        out.push_str(template).map_err(|_| url_too_long())?;
        out.push_str(encoded.as_str()).map_err(|_| url_too_long())?;
    }
    Ok(())
}

/// Percent-encode a string per RFC 3986: only unreserved characters
/// (ALPHA, DIGIT, and the four marks - . _ ~) pass through unchanged.
fn percent_encode<const N: usize>(s: &str, out: &mut ArrayString<N>) -> Result<()> {
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char).map_err(|_| url_too_long())?;
            }
            // An escape is three bytes appended as one, so a buffer with room
            // for only part of it takes none of it: a URL cut mid-escape asks
            // for something other than what the user typed.
            _ => {
                let mut esc: ArrayString<3> = ArrayString::new();
                write!(esc, "%{:02X}", byte).map_err(|_| url_too_long())?;
                out.push_str(esc.as_str()).map_err(|_| url_too_long())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::launch::*;

    fn enc(s: &str) -> String {
        let mut out: ArrayString<256> = ArrayString::new();
        percent_encode(s, &mut out).expect("encode");
        out.as_str().to_string()
    }

    fn surl(template: &str, query: &str) -> String {
        let mut out: ArrayString<URL_CAP> = ArrayString::new();
        build_search_url(template, query, &mut out).expect("build");
        out.as_str().to_string()
    }

    #[test]
    fn percent_encode_unreserved_passthrough() {
        assert_eq!(enc("abcXYZ-._~09"), "abcXYZ-._~09");
    }

    #[test]
    fn build_search_url_substitutes_placeholder() {
        assert_eq!(
            surl("https://duckduckgo.com/?q=%s", "hello world"),
            "https://duckduckgo.com/?q=hello%20world"
        );
    }

    #[test]
    fn build_search_url_appends_when_no_placeholder() {
        assert_eq!(
            surl("https://example.com/?q=", "a&b"),
            "https://example.com/?q=a%26b"
        );
    }

    #[test]
    fn build_search_url_replaces_only_first_placeholder() {
        assert_eq!(surl("https://x/%s/%s", "a"), "https://x/a/%s");
    }

    #[test]
    fn percent_encode_spaces_and_punctuation() {
        assert_eq!(enc("hello world"), "hello%20world");
        assert_eq!(enc("a&b=c"), "a%26b%3Dc");
        assert_eq!(enc("/?#"), "%2F%3F%23");
    }

    #[test]
    fn percent_encode_unicode_uses_utf8_bytes() {
        // "æ" is 0xC3 0xA6 in UTF-8
        assert_eq!(enc("æ"), "%C3%A6");
    }

    #[test]
    fn allowed_schemes_pass() {
        assert!(has_allowed_scheme("https://example.com"));
        assert!(has_allowed_scheme("http://example.com"));
        assert!(has_allowed_scheme("file:///etc/hosts"));
        assert!(has_allowed_scheme("mailto:a@b.com"));
    }

    #[test]
    fn disallowed_schemes_rejected() {
        assert!(!has_allowed_scheme("javascript:alert(1)"));
        assert!(!has_allowed_scheme("ftp://example.com"));
        assert!(!has_allowed_scheme("example.com"));
        assert!(!has_allowed_scheme("-x"));
    }

    /// The program and arguments a launch execs for a given Exec value.
    fn target(exec: &str, in_flatpak: bool) -> (String, Vec<String>) {
        target_of(exec, false, in_flatpak)
    }

    /// The same, for an entry with Terminal=true.
    fn terminal_target(exec: &str, in_flatpak: bool) -> (String, Vec<String>) {
        target_of(exec, true, in_flatpak)
    }

    fn target_of(exec: &str, terminal: bool, in_flatpak: bool) -> (String, Vec<String>) {
        let mut argv: ArrayVec<ArrayString<ARG_CAP>, MAX_ARGS> = ArrayVec::new();
        exec_argv(exec, &mut argv).expect("tokenize");
        let mut args: ArrayVec<&str, LAUNCH_ARGS> = ArrayVec::new();
        let program = exec_target(&argv, terminal, in_flatpak, &mut args).expect("target");
        (
            program.to_string(),
            args.iter().map(|a| a.to_string()).collect(),
        )
    }

    #[test]
    fn exec_target_runs_the_command_itself() {
        let (program, args) = target("firefox --new-window", false);
        assert_eq!(program, "firefox");
        assert_eq!(args, ["--new-window"]);
    }

    #[test]
    fn exec_target_goes_through_the_host_when_sandboxed() {
        let (program, args) = target("firefox --new-window", true);
        assert_eq!(program, "flatpak-spawn");
        assert_eq!(args, ["--host", "firefox", "--new-window"]);
    }

    #[test]
    fn exec_target_sandboxed_keeps_a_bare_command_whole() {
        let (program, args) = target("gimp", true);
        assert_eq!(program, "flatpak-spawn");
        assert_eq!(args, ["--host", "gimp"]);
    }

    #[test]
    fn exec_target_holds_a_full_length_command_line_when_sandboxed() {
        // The most a tokenized Exec yields, plus the two flatpak-spawn takes.
        let exec = (0..MAX_ARGS)
            .map(|i| format!("a{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let (program, args) = target(&exec, true);
        assert_eq!(program, "flatpak-spawn");
        assert_eq!(args.len(), MAX_ARGS + 1);
        assert_eq!(args[0], "--host");
        assert_eq!(args[1], "a0");
        assert_eq!(args[MAX_ARGS], format!("a{}", MAX_ARGS - 1));

        // A terminal app adds the runner in front as well.
        let (program, args) = terminal_target(&exec, true);
        assert_eq!(program, "flatpak-spawn");
        assert_eq!(args.len(), MAX_ARGS + 2);
        assert_eq!(args[..3], ["--host", "xdg-terminal-exec", "a0"]);
        assert_eq!(args[MAX_ARGS + 1], format!("a{}", MAX_ARGS - 1));
    }

    #[test]
    fn exec_target_opens_a_terminal_app_through_xdg_terminal_exec() {
        let (program, args) = terminal_target("btop --utf-force", false);
        assert_eq!(program, "xdg-terminal-exec");
        assert_eq!(args, ["btop", "--utf-force"]);

        let (program, args) = terminal_target("btop --utf-force", true);
        assert_eq!(program, "flatpak-spawn");
        assert_eq!(args, ["--host", "xdg-terminal-exec", "btop", "--utf-force"]);
    }

    /// Without xdg-terminal-exec a terminal app has nothing to open its window,
    /// so it is not offered. Only an executable file counts as the program.
    #[test]
    fn terminal_apps_are_offered_only_when_xdg_terminal_exec_is_installed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = format!(
            "{}/bnk_launch_runner_{}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let runner = format!("{dir}/xdg-terminal-exec");
        let path = format!("/nonexistent:{dir}");

        let offered = |path: &str| {
            let mut btop = DesktopEntry::new("btop", "btop").expect("entry");
            btop.terminal = true;
            let mut entries = Catalog::new();
            entries
                .push(DesktopEntry::new("Files", "nautilus").expect("entry"))
                .expect("room");
            entries.push(btop).expect("room");
            drop_unlaunchable(&mut entries, false, Some(path));
            entries
                .iter()
                .map(|e| e.name.as_str().to_string())
                .collect::<Vec<_>>()
        };

        assert_eq!(offered(&path), ["Files"], "no runner on PATH");

        std::fs::write(&runner, "#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(offered(&path), ["Files"], "a runner that cannot execute");

        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(offered(&path), ["Files", "btop"], "a runner on PATH");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// exec keeps an ignored signal and the signal mask, so whatever the
    /// launcher ignores or blocks, every app it starts would too. Something
    /// that starts the launcher under nohup or with & ignores SIGHUP, SIGINT or
    /// SIGQUIT; SIGUSR2 stands in for those, and SIGUSR1 for a blocked signal.
    /// Nothing in the suite uses either.
    #[test]
    fn a_launched_app_starts_with_every_signal_at_its_default() {
        const SIGUSR1: i32 = 10;
        const SIGUSR2: i32 = 12;
        assert_eq!(syscall::signal_disposition(SIGUSR2, syscall::SIG_IGN), 0);

        let out = format!(
            "{}/bnk_launch_signals_{}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::remove_file(&out);
        // The mask is per thread, and a forked child takes the forking thread's,
        // so the block lives on a thread of its own.
        let script = "exec cat /proc/self/status > \"$1\"";
        let spawned = std::thread::spawn({
            let out = out.clone();
            move || {
                assert_eq!(syscall::set_signal_mask(1 << (SIGUSR1 - 1)), 0);
                spawn::<{ ARG_CAP + 1 }>("sh", &["-c", script, "sh", &out])
            }
        })
        .join()
        .expect("spawning thread");
        spawned.expect("spawn sh");

        let start = std::time::Instant::now();
        let status = loop {
            let text = std::fs::read_to_string(&out).unwrap_or_default();
            if text.contains("SigBlk:") && text.contains("SigIgn:") {
                break text;
            }
            assert!(
                start.elapsed().as_secs() < 5,
                "the child never wrote its status"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let _ = std::fs::remove_file(&out);

        let mask = |field: &str| {
            let line = status
                .lines()
                .find(|l| l.starts_with(field))
                .expect("field");
            u64::from_str_radix(line[field.len()..].trim(), 16).expect("hex mask")
        };
        assert_eq!(mask("SigIgn:"), 0, "ignored signals, bit N-1 is signal N");
        assert_eq!(mask("SigBlk:"), 0, "blocked signals, bit N-1 is signal N");
    }

    /// A launched app gets stdio and nothing else. An fd the launcher holds
    /// without close-on-exec, one it inherited or one a peer passed it, would
    /// otherwise stay open in the app for its whole life: a clipboard pipe's
    /// reader would wait on an EOF that never comes.
    #[test]
    fn a_launched_app_inherits_only_stdio() {
        // A number no shell picks for its own redirections, opened the way an
        // inherited fd would be: without close-on-exec.
        const HELD: i32 = 517;
        let devnull = syscall::CPath::new("/dev/null").expect("path");
        let fd = syscall::openat(syscall::AT_FDCWD, &devnull, syscall::O_RDWR, 0);
        assert!(fd >= 0, "open /dev/null");
        assert_eq!(syscall::dup2(fd, HELD), HELD);
        // SAFETY: fd was opened above and nothing else owns it.
        unsafe { syscall::close(fd) };

        let out = format!(
            "{}/bnk_launch_fds_{}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::remove_file(&out);
        // Written aside and renamed, so the file appears whole.
        let script = "ls -1 /proc/self/fd > \"$1.tmp\" && mv \"$1.tmp\" \"$1\"";
        let spawned = spawn::<{ ARG_CAP + 1 }>("sh", &["-c", script, "sh", &out]);

        let start = std::time::Instant::now();
        let listing = loop {
            if let Ok(text) = std::fs::read_to_string(&out) {
                break text;
            }
            assert!(
                start.elapsed().as_secs() < 5,
                "the child never listed its fds"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let _ = std::fs::remove_file(&out);
        // SAFETY: HELD was made by the dup2 above and nothing else owns it.
        unsafe { syscall::close(HELD) };
        spawned.expect("spawn sh");

        assert!(
            !listing.lines().any(|l| l == HELD.to_string()),
            "the app inherited fd {HELD}: {listing:?}"
        );
    }

    #[test]
    fn exec_target_refuses_an_empty_command_line() {
        // Refused whatever goes in front, so nothing runs a bare flatpak-spawn
        // or xdg-terminal-exec.
        let argv: ArrayVec<ArrayString<ARG_CAP>, MAX_ARGS> = ArrayVec::new();
        for (terminal, in_flatpak) in [(false, false), (false, true), (true, false), (true, true)] {
            let mut args: ArrayVec<&str, LAUNCH_ARGS> = ArrayVec::new();
            assert!(exec_target(&argv, terminal, in_flatpak, &mut args).is_err());
        }
    }
}
