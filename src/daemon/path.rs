use std::{
    env, fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

/// Resolve Fut's single per-user socket. An explicit path always wins.
pub fn socket_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = env::var_os("FUT_SOCKET") {
        return Ok(path.into());
    }
    if let Some(directory) = env::var_os("FUT_RUNTIME_DIR") {
        return Ok(PathBuf::from(directory).join("fut.sock"));
    }
    if let Some(directory) = env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(directory).join("fut/fut.sock"));
    }
    let temporary = env::var_os("TMPDIR").unwrap_or_else(|| "/tmp".into());
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    Ok(PathBuf::from(temporary).join(format!("fut-{uid}/fut.sock")))
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
    fn explicit_socket_is_unchanged() {
        assert_eq!(
            socket_path(Some(Path::new("/tmp/e2e/fut.sock"))).unwrap(),
            PathBuf::from("/tmp/e2e/fut.sock")
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
