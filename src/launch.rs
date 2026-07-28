//! Application launching via fork/exec.

use core::ffi::c_char;
use core::fmt::Write;

use crate::desktop::{exec_argv, DesktopEntry, ARG_CAP, MAX_ARGS};
use crate::platform::arena::{ArrayString, ArrayVec};
use crate::platform::error::{Error, Result};
use crate::platform::syscall;

extern "C" {
    /// PATH-searching exec; replaces the process image or returns -1 on failure.
    fn execvp(file: *const c_char, argv: *const *const c_char) -> i32;
}

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
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| Error::msg("empty exec command"))?;

    let mut arg_refs: ArrayVec<&str, MAX_ARGS> = ArrayVec::new();
    for a in rest {
        let _ = arg_refs.push(a.as_str());
    }
    spawn::<{ ARG_CAP + 1 }>(program.as_str(), &arg_refs)
}

/// Open a URL in the user's default browser via xdg-open.
pub fn launch_url(url: &str) -> Result<()> {
    // xdg-open is a shell wrapper, so only hand it a vetted URL: reject a
    // leading dash (which it could read as an option) and anything without a
    // scheme we are willing to open.
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
    let mut cstrings: ArrayVec<ArrayString<CAP>, { MAX_ARGS + 1 }> = ArrayVec::new();
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

    let mut ptrs: ArrayVec<*const c_char, { MAX_ARGS + 2 }> = ArrayVec::new();
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
        // An ignored signal stays ignored across exec, and the launcher ignores
        // SIGPIPE. Hand the application back the default, or its own pipelines
        // would see writes to a closed reader fail with EPIPE forever instead of
        // ending the process the way every tool expects.
        syscall::signal_disposition(syscall::SIGPIPE, syscall::SIG_DFL);
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

/// Byte capacity of a URL the launcher builds or opens.
pub const URL_CAP: usize = 2048;

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
    use super::*;

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
}
