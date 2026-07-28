//! The platform layer: everything that is about talking to the machine rather
//! than about what a launcher does.
//!
//! - **Foundation**: raw x86_64 Linux syscalls (syscall), the minimal error type
//!   (error), fixed-capacity collections standing in for Vec and String (arena),
//!   filesystem helpers (fs), environment access (env), and the monotonic clock
//!   (time).
//! - **Event loop**: an io_uring instance with mapped submission and completion
//!   rings (uring).
//! - **Wayland**: the wire codec (wire), the connection (conn), and the
//!   hand-transcribed protocol constants (protocol).
//! - **Input**: keyboard translation via libxkbcommon (xkb).
//! - **Font**: FreeType rasterization (freetype).
//!
//! The layer is a clean leaf, and the point of keeping it one is that it could
//! be lifted out whole: a module here may depend on other platform modules and
//! on core, never on the app above it. Nothing in the language enforces that
//! direction inside a single crate, so the test at the bottom does.
//!
//! Because the binary is no_std, this layer carries pieces a std program would
//! not need to write: arena is the allocator's replacement, and fs, env, time,
//! and uring are the runtime around it. What each answers is a machine question
//! with a machine answer, which is what keeps them portable. Anything that
//! decides what an answer *means* (that a key press is a paste, that a name is
//! worth showing) belongs to the app.

pub(crate) mod arena;
pub(crate) mod bytes;
pub(crate) mod conn;
pub(crate) mod env;
pub(crate) mod error;
pub(crate) mod freetype;
pub(crate) mod fs;
pub(crate) mod protocol;
pub(crate) mod syscall;
pub(crate) mod time;
pub(crate) mod uring;
pub(crate) mod wire;
pub(crate) mod xkb;

#[cfg(test)]
mod tests {
    /// The platform layer must stay a clean leaf, so that lifting it out of the
    /// crate would be a move rather than an untangling: a module here may name
    /// only crate::platform::*, never crate::app, crate::client, crate::desktop,
    /// or anything else above it. Rust checks no such direction inside one
    /// crate, so this walks the sources and does.
    ///
    /// Only non-test code is checked. The binary is no_std, but the tests build
    /// with std and sit at the bottom of each file, so scanning stops at the
    /// first cfg(test): a test reaching for a type from the app layer to build a
    /// fixture says nothing about the leaf's dependencies.
    #[test]
    fn platform_only_depends_on_itself() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/platform");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(dir).expect("read src/platform") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("read platform source");
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            for (line_no, line) in src.lines().enumerate() {
                if line.trim_start().starts_with("#[cfg(test)]") {
                    break;
                }
                // Doc and line comments may cross-reference any module freely.
                if line.trim_start().starts_with("//") {
                    continue;
                }
                for (idx, _) in line.match_indices("crate::") {
                    let after = &line[idx + "crate::".len()..];
                    let module: String = after
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if !module.is_empty() && module != "platform" {
                        offenders.push(format!("{name}:{}: crate::{module}", line_no + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "platform modules must not reach outside the layer:\n{}",
            offenders.join("\n")
        );
    }
}
