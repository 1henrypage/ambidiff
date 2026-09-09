//! Root-relative filesystem reads for the review root.
//!
//! Every read walks from an `openat` file descriptor on the root directory,
//! one path component at a time, refusing to follow a symlink out of the
//! root (`O_NOFOLLOW` on every component) and refusing any path that could
//! escape the root before a single syscall runs (absolute, `..`, `.`, an
//! empty component, a trailing slash, an embedded NUL). The final component
//! is opened without following: a symlink there is reported as its link
//! text (matching how Git itself treats a symlink entry), never followed.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub use crate::source::PathReason;

/// A file, or the link text of a symlink entry, read from the review root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootEntry {
    File(Vec<u8>),
    Symlink(String),
}

/// Failure reading a root-relative path.
#[derive(Debug)]
pub enum RootError {
    /// The path string itself is rejected before any syscall (see
    /// [`check_relative_path`]), or a middle path component is a symlink.
    InvalidPath(PathReason),
    /// The final component exists but is not a regular file or a symlink
    /// (a directory, a fifo, a device, a socket).
    NotRegularFile,
    Io(io::Error),
}

/// Split and validate a root-relative path string into components, denying
/// anything that could let it escape the root: an absolute path, `..`, `.`,
/// an empty component (`a//b`), a trailing slash, and an embedded NUL. Pure;
/// runs before any syscall touches the filesystem.
pub fn check_relative_path(path: &str) -> Result<Vec<&str>, PathReason> {
    if path.is_empty() {
        return Err(PathReason::Empty);
    }
    if path.contains('\0') {
        return Err(PathReason::ContainsNul);
    }
    if path.starts_with('/') {
        return Err(PathReason::Absolute);
    }
    if path.ends_with('/') {
        return Err(PathReason::TrailingSlash);
    }
    let mut parts = Vec::new();
    for component in path.split('/') {
        match component {
            "" => return Err(PathReason::EmptyComponent),
            "." => return Err(PathReason::CurrentComponent),
            ".." => return Err(PathReason::ParentComponent),
            other => parts.push(other),
        }
    }
    Ok(parts)
}

fn cstr(component: &str) -> io::Result<CString> {
    CString::new(component.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path component contains NUL"))
}

fn is_symlink_at(parent_fd: RawFd, name: &CString) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatat(parent_fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
    rc == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFLNK
}

fn readlink_at(parent_fd: RawFd, name: &CString) -> io::Result<String> {
    let mut buf = vec![0u8; 4096];
    loop {
        let n = unsafe {
            libc::readlinkat(
                parent_fd,
                name.as_ptr(),
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = n as usize;
        if n < buf.len() {
            buf.truncate(n);
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
        buf.resize(buf.len() * 2, 0);
    }
}

enum Final {
    File(File),
    Symlink(String),
}

/// An open file descriptor on the review root, the base every read walks
/// from.
pub struct ReviewRoot {
    root: File,
}

impl ReviewRoot {
    pub fn open(root: &Path) -> io::Result<Self> {
        let c = CString::new(root.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "root path contains NUL"))?;
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ReviewRoot {
            root: unsafe { File::from_raw_fd(fd) },
        })
    }

    /// Walk the directory components of a validated path, returning the
    /// open descriptor on the final directory (or a duplicate of the root
    /// descriptor when `dirs` is empty). `Ok(None)` means a component along
    /// the way does not exist.
    fn walk_dirs(&self, dirs: &[&str]) -> Result<Option<File>, RootError> {
        if dirs.is_empty() {
            let dup = unsafe { libc::fcntl(self.root.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                return Err(RootError::Io(io::Error::last_os_error()));
            }
            return Ok(Some(unsafe { File::from_raw_fd(dup) }));
        }
        let mut cur_fd = self.root.as_raw_fd();
        let mut owned: Option<File> = None;
        for comp in dirs {
            let name = cstr(comp).map_err(RootError::Io)?;
            let fd = unsafe {
                libc::openat(
                    cur_fd,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let err = io::Error::last_os_error();
                return match err.raw_os_error() {
                    Some(e) if e == libc::ELOOP => {
                        Err(RootError::InvalidPath(PathReason::SymlinkComponent))
                    }
                    // `O_DIRECTORY | O_NOFOLLOW` on a symlink reports ELOOP on
                    // Linux but ENOTDIR on macOS/BSD; disambiguate with an
                    // `AT_SYMLINK_NOFOLLOW` stat rather than guessing which
                    // platform we're on.
                    Some(e) if e == libc::ENOTDIR => {
                        if is_symlink_at(cur_fd, &name) {
                            Err(RootError::InvalidPath(PathReason::SymlinkComponent))
                        } else {
                            Ok(None)
                        }
                    }
                    Some(e) if e == libc::ENOENT => Ok(None),
                    _ => Err(RootError::Io(err)),
                };
            }
            let f = unsafe { File::from_raw_fd(fd) };
            cur_fd = f.as_raw_fd();
            owned = Some(f);
        }
        Ok(owned)
    }

    fn open_final(&self, rel: &str) -> Result<Option<Final>, RootError> {
        let parts = check_relative_path(rel).map_err(RootError::InvalidPath)?;
        let (dirs, last) = parts.split_at(parts.len() - 1);
        let dir_file = match self.walk_dirs(dirs)? {
            Some(f) => f,
            None => return Ok(None),
        };
        let name = cstr(last[0]).map_err(RootError::Io)?;
        let fd = unsafe {
            libc::openat(
                dir_file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_NOCTTY
                    | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let err = io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(e) if e == libc::ELOOP => {
                    let text = readlink_at(dir_file.as_raw_fd(), &name).map_err(RootError::Io)?;
                    Ok(Some(Final::Symlink(text)))
                }
                Some(e) if e == libc::ENOENT || e == libc::ENOTDIR => Ok(None),
                _ => Err(RootError::Io(err)),
            };
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata().map_err(RootError::Io)?;
        if !meta.is_file() {
            return Err(RootError::NotRegularFile);
        }
        Ok(Some(Final::File(file)))
    }

    /// Read a root-relative path's whole content: at most `limit + 1` bytes
    /// (the caller compares the length against `limit` to detect an
    /// oversized file without ever reading it in full). `Ok(None)` means the
    /// path does not exist.
    pub fn read(&self, rel: &str, limit: u64) -> Result<Option<RootEntry>, RootError> {
        match self.open_final(rel)? {
            None => Ok(None),
            Some(Final::Symlink(text)) => Ok(Some(RootEntry::Symlink(text))),
            Some(Final::File(file)) => {
                let mut buf = Vec::new();
                file.take(limit + 1)
                    .read_to_end(&mut buf)
                    .map_err(RootError::Io)?;
                Ok(Some(RootEntry::File(buf)))
            }
        }
    }

    /// Size of a root-relative path without reading its content (a
    /// symlink's size is its link text's length, matching Git).
    pub fn size(&self, rel: &str) -> Result<Option<u64>, RootError> {
        match self.open_final(rel)? {
            None => Ok(None),
            Some(Final::Symlink(text)) => Ok(Some(text.len() as u64)),
            Some(Final::File(file)) => {
                let meta = file.metadata().map_err(RootError::Io)?;
                Ok(Some(meta.len()))
            }
        }
    }

    /// Stream a root-relative path's content through `sink` in bounded
    /// chunks, at most `limit + 1` bytes total, without buffering the whole
    /// file. `Ok(None)` means the path does not exist.
    pub fn read_with(
        &self,
        rel: &str,
        limit: u64,
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> Result<Option<()>, RootError> {
        match self.open_final(rel)? {
            None => Ok(None),
            Some(Final::Symlink(text)) => {
                sink(text.as_bytes()).map_err(RootError::Io)?;
                Ok(Some(()))
            }
            Some(Final::File(file)) => {
                let mut reader = file.take(limit + 1);
                let mut buf = [0u8; 65536];
                loop {
                    let n = reader.read(&mut buf).map_err(RootError::Io)?;
                    if n == 0 {
                        break;
                    }
                    sink(&buf[..n]).map_err(RootError::Io)?;
                }
                Ok(Some(()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_relative_path_accepts_plain_and_nested() {
        assert_eq!(check_relative_path("a").expect("valid"), vec!["a"]);
        assert_eq!(
            check_relative_path("a/b/c").expect("valid"),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn check_relative_path_rejects_each_escape_shape() {
        assert_eq!(check_relative_path(""), Err(PathReason::Empty));
        assert_eq!(check_relative_path("/a"), Err(PathReason::Absolute));
        assert_eq!(check_relative_path("a/"), Err(PathReason::TrailingSlash));
        assert_eq!(check_relative_path("a//b"), Err(PathReason::EmptyComponent));
        assert_eq!(
            check_relative_path("./a"),
            Err(PathReason::CurrentComponent)
        );
        assert_eq!(
            check_relative_path("../a"),
            Err(PathReason::ParentComponent)
        );
        assert_eq!(
            check_relative_path("a/../b"),
            Err(PathReason::ParentComponent)
        );
        assert_eq!(check_relative_path("a\0b"), Err(PathReason::ContainsNul));
    }

    #[test]
    fn read_missing_path_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = ReviewRoot::open(dir.path()).expect("open root");
        assert!(root.read("nope.txt", 1024).expect("read").is_none());
        assert!(root.size("nope.txt").expect("size").is_none());
    }

    #[test]
    fn read_regular_file_round_trips_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), b"hello world").expect("write");
        let root = ReviewRoot::open(dir.path()).expect("open root");
        match root.read("a.txt", 1024).expect("read").expect("present") {
            RootEntry::File(bytes) => assert_eq!(bytes, b"hello world"),
            RootEntry::Symlink(_) => panic!("expected a file"),
        }
        assert_eq!(root.size("a.txt").expect("size"), Some(11));
    }

    #[test]
    fn read_nested_file_walks_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("sub/dir")).expect("mkdir");
        std::fs::write(dir.path().join("sub/dir/f.txt"), b"nested").expect("write");
        let root = ReviewRoot::open(dir.path()).expect("open root");
        match root
            .read("sub/dir/f.txt", 1024)
            .expect("read")
            .expect("present")
        {
            RootEntry::File(bytes) => assert_eq!(bytes, b"nested"),
            RootEntry::Symlink(_) => panic!("expected a file"),
        }
    }

    #[test]
    fn read_caps_at_limit_plus_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("big.txt"), vec![b'x'; 100]).expect("write");
        let root = ReviewRoot::open(dir.path()).expect("open root");
        match root.read("big.txt", 10).expect("read").expect("present") {
            RootEntry::File(bytes) => assert_eq!(bytes.len(), 11),
            RootEntry::Symlink(_) => panic!("expected a file"),
        }
    }

    #[test]
    fn symlink_entry_reads_as_link_text_not_target_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("target.txt"), b"target content").expect("write");
        std::os::unix::fs::symlink("target.txt", dir.path().join("link.txt")).expect("symlink");
        let root = ReviewRoot::open(dir.path()).expect("open root");
        match root.read("link.txt", 1024).expect("read").expect("present") {
            RootEntry::Symlink(text) => assert_eq!(text, "target.txt"),
            RootEntry::File(_) => panic!("expected a symlink"),
        }
    }

    #[test]
    fn escaping_symlink_component_is_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::write(outside.path().join("secret.txt"), b"secret").expect("write");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).expect("symlink");
        let root = ReviewRoot::open(dir.path()).expect("open root");
        match root.read("escape/secret.txt", 1024) {
            Err(RootError::InvalidPath(PathReason::SymlinkComponent)) => {}
            other => panic!("expected a denied symlink component, got {other:?}"),
        }
    }

    #[test]
    fn directory_entry_is_not_a_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        let root = ReviewRoot::open(dir.path()).expect("open root");
        match root.read("sub", 1024) {
            Err(RootError::NotRegularFile) => {}
            other => panic!("expected NotRegularFile, got {other:?}"),
        }
    }

    #[test]
    fn read_with_streams_without_buffering_whole_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("f.txt"), b"abcdefgh").expect("write");
        let root = ReviewRoot::open(dir.path()).expect("open root");
        let mut collected = Vec::new();
        root.read_with("f.txt", 1024, &mut |chunk| {
            collected.extend_from_slice(chunk);
            Ok(())
        })
        .expect("read_with");
        assert_eq!(collected, b"abcdefgh");
    }
}
