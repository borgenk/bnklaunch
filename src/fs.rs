//! Minimal filesystem access on raw syscalls.
//!
//! Paths are plain byte strings rather than a Path type; the helpers here
//! NUL-terminate them on the stack for the C-string syscall arguments. File
//! contents read into a caller-provided fixed buffer, and directory entries are
//! walked from a getdents64 buffer without allocating.

#![allow(dead_code)]

use crate::arena::{ArrayString, ArrayVec};
use crate::error::{Error, Result};
use crate::syscall::{
    self, linux_dirent64, stat, Fd, RawFd, AT_FDCWD, AT_SYMLINK_NOFOLLOW, EEXIST, EINTR, O_CREAT,
    O_DIRECTORY, O_RDONLY, O_TRUNC, O_WRONLY, S_IFDIR, S_IFMT,
};

/// Longest path the helpers handle.
pub const PATH_CAP: usize = crate::syscall::PATH_CAP;

/// Build a checked C path for the path syscalls, or an error if it does not fit
/// or holds an interior NUL.
pub fn cpath(path: &str) -> Result<syscall::CPath> {
    syscall::CPath::new(path).ok_or_else(|| Error::msg("invalid path"))
}

/// Read a whole file into out, replacing its contents. Errors if the file does
/// not fit out's capacity.
pub fn read_file<const N: usize>(path: &str, out: &mut ArrayVec<u8, N>) -> Result<()> {
    out.clear();
    let cp = cpath(path)?;
    let fd = syscall::openat(AT_FDCWD, &cp, O_RDONLY, 0);
    if fd < 0 {
        return Err(Error::from_errno(-fd));
    }
    let fd = Fd::new(fd);

    let mut chunk = [0u8; 8192];
    loop {
        let r = syscall::read_fd(fd.as_raw_fd(), &mut chunk);
        if r < 0 {
            let e = -r as i32;
            if e == EINTR {
                continue;
            }
            return Err(Error::from_errno(e));
        }
        if r == 0 {
            return Ok(());
        }
        if out.extend_from_slice(&chunk[..r as usize]).is_err() {
            return Err(Error::msg("file too large for buffer"));
        }
    }
}

/// Write data to a file, creating or truncating it (mode 0644).
pub fn write_file(path: &str, data: &[u8]) -> Result<()> {
    let cp = cpath(path)?;
    let fd = syscall::openat(AT_FDCWD, &cp, O_WRONLY | O_CREAT | O_TRUNC, 0o644);
    if fd < 0 {
        return Err(Error::from_errno(-fd));
    }
    let fd = Fd::new(fd);
    write_all(fd.as_raw_fd(), data)
}

/// Write a buffer in full, retrying short writes and interrupts.
pub fn write_all(fd: RawFd, data: &[u8]) -> Result<()> {
    let mut off = 0;
    while off < data.len() {
        let r = syscall::write_fd(fd, &data[off..]);
        if r < 0 {
            let e = -r as i32;
            if e == EINTR {
                continue;
            }
            return Err(Error::from_errno(e));
        }
        off += r as usize;
    }
    Ok(())
}

/// Rename a file (atomic within a filesystem).
pub fn rename(from: &str, to: &str) -> Result<()> {
    let f = cpath(from)?;
    let t = cpath(to)?;
    let r = syscall::rename(&f, &t);
    if r < 0 {
        return Err(Error::from_errno(-r));
    }
    Ok(())
}

/// Create a directory and any missing parents (mode 0755). An existing
/// directory is not an error.
pub fn mkdir_p(path: &str) -> Result<()> {
    let mut prefix: ArrayString<PATH_CAP> = ArrayString::new();
    for component in path.split('/') {
        if component.is_empty() {
            // A leading empty component means an absolute path: keep the root
            // slash and move on.
            if prefix.is_empty() {
                prefix.push('/').map_err(|_| Error::msg("path too long"))?;
            }
            continue;
        }
        if !prefix.is_empty() && !prefix.as_str().ends_with('/') {
            prefix.push('/').map_err(|_| Error::msg("path too long"))?;
        }
        prefix
            .push_str(component)
            .map_err(|_| Error::msg("path too long"))?;

        let cp = cpath(prefix.as_str())?;
        let r = syscall::mkdir(&cp, 0o755);
        if r < 0 && -r != EEXIST {
            return Err(Error::from_errno(-r));
        }
    }
    Ok(())
}

/// Stat a path, or None on any error. flags is passed to newfstatat (e.g.
/// AT_SYMLINK_NOFOLLOW to stat a symlink itself).
fn stat_path(path: &str, flags: i32) -> Option<stat> {
    let cp = cpath(path).ok()?;
    syscall::newfstatat(AT_FDCWD, &cp, flags)
}

/// Whether a path resolves to a directory (following symlinks).
pub fn is_dir(path: &str) -> bool {
    stat_path(path, 0).is_some_and(|st| st.st_mode & S_IFMT == S_IFDIR)
}

/// Whether a path is a directory without following a final symlink. Used by the
/// recursive scan so a symlinked directory is treated as a non-directory and
/// not descended into, which avoids cycles.
pub fn is_dir_nofollow(path: &str) -> bool {
    stat_path(path, AT_SYMLINK_NOFOLLOW).is_some_and(|st| st.st_mode & S_IFMT == S_IFDIR)
}

/// The modification time of a path in nanoseconds since the epoch, or None.
pub fn mtime_nanos(path: &str) -> Option<u64> {
    let st = stat_path(path, 0)?;
    let secs = (st.st_mtime.tv_sec as u64).checked_mul(1_000_000_000)?;
    Some(secs.wrapping_add(st.st_mtime.tv_nsec as u64))
}

/// Byte offset of d_name within a linux_dirent64 record (the fixed header is
/// 8 + 8 + 2 + 1 bytes, unpadded on the wire).
const DIRENT_NAME_OFFSET: usize = 19;

/// An open directory, read in getdents64 batches.
pub struct ReadDir {
    fd: Fd,
    buf: [u8; 8192],
    pos: usize,
    len: usize,
}

impl ReadDir {
    /// Open a directory for iteration.
    pub fn open(path: &str) -> Result<Self> {
        let cp = cpath(path)?;
        let fd = syscall::openat(AT_FDCWD, &cp, O_RDONLY | O_DIRECTORY, 0);
        if fd < 0 {
            return Err(Error::from_errno(-fd));
        }
        Ok(ReadDir {
            fd: Fd::new(fd),
            buf: [0; 8192],
            pos: 0,
            len: 0,
        })
    }

    /// Call f once per entry (excluding "." and ".."), passing the name and the
    /// getdents d_type. Recursing into a subdirectory opens a separate ReadDir,
    /// so it does not interfere with this one.
    pub fn for_each(&mut self, mut f: impl FnMut(&str, u8)) -> Result<()> {
        loop {
            if self.pos >= self.len {
                let fd = self.fd.as_raw_fd();
                let r = syscall::getdents64(fd, &mut self.buf);
                if r < 0 {
                    return Err(Error::from_errno(-r as i32));
                }
                if r == 0 {
                    return Ok(());
                }
                self.len = r as usize;
                self.pos = 0;
            }

            // The buffer is align-1 and a dirent header holds 8-aligned fields,
            // so read the header unaligned into a local rather than forming a
            // misaligned reference to it.
            // SAFETY: the kernel guarantees a full record at pos within len, and
            // its minimum length is the 24-byte header, so this read stays in
            // the buffer.
            let rec = unsafe {
                core::ptr::read_unaligned(self.buf.as_ptr().add(self.pos) as *const linux_dirent64)
            };
            let reclen = rec.d_reclen as usize;
            let d_type = rec.d_type;

            let name_start = self.pos + DIRENT_NAME_OFFSET;
            let name_end = self.pos + reclen;
            let raw = &self.buf[name_start..name_end];
            let nul = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            let name = core::str::from_utf8(&raw[..nul]).unwrap_or("");

            self.pos += reclen;

            if !name.is_empty() && name != "." && name != ".." {
                f(name, d_type);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_base(tag: &str) -> String {
        format!(
            "{}/bnk_fs_{}_{}",
            std::env::temp_dir().display(),
            tag,
            std::process::id()
        )
    }

    #[test]
    fn write_read_roundtrips_through_raw_syscalls() {
        let dir = temp_base("rw");
        mkdir_p(&dir).expect("mkdir");
        let path = format!("{dir}/file");
        write_file(&path, b"raw fs bytes").expect("write");

        let mut buf: ArrayVec<u8, 64> = ArrayVec::new();
        read_file(&path, &mut buf).expect("read");
        assert_eq!(buf.as_slice(), b"raw fs bytes");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_dir_lists_entries_and_skips_dot() {
        let dir = temp_base("dir");
        mkdir_p(&dir).expect("mkdir");
        write_file(&format!("{dir}/a.desktop"), b"x").expect("write a");
        mkdir_p(&format!("{dir}/sub")).expect("mkdir sub");

        let mut names: Vec<String> = Vec::new();
        ReadDir::open(&dir)
            .expect("open")
            .for_each(|name, _| names.push(name.to_string()))
            .expect("iterate");
        names.sort();
        assert_eq!(names, vec!["a.desktop".to_string(), "sub".to_string()]);

        assert!(is_dir(&dir));
        assert!(is_dir(&format!("{dir}/sub")));
        assert!(!is_dir(&format!("{dir}/a.desktop")));
        assert!(mtime_nanos(&dir).is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_moves_a_file() {
        let dir = temp_base("ren");
        mkdir_p(&dir).expect("mkdir");
        write_file(&format!("{dir}/from"), b"data").expect("write");
        rename(&format!("{dir}/from"), &format!("{dir}/to")).expect("rename");

        let mut buf: ArrayVec<u8, 16> = ArrayVec::new();
        read_file(&format!("{dir}/to"), &mut buf).expect("read");
        assert_eq!(buf.as_slice(), b"data");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
