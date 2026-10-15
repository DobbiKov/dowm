//! Creating the listening socket so that only the owning user can reach it.
//!
//! Three layers, any one of which keeps other users out: the directory is checked to be
//! ours and not group/other-writable (and is created 0700), the socket file is created
//! is chmod'ed to 0600 right after bind, and [`peer_is_current_user`] checks every
//! accepted connection's credentials.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("another dowm is already listening on {0}")]
    AlreadyRunning(String),
    #[error("{path} exists and is not a socket; refusing to replace it")]
    NotASocket { path: String },
    #[error("{path} is not a private directory owned by you ({why})")]
    UnsafeDirectory { path: String, why: &'static str },
    #[error("socket path {0} is too long for a Unix socket")]
    PathTooLong(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// `sun_path` is 104 bytes on macOS including the NUL.
const MAX_SOCKET_PATH: usize = 103;

pub fn bind(path: &Path) -> Result<UnixListener, BindError> {
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(BindError::PathTooLong(path.display().to_string()));
    }
    let dir = path.parent().ok_or_else(|| BindError::UnsafeDirectory {
        path: path.display().to_string(),
        why: "no parent directory",
    })?;
    prepare_dir(dir)?;
    remove_stale(path)?;

    // The directory is private (checked above, or created 0700), so nobody else can reach
    // the socket in the instant before it is restricted; peers are also credential-checked.
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn prepare_dir(dir: &Path) -> Result<(), BindError> {
    let unsafe_dir = |why| BindError::UnsafeDirectory { path: dir.display().to_string(), why };
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(unsafe_dir("not a directory, or a symlink"));
            }
            if meta.uid() != current_uid() {
                return Err(unsafe_dir("owned by someone else"));
            }
            if meta.mode() & 0o022 != 0 {
                return Err(unsafe_dir("writable by group or others"));
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            // mkdir applies the umask; chmod does not.
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// A leftover socket from a crashed daemon is removed; a live one is an error; anything
/// that is not a socket is never touched.
fn remove_stale(path: &Path) -> Result<(), BindError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if !meta.file_type().is_socket() || meta.uid() != current_uid() {
        return Err(BindError::NotASocket { path: path.display().to_string() });
    }
    match UnixStream::connect(path) {
        Ok(_) => Err(BindError::AlreadyRunning(path.display().to_string())),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            fs::remove_file(path)?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Whether the process on the other end of `stream` runs as the same user as us.
pub fn peer_is_current_user(stream: &UnixStream) -> bool {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: the fd is a live socket and both out-pointers are valid.
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    rc == 0 && uid == current_uid()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dowm-sock-{}-{name}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_socket_is_0600_and_its_new_directory_0700() {
        let dir = temp_dir("modes");
        let path = dir.join("dowm.sock");
        let _listener = bind(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn a_stale_socket_is_replaced_but_a_live_one_is_not() {
        let dir = temp_dir("stale");
        let path = dir.join("dowm.sock");
        drop(bind(&path).unwrap()); // listener gone, file left behind
        assert!(path.exists());
        let live = bind(&path).expect("stale socket replaced");
        assert!(matches!(bind(&path), Err(BindError::AlreadyRunning(_))));
        drop(live);
    }

    #[test]
    fn a_regular_file_at_the_path_is_never_deleted() {
        let dir = temp_dir("file");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("dowm.sock");
        fs::write(&path, "precious").unwrap();
        assert!(matches!(bind(&path), Err(BindError::NotASocket { .. })));
        assert_eq!(fs::read_to_string(&path).unwrap(), "precious");
    }

    #[test]
    fn a_group_writable_directory_is_refused() {
        let dir = temp_dir("open");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o770)).unwrap();
        assert!(matches!(bind(&dir.join("dowm.sock")), Err(BindError::UnsafeDirectory { .. })));
    }

    #[test]
    fn a_symlinked_directory_is_refused() {
        let real = temp_dir("real");
        fs::create_dir_all(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        let link = temp_dir("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(bind(&link.join("dowm.sock")), Err(BindError::UnsafeDirectory { .. })));
    }

    #[test]
    fn an_overlong_path_is_an_error_not_a_truncation() {
        let path = PathBuf::from(format!("/tmp/{}/dowm.sock", "x".repeat(120)));
        assert!(matches!(bind(&path), Err(BindError::PathTooLong(_))));
    }

    #[test]
    fn a_connection_from_this_user_passes_the_peer_check() {
        let dir = temp_dir("peer");
        let path = dir.join("dowm.sock");
        let listener = bind(&path).unwrap();
        let _client = UnixStream::connect(&path).unwrap();
        let (server_side, _) = listener.accept().unwrap();
        assert!(peer_is_current_user(&server_side));
    }
}
