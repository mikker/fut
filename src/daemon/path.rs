use std::{
    env, fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

fn absolute_path(value: &std::ffi::OsStr, source: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if path.as_os_str().is_empty() {
        bail!("{source} must not be empty");
    }
    if !path.is_absolute() {
        bail!("{source} must be an absolute path: {}", path.display());
    }
    Ok(path)
}

fn socket_at(path: PathBuf, source: &str) -> Result<PathBuf> {
    if path.file_name().is_none() {
        bail!("{source} must name a socket file, not {}", path.display());
    }
    if path.parent() == Some(Path::new("/")) {
        bail!(
            "{source} must not place the Fut socket directly in the filesystem root: {}",
            path.display()
        );
    }
    Ok(path)
}

fn runtime_at(value: &std::ffi::OsStr, source: &str) -> Result<PathBuf> {
    let directory = absolute_path(value, source)?;
    if directory == Path::new("/") {
        bail!("{source} must not be the filesystem root");
    }
    Ok(directory)
}

/// Resolve Fut's single per-user socket. An explicit path always wins.
pub fn socket_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return socket_at(absolute_path(path.as_os_str(), "--socket")?, "--socket");
    }
    if let Some(path) = env::var_os("FUT_SOCKET") {
        return socket_at(absolute_path(&path, "FUT_SOCKET")?, "FUT_SOCKET");
    }
    if let Some(directory) = env::var_os("FUT_RUNTIME_DIR") {
        return Ok(runtime_at(&directory, "FUT_RUNTIME_DIR")?.join("fut.sock"));
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    #[cfg(target_os = "linux")]
    let user_runtime = Some(PathBuf::from(format!("/run/user/{uid}")));
    #[cfg(not(target_os = "linux"))]
    let user_runtime: Option<PathBuf> = None;

    Ok(default_socket_path(
        env::var_os("XDG_RUNTIME_DIR").as_deref(),
        user_runtime.as_deref(),
        env::var_os("TMPDIR").as_deref(),
        uid,
    ))
}

fn default_socket_path(
    xdg_runtime: Option<&std::ffi::OsStr>,
    user_runtime: Option<&Path>,
    temporary: Option<&std::ffi::OsStr>,
    uid: u32,
) -> PathBuf {
    // The XDG Base Directory specification says empty or relative values are
    // invalid and must be ignored.
    if let Some(directory) = xdg_runtime
        && !directory.is_empty()
        && Path::new(directory).is_absolute()
    {
        return PathBuf::from(directory).join("fut/fut.sock");
    }
    // Noninteractive SSH sessions may omit XDG_RUNTIME_DIR even though the
    // user's login runtime directory exists. Resolve the same socket as a
    // normal login, but never trust a symlink or another user's directory.
    if let Some(directory) = user_runtime
        && let Ok(metadata) = fs::symlink_metadata(directory)
        && metadata.is_dir()
        && metadata.uid() == uid
        && metadata.permissions().mode() & 0o077 == 0
    {
        return directory.join("fut/fut.sock");
    }
    let temporary = temporary
        .filter(|path| !path.is_empty() && Path::new(path).is_absolute())
        .unwrap_or_else(|| std::ffi::OsStr::new("/tmp"));
    PathBuf::from(temporary).join(format!("fut-{uid}/fut.sock"))
}

pub fn runtime_dir(socket: &Path) -> Result<&Path> {
    socket
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| anyhow::anyhow!("socket path must have a parent directory"))
}

pub fn prepare_runtime_dir(socket: &Path) -> Result<()> {
    let directory = runtime_dir(socket)?;
    match fs::create_dir(directory) {
        Ok(()) => fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure runtime directory {}", directory.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(directory)
                .with_context(|| format!("inspect runtime directory {}", directory.display()))?;
            // SAFETY: geteuid has no preconditions and cannot fail.
            let euid = unsafe { libc::geteuid() };
            if !metadata.file_type().is_dir() {
                bail!("runtime path is not a directory: {}", directory.display());
            }
            if metadata.uid() != euid {
                bail!(
                    "runtime directory is not owned by the current user: {}",
                    directory.display()
                );
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                bail!(
                    "runtime directory has group or other permissions: {}",
                    directory.display()
                );
            }
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("create runtime directory {}", directory.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_without_valid_xdg_uses_existing_user_runtime() {
        let runtime = tempfile::tempdir().unwrap();
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let uid = fs::metadata(runtime.path()).unwrap().uid();

        for xdg in [None, Some("".as_ref()), Some("relative".as_ref())] {
            assert_eq!(
                default_socket_path(xdg, Some(runtime.path()), Some("/tmp".as_ref()), uid),
                runtime.path().join("fut/fut.sock")
            );
        }
        // Discovery must not create runtime state or require a running daemon.
        assert!(!runtime.path().join("fut").exists());
    }

    #[test]
    fn explicit_xdg_location_wins_over_user_runtime() {
        let runtime = tempfile::tempdir().unwrap();
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let uid = fs::metadata(runtime.path()).unwrap().uid();
        assert_eq!(
            default_socket_path(
                Some("/custom/runtime".as_ref()),
                Some(runtime.path()),
                None,
                uid
            ),
            PathBuf::from("/custom/runtime/fut/fut.sock")
        );
    }

    #[test]
    fn absent_user_runtime_preserves_temporary_fallbacks() {
        let runtime = tempfile::tempdir().unwrap();
        let missing = runtime.path().join("missing");
        for user_runtime in [None, Some(missing.as_path())] {
            assert_eq!(
                default_socket_path(None, user_runtime, Some("/custom/tmp".as_ref()), 1000),
                PathBuf::from("/custom/tmp/fut-1000/fut.sock")
            );
            for temporary in [None, Some("".as_ref()), Some("relative".as_ref())] {
                assert_eq!(
                    default_socket_path(None, user_runtime, temporary, 1000),
                    PathBuf::from("/tmp/fut-1000/fut.sock")
                );
            }
        }
    }

    #[test]
    fn discovered_user_runtime_must_be_private_and_owned_by_user() {
        let runtime = tempfile::tempdir().unwrap();
        let uid = fs::metadata(runtime.path()).unwrap().uid();
        let fallback = PathBuf::from(format!("/tmp/fut-{uid}/fut.sock"));

        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            default_socket_path(None, Some(runtime.path()), None, uid),
            fallback
        );

        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let other_uid = uid.wrapping_add(1);
        assert_eq!(
            default_socket_path(None, Some(runtime.path()), None, other_uid),
            PathBuf::from(format!("/tmp/fut-{other_uid}/fut.sock"))
        );
    }

    #[test]
    fn discovered_user_runtime_rejects_symlinks_and_files() {
        use std::os::unix::fs::symlink;

        let runtime = tempfile::tempdir().unwrap();
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let uid = fs::metadata(runtime.path()).unwrap().uid();
        let link = runtime.path().join("link");
        symlink(runtime.path(), &link).unwrap();
        let file = runtime.path().join("file");
        fs::write(&file, "").unwrap();
        for path in [&link, &file] {
            assert_eq!(
                default_socket_path(None, Some(path), None, uid),
                PathBuf::from(format!("/tmp/fut-{uid}/fut.sock"))
            );
        }
    }

    #[test]
    fn explicit_socket_is_unchanged() {
        assert_eq!(
            socket_path(Some(Path::new("/tmp/e2e/fut.sock"))).unwrap(),
            PathBuf::from("/tmp/e2e/fut.sock")
        );
    }

    #[test]
    fn explicit_socket_requires_an_absolute_file_path() {
        assert!(
            socket_path(Some(Path::new("relative.sock")))
                .unwrap_err()
                .to_string()
                .contains("--socket must be an absolute path")
        );
        assert!(
            socket_path(Some(Path::new("/")))
                .unwrap_err()
                .to_string()
                .contains("must name a socket file")
        );
        assert!(
            socket_at(PathBuf::from("/fut.sock"), "FUT_SOCKET")
                .unwrap_err()
                .to_string()
                .contains("filesystem root")
        );
    }

    #[test]
    fn runtime_directory_is_private() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("fut/fut.sock");
        prepare_runtime_dir(&socket).unwrap();
        assert_eq!(
            fs::metadata(socket.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn insecure_existing_parent_is_rejected_without_chmod() {
        let temporary = tempfile::tempdir().unwrap();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o755)).unwrap();

        let error = prepare_runtime_dir(&temporary.path().join("fut.sock")).unwrap_err();

        assert!(error.to_string().contains("group or other permissions"));
        assert_eq!(
            fs::metadata(temporary.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn symlink_runtime_directory_is_rejected() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = temporary.path().join("runtime");
        symlink(&target, &link).unwrap();

        assert!(prepare_runtime_dir(&link.join("fut.sock")).is_err());
    }
}
