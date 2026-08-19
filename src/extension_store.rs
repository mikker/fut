//! Fut-owned, local managed extension packages and enablement state.

use std::{
    collections::HashSet,
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    task::JoinHandle,
    time,
};
use uuid::Uuid;

use crate::extensions;

const INDEX_FILE_NAME: &str = "index.json";
const LOCK_FILE_NAME: &str = ".lock";
const INDEX_SCHEMA_VERSION: u8 = 1;
const MAX_INDEX_BYTES: u64 = 1024 * 1024;
const MAX_MANAGED_EXTENSIONS: usize = 32;
const MAX_PATH_BYTES: usize = 4096;
const MAX_VERSION_BYTES: usize = 128;
const MAX_REMOTE_URL_BYTES: usize = 4096;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_OUTPUT_LIMIT: usize = 256 * 1024;

pub(crate) const MAX_PACKAGE_FILES: usize = 1024;
pub(crate) const MAX_PACKAGE_ENTRIES: usize = 2048;
pub(crate) const MAX_PACKAGE_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_PACKAGE_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedExtension {
    pub(crate) id: String,
    pub(crate) version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provenance: Option<ExtensionProvenance>,
    pub(crate) content_sha256: String,
    pub(crate) install_path: PathBuf,
    pub(crate) enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ExtensionProvenance {
    Git { remote_url: String, commit: String },
}

impl ExtensionProvenance {
    fn git(&self) -> (&str, &str) {
        match self {
            Self::Git { remote_url, commit } => (remote_url, commit),
        }
    }
}

#[derive(Clone)]
struct InstallOrigin {
    source: Option<PathBuf>,
    provenance: Option<ExtensionProvenance>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoreChange {
    pub(crate) extension: ManagedExtension,
    pub(crate) changed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitUpdateChange {
    pub(crate) previous: ManagedExtension,
    pub(crate) current: StoreChange,
}

#[derive(Debug, Error)]
pub(crate) enum StoreMutationError {
    #[error("managed extension {id:?} is not installed")]
    NotFound { id: String },
    #[error("managed extension {id:?} is enabled; disable it before removing it")]
    Enabled { id: String },
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoreIndex {
    schema_version: u8,
    extensions: Vec<ManagedExtension>,
}

impl Default for StoreIndex {
    fn default() -> Self {
        Self {
            schema_version: INDEX_SCHEMA_VERSION,
            extensions: Vec::new(),
        }
    }
}

struct Store {
    root: PathBuf,
    _lock: StoreLock,
}

struct StoreLock {
    file: File,
}

impl StoreLock {
    fn acquire(root: &Path, operation: libc::c_int) -> Result<Self> {
        let path = root.join(LOCK_FILE_NAME);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| format!("open managed extension store lock {}", path.display()))?;
        if !file
            .metadata()
            .with_context(|| format!("inspect managed extension store lock {}", path.display()))?
            .file_type()
            .is_file()
        {
            bail!(
                "managed extension store lock {} is not a regular file",
                path.display()
            );
        }
        // SAFETY: `file` owns a valid descriptor for the lifetime of the lock.
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), operation) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("lock managed extension store {}", root.display()));
        }
        Ok(Self { file })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains valid until this value has been dropped.
        let _ = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.file), libc::LOCK_UN) };
    }
}

impl Store {
    fn open_for_write(root: &Path) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| {
            format!(
                "create managed extension store directory {}",
                root.display()
            )
        })?;
        let root = canonical_store_root(root)?;
        let lock = StoreLock::acquire(&root, libc::LOCK_EX)?;
        Ok(Self { root, _lock: lock })
    }

    fn open_existing(root: &Path) -> Result<Option<Self>> {
        Self::open_existing_with_lock(root, libc::LOCK_SH)
    }

    fn open_existing_for_write(root: &Path) -> Result<Option<Self>> {
        Self::open_existing_with_lock(root, libc::LOCK_EX)
    }

    fn open_existing_with_lock(root: &Path, operation: libc::c_int) -> Result<Option<Self>> {
        match fs::symlink_metadata(root) {
            Ok(metadata) if metadata.file_type().is_dir() || metadata.file_type().is_symlink() => {}
            Ok(_) => bail!(
                "managed extension store {} is not a directory",
                root.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect managed extension store {}", root.display())
                });
            }
        }
        let root = canonical_store_root(root)?;
        let lock = StoreLock::acquire(&root, operation)?;
        Ok(Some(Self { root, _lock: lock }))
    }

    fn read_index(&self) -> Result<StoreIndex> {
        read_index(&self.root)
    }

    fn write_index(&self, index: &StoreIndex) -> Result<()> {
        write_index(&self.root, index)
    }
}

/// Resolve the local managed store without creating it. A missing store or a
/// process without a usable data/home directory contributes no extensions.
pub(crate) fn enabled_roots() -> Result<Vec<PathBuf>> {
    let Some(configured_root) = default_store_root()? else {
        return Ok(Vec::new());
    };
    enabled_roots_at(&configured_root)
}

fn enabled_roots_at(configured_root: &Path) -> Result<Vec<PathBuf>> {
    let Some(store) = Store::open_existing(configured_root)? else {
        return Ok(Vec::new());
    };
    let index = store.read_index()?;
    let mut roots = Vec::new();
    for extension in index
        .extensions
        .iter()
        .filter(|extension| extension.enabled)
    {
        verify_installed_extension(&store.root, extension)
            .with_context(|| format!("verify enabled managed extension {:?}", extension.id))?;
        roots.push(extension.install_path.clone());
    }
    Ok(roots)
}

/// Copy and validate a local package without executing any package content.
/// A newly installed package starts disabled; reinstalling an ID preserves its
/// previous enabled state while atomically moving the index to new content.
pub(crate) fn install(configured_source: &Path) -> Result<StoreChange> {
    let configured_store = default_store_root()?
        .context("cannot resolve managed extension store; set absolute XDG_DATA_HOME or HOME")?;
    install_at(configured_source, &configured_store)
}

fn install_at(configured_source: &Path, configured_store: &Path) -> Result<StoreChange> {
    let source = prepare_source(configured_source)?;
    if configured_store.starts_with(&source) {
        bail!(
            "managed extension store {} cannot be inside source package {}",
            configured_store.display(),
            source.display()
        );
    }
    let origin = InstallOrigin {
        source: Some(source.clone()),
        provenance: None,
    };
    install_prepared_at(&source, configured_store, origin, None, None)
}

fn install_prepared_at(
    source: &Path,
    configured_store: &Path,
    origin: InstallOrigin,
    expected_digest: Option<&str>,
    expected_previous: Option<&ManagedExtension>,
) -> Result<StoreChange> {
    let store = Store::open_for_write(configured_store)?;
    let mut index = store.read_index()?;
    let staging = store.root.join(format!(".install-{}", Uuid::new_v4()));
    fs::create_dir(&staging)
        .with_context(|| format!("create extension staging directory {}", staging.display()))?;

    let staged_result = (|| -> Result<(ManagedExtension, Option<ManagedExtension>, bool)> {
        copy_package(source, &staging)?;
        make_tree_read_only(&staging)?;
        let staged = extensions::validate_package(&staging)
            .with_context(|| format!("validate staged extension {}", staging.display()))?;
        if staged.version.len() > MAX_VERSION_BYTES {
            bail!(
                "extension {:?} version exceeds {MAX_VERSION_BYTES} bytes",
                staged.id
            );
        }
        let previous = index
            .extensions
            .iter()
            .find(|extension| extension.id == staged.id)
            .cloned();
        if let Some(expected) = expected_previous {
            if staged.id != expected.id {
                bail!(
                    "Git update for extension {:?} fetched package {:?}",
                    expected.id,
                    staged.id
                );
            }
            if previous.as_ref() != Some(expected) {
                bail!(
                    "managed extension {:?} changed while its Git update was being staged",
                    expected.id
                );
            }
        }
        if previous.is_none() && index.extensions.len() >= MAX_MANAGED_EXTENSIONS {
            bail!(
                "managed extension store contains {} extensions; maximum is {MAX_MANAGED_EXTENSIONS}",
                index.extensions.len()
            );
        }
        let digest = hash_package(&staging)?;
        if expected_digest.is_some_and(|expected| expected != digest) {
            bail!(
                "extension content SHA-256 mismatch: expected {}, got {digest}",
                expected_digest.expect("checked expected digest")
            );
        }
        let version_parent = ensure_package_parent(&store.root, &staged.id, &staged.version)?;
        let install_path = version_parent.join(&digest);
        let metadata = ManagedExtension {
            id: staged.id,
            version: staged.version,
            source: origin.source.clone(),
            provenance: origin.provenance.clone(),
            content_sha256: digest,
            install_path: install_path.clone(),
            enabled: previous.as_ref().is_some_and(|extension| extension.enabled),
        };

        let created = match fs::symlink_metadata(&install_path) {
            Ok(_) => {
                verify_installed_extension(&store.root, &metadata).with_context(|| {
                    format!(
                        "reuse existing immutable package {}",
                        install_path.display()
                    )
                })?;
                false
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // macOS requires write permission on a directory when moving
                // it between parents so it can update the directory's `..`
                // entry. The package hash excludes its root mode; restore the
                // immutable mode immediately after the atomic rename.
                fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).with_context(
                    || {
                        format!(
                            "prepare staged extension for atomic rename {}",
                            staging.display()
                        )
                    },
                )?;
                fs::rename(&staging, &install_path).with_context(|| {
                    format!(
                        "atomically install staged extension {} at {}",
                        staging.display(),
                        install_path.display()
                    )
                })?;
                if let Err(error) =
                    fs::set_permissions(&install_path, fs::Permissions::from_mode(0o555))
                {
                    let _ = remove_tree(&install_path);
                    return Err(error).with_context(|| {
                        format!(
                            "make installed extension root read-only {}",
                            install_path.display()
                        )
                    });
                }
                true
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect extension install path {}", install_path.display())
                });
            }
        };
        Ok((metadata, previous, created))
    })();

    let (metadata, previous, created) = match staged_result {
        Ok(result) => result,
        Err(error) => {
            let _ = remove_tree(&staging);
            return Err(error);
        }
    };
    if !created {
        remove_tree(&staging).with_context(|| {
            format!("remove reused extension staging tree {}", staging.display())
        })?;
    }

    let changed = previous.as_ref() != Some(&metadata);
    if !changed && !created {
        return Ok(StoreChange {
            extension: metadata,
            changed: false,
        });
    }
    index
        .extensions
        .retain(|extension| extension.id != metadata.id);
    index.extensions.push(metadata.clone());
    index
        .extensions
        .sort_by(|left, right| left.id.cmp(&right.id));
    if let Err(error) = store.write_index(&index) {
        if created {
            let _ = remove_tree(&metadata.install_path);
        }
        return Err(error.context(
            "update managed extension index; the previously installed package remains selected",
        ));
    }

    // Keep superseded immutable bytes available: a running daemon may still
    // hold the previous catalog generation and launch its captured paths until
    // the user reloads it. A future store GC can remove packages after proving
    // that no live daemon catalog references them.
    Ok(StoreChange {
        extension: metadata,
        changed,
    })
}

/// Fetch one exact Git commit into an isolated checkout and pass its files
/// through the same bounded package installer used by local sources.
pub(crate) async fn install_git(
    remote_url: &str,
    revision: &str,
    expected_digest: Option<&str>,
) -> Result<StoreChange> {
    validate_remote_url(remote_url)?;
    let commit = normalize_commit(revision)?;
    if let Some(digest) = expected_digest {
        validate_digest(digest).context("invalid expected extension content SHA-256")?;
    }
    let configured_store = default_store_root()?
        .context("cannot resolve managed extension store; set absolute XDG_DATA_HOME or HOME")?;
    let checkout = acquire_git_checkout(remote_url, &commit).await?;
    let source = prepare_source(checkout.path())?;
    install_prepared_at(
        &source,
        &configured_store,
        InstallOrigin {
            source: None,
            provenance: Some(ExtensionProvenance::Git {
                remote_url: remote_url.to_owned(),
                commit,
            }),
        },
        expected_digest,
        None,
    )
}

/// Update only an existing Git-sourced extension, using its recorded remote
/// and a newly supplied exact commit. The index is checked again after the
/// fetch so a concurrent mutation cannot be overwritten.
pub(crate) async fn update_git(
    id: &str,
    revision: &str,
    expected_digest: Option<&str>,
) -> Result<GitUpdateChange> {
    validate_id(id)?;
    let commit = normalize_commit(revision)?;
    if let Some(digest) = expected_digest {
        validate_digest(digest).context("invalid expected extension content SHA-256")?;
    }
    let configured_store = default_store_root()?
        .context("cannot resolve managed extension store; set absolute XDG_DATA_HOME or HOME")?;
    let existing = {
        let store = Store::open_existing(&configured_store)?
            .with_context(|| format!("managed extension {id:?} is not installed"))?;
        store
            .read_index()?
            .extensions
            .into_iter()
            .find(|extension| extension.id == id)
            .with_context(|| format!("managed extension {id:?} is not installed"))?
    };
    let (remote_url, previous_commit) = existing
        .provenance
        .as_ref()
        .map(ExtensionProvenance::git)
        .with_context(|| {
            format!(
                "managed extension {id:?} was installed from a local path and cannot be updated from Git"
            )
        })?;
    if commit == previous_commit {
        bail!(
            "Git update for extension {id:?} must supply a commit different from its installed commit {previous_commit}"
        );
    }
    let remote_url = remote_url.to_owned();
    let checkout = acquire_git_checkout(&remote_url, &commit).await?;
    let source = prepare_source(checkout.path())?;
    let current = install_prepared_at(
        &source,
        &configured_store,
        InstallOrigin {
            source: None,
            provenance: Some(ExtensionProvenance::Git { remote_url, commit }),
        },
        expected_digest,
        Some(&existing),
    )?;
    Ok(GitUpdateChange {
        previous: existing,
        current,
    })
}

async fn acquire_git_checkout(remote_url: &str, commit: &str) -> Result<tempfile::TempDir> {
    let checkout = tempfile::Builder::new()
        .prefix("fut-extension-git-")
        .tempdir()
        .context("create temporary Git extension checkout")?;
    let init_arguments = if commit.len() == 64 {
        vec!["init", "--quiet", "--object-format=sha256"]
    } else {
        vec!["init", "--quiet"]
    };
    git_success(
        run_git(checkout.path(), &init_arguments).await?,
        "initialize temporary repository",
    )?;
    git_success(
        run_git(
            checkout.path(),
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--depth=1",
                "--no-recurse-submodules",
                "--",
                remote_url,
                commit,
            ],
        )
        .await?,
        "fetch pinned extension commit",
    )?;

    let peeled = format!("{commit}^{{commit}}");
    let resolved = git_success(
        run_git(checkout.path(), &["rev-parse", "--verify", &peeled]).await?,
        "verify fetched extension commit",
    )?;
    let resolved = required_git_line(&resolved.stdout, "resolved commit")?;
    if resolved != commit.as_bytes() {
        bail!(
            "fetched Git object did not resolve to requested commit {commit} (got {})",
            String::from_utf8_lossy(resolved)
        );
    }

    let tree = git_success(
        run_git(
            checkout.path(),
            &[
                "ls-tree",
                "-r",
                "-t",
                "--format=%(objectmode) %(objectsize)",
                commit,
            ],
        )
        .await?,
        "inspect extension tree",
    )?;
    inspect_git_tree(&tree.stdout, commit)?;

    git_success(
        run_git(
            checkout.path(),
            &[
                "checkout",
                "--quiet",
                "--detach",
                "--force",
                "--no-recurse-submodules",
                commit,
            ],
        )
        .await?,
        "check out pinned extension commit",
    )?;
    remove_tree(&checkout.path().join(".git"))
        .context("remove temporary Git metadata before package installation")?;
    normalize_checkout_permissions(checkout.path())?;
    Ok(checkout)
}

fn inspect_git_tree(output: &[u8], commit: &str) -> Result<()> {
    let mut entries = 0_usize;
    let mut files = 0_usize;
    let mut bytes = 0_u64;
    for line in output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        entries += 1;
        if entries > MAX_PACKAGE_ENTRIES {
            bail!(
                "Git extension commit {commit} contains more than {MAX_PACKAGE_ENTRIES} filesystem entries"
            );
        }
        let separator = line
            .iter()
            .position(|byte| *byte == b' ')
            .with_context(|| format!("Git returned an invalid tree entry for commit {commit}"))?;
        let (mode, size) = (&line[..separator], &line[separator + 1..]);
        if mode == b"160000" {
            bail!(
                "Git extension commit {commit} contains a submodule; submodules are not installed"
            );
        }
        if mode == b"040000" {
            continue;
        }
        if !matches!(mode, b"100644" | b"100755" | b"120000") {
            bail!(
                "Git extension commit {commit} contains unsupported tree mode {}",
                String::from_utf8_lossy(mode)
            );
        }
        files += 1;
        if files > MAX_PACKAGE_FILES {
            bail!("Git extension commit {commit} contains more than {MAX_PACKAGE_FILES} files");
        }
        let size = std::str::from_utf8(size)
            .context("Git returned a non-UTF-8 tree object size")?
            .parse::<u64>()
            .context("Git returned an invalid tree object size")?;
        if size > MAX_PACKAGE_FILE_BYTES {
            bail!(
                "Git extension commit {commit} contains a {size}-byte file; maximum is {MAX_PACKAGE_FILE_BYTES}"
            );
        }
        bytes = bytes
            .checked_add(size)
            .context("Git extension package total size overflow")?;
        if bytes > MAX_PACKAGE_TOTAL_BYTES {
            bail!(
                "Git extension commit {commit} contents exceed the {MAX_PACKAGE_TOTAL_BYTES}-byte total maximum"
            );
        }
    }
    Ok(())
}

fn normalize_checkout_permissions(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect Git checkout entry {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.file_type().is_dir() {
        for entry in fs::read_dir(path)
            .with_context(|| format!("read Git checkout directory {}", path.display()))?
        {
            normalize_checkout_permissions(&entry?.path())?;
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("normalize Git checkout directory {}", path.display()))?;
    } else if metadata.file_type().is_file() {
        let mode = if metadata.permissions().mode() & 0o111 == 0 {
            0o644
        } else {
            0o755
        };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("normalize Git checkout file {}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn enable(id: &str) -> std::result::Result<StoreChange, StoreMutationError> {
    set_enabled(id, true)
}

pub(crate) fn disable(id: &str) -> std::result::Result<StoreChange, StoreMutationError> {
    set_enabled(id, false)
}

fn set_enabled(id: &str, enabled: bool) -> std::result::Result<StoreChange, StoreMutationError> {
    validate_id(id)?;
    let configured_root =
        default_store_root()?.ok_or_else(|| StoreMutationError::NotFound { id: id.to_owned() })?;
    set_enabled_at(&configured_root, id, enabled)
}

fn set_enabled_at(
    configured_root: &Path,
    id: &str,
    enabled: bool,
) -> std::result::Result<StoreChange, StoreMutationError> {
    validate_id(id)?;
    let store = match Store::open_existing_for_write(configured_root)? {
        Some(store) => store,
        None => return Err(StoreMutationError::NotFound { id: id.to_owned() }),
    };
    let mut index = store.read_index()?;
    let extension = index
        .extensions
        .iter_mut()
        .find(|extension| extension.id == id)
        .ok_or_else(|| StoreMutationError::NotFound { id: id.to_owned() })?;
    if enabled {
        verify_installed_extension(&store.root, extension)
            .with_context(|| format!("verify managed extension {id:?} before enabling"))?;
    }
    let changed = extension.enabled != enabled;
    extension.enabled = enabled;
    let extension = extension.clone();
    if changed {
        store.write_index(&index)?;
    }
    Ok(StoreChange { extension, changed })
}

pub(crate) fn remove(id: &str) -> std::result::Result<StoreChange, StoreMutationError> {
    validate_id(id)?;
    let configured_root =
        default_store_root()?.ok_or_else(|| StoreMutationError::NotFound { id: id.to_owned() })?;
    remove_at(&configured_root, id)
}

fn remove_at(
    configured_root: &Path,
    id: &str,
) -> std::result::Result<StoreChange, StoreMutationError> {
    validate_id(id)?;
    let store = Store::open_existing_for_write(configured_root)?
        .ok_or_else(|| StoreMutationError::NotFound { id: id.to_owned() })?;
    let mut index = store.read_index()?;
    let position = index
        .extensions
        .iter()
        .position(|extension| extension.id == id)
        .ok_or_else(|| StoreMutationError::NotFound { id: id.to_owned() })?;
    let extension = index.extensions[position].clone();
    if extension.enabled {
        return Err(StoreMutationError::Enabled { id: id.to_owned() });
    }

    verify_installed_extension(&store.root, &extension)
        .with_context(|| format!("verify managed extension {id:?} before removal"))?;
    index.extensions.remove(position);
    store.write_index(&index)?;
    // Unindexing is immediate, but immutable content remains so a running
    // daemon's older catalog cannot be broken by a daemonless store mutation.
    Ok(StoreChange {
        extension,
        changed: true,
    })
}

fn default_store_root() -> Result<Option<PathBuf>> {
    if let Some(directory) = env::var_os("XDG_DATA_HOME") {
        let directory = PathBuf::from(directory);
        if directory.is_absolute() {
            validate_path("XDG_DATA_HOME", &directory)?;
            return Ok(Some(directory.join("fut/extensions")));
        }
    }
    let Some(home) = env::var_os("HOME") else {
        return Ok(None);
    };
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        bail!("HOME must be absolute when resolving the managed extension store");
    }
    validate_path("HOME", &home)?;
    Ok(Some(home.join(".local/share/fut/extensions")))
}

fn canonical_store_root(root: &Path) -> Result<PathBuf> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("resolve managed extension store {}", root.display()))?;
    validate_path("managed extension store path", &root)?;
    if !fs::metadata(&root)
        .with_context(|| format!("inspect managed extension store {}", root.display()))?
        .is_dir()
    {
        bail!(
            "managed extension store {} is not a directory",
            root.display()
        );
    }
    Ok(root)
}

fn ensure_package_parent(root: &Path, id: &str, version: &str) -> Result<PathBuf> {
    let mut current = root.to_owned();
    for component in ["packages", id, version] {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => bail!(
                "managed extension package parent {} is not a real directory",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).with_context(|| {
                    format!(
                        "create managed extension package directory {}",
                        current.display()
                    )
                })?;
                fs::set_permissions(&current, fs::Permissions::from_mode(0o700)).with_context(
                    || {
                        format!(
                            "secure managed extension package directory {}",
                            current.display()
                        )
                    },
                )?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspect managed extension package directory {}",
                        current.display()
                    )
                });
            }
        }
    }
    Ok(current)
}

fn prepare_source(configured_source: &Path) -> Result<PathBuf> {
    let absolute = if configured_source.is_absolute() {
        configured_source.to_owned()
    } else {
        env::current_dir()
            .context("read current directory for extension installation")?
            .join(configured_source)
    };
    let metadata = fs::symlink_metadata(&absolute)
        .with_context(|| format!("inspect extension source {}", absolute.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "extension source {} is a symbolic link; install requires a real directory",
            absolute.display()
        );
    }
    if !metadata.file_type().is_dir() {
        bail!("extension source {} is not a directory", absolute.display());
    }
    let source = fs::canonicalize(&absolute)
        .with_context(|| format!("resolve extension source {}", absolute.display()))?;
    validate_path("canonical extension source", &source)?;
    Ok(source)
}

fn read_index(root: &Path) -> Result<StoreIndex> {
    let path = root.join(INDEX_FILE_NAME);
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StoreIndex::default());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("open managed extension index {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect managed extension index {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "managed extension index {} is not a regular file",
            path.display()
        );
    }
    if metadata.len() > MAX_INDEX_BYTES {
        bail!(
            "managed extension index {} is {} bytes; maximum is {MAX_INDEX_BYTES}",
            path.display(),
            metadata.len()
        );
    }
    let mut bytes = Vec::new();
    file.take(MAX_INDEX_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read managed extension index {}", path.display()))?;
    if bytes.len() as u64 > MAX_INDEX_BYTES {
        bail!(
            "managed extension index {} exceeds the {MAX_INDEX_BYTES}-byte maximum",
            path.display()
        );
    }
    let index = serde_json::from_slice::<StoreIndex>(&bytes)
        .with_context(|| format!("parse managed extension index {}", path.display()))?;
    validate_index(root, &index)?;
    Ok(index)
}

fn validate_index(root: &Path, index: &StoreIndex) -> Result<()> {
    if index.schema_version != INDEX_SCHEMA_VERSION {
        bail!(
            "managed extension index schema_version {} is unsupported; expected {INDEX_SCHEMA_VERSION}",
            index.schema_version
        );
    }
    if index.extensions.len() > MAX_MANAGED_EXTENSIONS {
        bail!(
            "managed extension index contains {} extensions; maximum is {MAX_MANAGED_EXTENSIONS}",
            index.extensions.len()
        );
    }
    let mut ids = HashSet::new();
    let mut previous_id: Option<&str> = None;
    for extension in &index.extensions {
        validate_id(&extension.id)?;
        if !ids.insert(extension.id.as_str()) {
            bail!(
                "managed extension index repeats extension ID {:?}",
                extension.id
            );
        }
        if previous_id.is_some_and(|previous| previous >= extension.id.as_str()) {
            bail!("managed extension index entries must be sorted by ID");
        }
        previous_id = Some(&extension.id);
        Version::parse(&extension.version).with_context(|| {
            format!(
                "managed extension {:?} has invalid version {:?}",
                extension.id, extension.version
            )
        })?;
        if extension.version.len() > MAX_VERSION_BYTES {
            bail!(
                "managed extension {:?} version exceeds {MAX_VERSION_BYTES} bytes",
                extension.id
            );
        }
        validate_digest(&extension.content_sha256).with_context(|| {
            format!(
                "managed extension {:?} has invalid content_sha256",
                extension.id
            )
        })?;
        if extension.source.is_some() == extension.provenance.is_some() {
            bail!(
                "managed extension {:?} must have exactly one local source or remote provenance",
                extension.id
            );
        }
        if let Some(source) = &extension.source {
            validate_path("managed extension source", source)?;
            if !source.is_absolute() {
                bail!(
                    "managed extension {:?} source must be absolute",
                    extension.id
                );
            }
        }
        if let Some(provenance) = &extension.provenance {
            let (remote_url, commit) = provenance.git();
            validate_remote_url(remote_url).with_context(|| {
                format!(
                    "managed extension {:?} has invalid Git remote URL",
                    extension.id
                )
            })?;
            validate_commit(commit).with_context(|| {
                format!(
                    "managed extension {:?} has invalid Git commit",
                    extension.id
                )
            })?;
        }
        validate_path("managed extension install_path", &extension.install_path)?;
        if !extension.install_path.is_absolute() {
            bail!(
                "managed extension {:?} install_path must be absolute",
                extension.id
            );
        }
        let expected = root
            .join("packages")
            .join(&extension.id)
            .join(&extension.version)
            .join(&extension.content_sha256);
        if extension.install_path != expected {
            bail!(
                "managed extension {:?} install_path {} does not match its immutable store path {}",
                extension.id,
                extension.install_path.display(),
                expected.display()
            );
        }
    }
    Ok(())
}

fn write_index(root: &Path, index: &StoreIndex) -> Result<()> {
    validate_index(root, index)?;
    let mut bytes =
        serde_json::to_vec_pretty(index).context("serialize managed extension index")?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_INDEX_BYTES {
        bail!("managed extension index exceeds the {MAX_INDEX_BYTES}-byte maximum");
    }
    let path = root.join(INDEX_FILE_NAME);
    let temporary = root.join(format!(".index-{}.tmp", Uuid::new_v4()));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temporary)
            .with_context(|| {
                format!(
                    "create managed extension index staging file {}",
                    temporary.display()
                )
            })?;
        file.write_all(&bytes).with_context(|| {
            format!(
                "write managed extension index staging file {}",
                temporary.display()
            )
        })?;
        file.sync_all().with_context(|| {
            format!(
                "sync managed extension index staging file {}",
                temporary.display()
            )
        })?;
        fs::rename(&temporary, &path).with_context(|| {
            format!(
                "atomically replace managed extension index {}",
                path.display()
            )
        })?;
        File::open(root)
            .with_context(|| format!("open managed extension store {} for sync", root.display()))?
            .sync_all()
            .with_context(|| format!("sync managed extension store {}", root.display()))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn verify_installed_extension(root: &Path, extension: &ManagedExtension) -> Result<()> {
    let expected = root
        .join("packages")
        .join(&extension.id)
        .join(&extension.version)
        .join(&extension.content_sha256);
    if extension.install_path != expected {
        bail!(
            "managed extension {:?} has an unexpected install path {}",
            extension.id,
            extension.install_path.display()
        );
    }
    let metadata = fs::symlink_metadata(&extension.install_path).with_context(|| {
        format!(
            "inspect managed extension install path {}",
            extension.install_path.display()
        )
    })?;
    if !metadata.file_type().is_dir() {
        bail!(
            "managed extension install path {} is not a real directory",
            extension.install_path.display()
        );
    }
    let canonical = fs::canonicalize(&extension.install_path).with_context(|| {
        format!(
            "resolve managed extension install path {}",
            extension.install_path.display()
        )
    })?;
    if canonical != extension.install_path {
        bail!(
            "managed extension install path {} resolves outside its immutable location",
            extension.install_path.display()
        );
    }
    let actual_digest = hash_package(&extension.install_path)?;
    if actual_digest != extension.content_sha256 {
        bail!(
            "managed extension {:?} content hash mismatch: index has {}, package has {}",
            extension.id,
            extension.content_sha256,
            actual_digest
        );
    }
    let declaration = extensions::validate_package(&extension.install_path)?;
    if declaration.id != extension.id || declaration.version != extension.version {
        bail!(
            "managed extension metadata does not match its manifest (index {} {}, manifest {} {})",
            extension.id,
            extension.version,
            declaration.id,
            declaration.version
        );
    }
    Ok(())
}

#[derive(Default)]
struct PackageStats {
    entries: usize,
    files: usize,
    bytes: u64,
}

impl PackageStats {
    fn add_entry(&mut self, path: &Path) -> Result<()> {
        self.entries += 1;
        if self.entries > MAX_PACKAGE_ENTRIES {
            bail!(
                "extension package contains more than {MAX_PACKAGE_ENTRIES} filesystem entries (at {})",
                path.display()
            );
        }
        Ok(())
    }

    fn add_file(&mut self, path: &Path, bytes: u64) -> Result<()> {
        self.files += 1;
        if self.files > MAX_PACKAGE_FILES {
            bail!(
                "extension package contains more than {MAX_PACKAGE_FILES} files (at {})",
                path.display()
            );
        }
        if bytes > MAX_PACKAGE_FILE_BYTES {
            bail!(
                "extension package file {} is {bytes} bytes; maximum is {MAX_PACKAGE_FILE_BYTES}",
                path.display()
            );
        }
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .context("extension package total size overflow")?;
        if self.bytes > MAX_PACKAGE_TOTAL_BYTES {
            bail!(
                "extension package contents exceed the {MAX_PACKAGE_TOTAL_BYTES}-byte total maximum"
            );
        }
        Ok(())
    }
}

fn copy_package(source: &Path, destination: &Path) -> Result<()> {
    let mut stats = PackageStats::default();
    copy_directory(source, destination, &mut stats)
}

fn copy_directory(source: &Path, destination: &Path, stats: &mut PackageStats) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("inspect extension package directory {}", source.display()))?;
    if !metadata.file_type().is_dir() {
        bail!(
            "extension package entry {} is not a real directory",
            source.display()
        );
    }
    let mut entries = fs::read_dir(source)
        .with_context(|| format!("read extension package directory {}", source.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by(|left, right| {
        left.file_name()
            .as_bytes()
            .cmp(right.file_name().as_bytes())
    });
    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path).with_context(|| {
            format!("inspect extension package entry {}", source_path.display())
        })?;
        stats.add_entry(&source_path)?;
        if metadata.file_type().is_symlink() {
            bail!(
                "extension package contains symbolic link {}; symbolic links are not installed",
                source_path.display()
            );
        }
        if metadata.file_type().is_dir() {
            fs::create_dir(&destination_path).with_context(|| {
                format!(
                    "create staged extension directory {}",
                    destination_path.display()
                )
            })?;
            copy_directory(&source_path, &destination_path, stats)?;
            let mode = (metadata.permissions().mode() & 0o777) | 0o500;
            fs::set_permissions(&destination_path, fs::Permissions::from_mode(mode)).with_context(
                || {
                    format!(
                        "set staged extension directory permissions {}",
                        destination_path.display()
                    )
                },
            )?;
        } else if metadata.file_type().is_file() {
            stats.add_file(&source_path, metadata.len())?;
            copy_file(&source_path, &destination_path, &metadata)?;
        } else {
            bail!(
                "extension package contains special file {}; only directories and regular files are accepted",
                source_path.display()
            );
        }
    }
    Ok(())
}

fn copy_file(source: &Path, destination: &Path, expected: &fs::Metadata) -> Result<()> {
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(source)
        .with_context(|| format!("open extension package file {}", source.display()))?;
    let opened = input
        .metadata()
        .with_context(|| format!("inspect opened extension package file {}", source.display()))?;
    if !opened.file_type().is_file() || opened.len() != expected.len() {
        bail!(
            "extension package file {} changed while being copied",
            source.display()
        );
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(destination)
        .with_context(|| format!("create staged extension file {}", destination.display()))?;
    let copied = std::io::copy(
        &mut std::io::Read::by_ref(&mut input).take(MAX_PACKAGE_FILE_BYTES + 1),
        &mut output,
    )
    .with_context(|| {
        format!(
            "copy extension package file {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    if copied != expected.len() || copied > MAX_PACKAGE_FILE_BYTES {
        bail!(
            "extension package file {} changed size while being copied",
            source.display()
        );
    }
    output
        .sync_all()
        .with_context(|| format!("sync staged extension file {}", destination.display()))?;
    let mode = (expected.permissions().mode() & 0o777) | 0o400;
    fs::set_permissions(destination, fs::Permissions::from_mode(mode)).with_context(|| {
        format!(
            "set staged extension file permissions {}",
            destination.display()
        )
    })?;
    Ok(())
}

fn make_tree_read_only(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect staged extension entry {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "staged extension unexpectedly contains symbolic link {}",
            path.display()
        );
    }
    if metadata.file_type().is_dir() {
        for entry in fs::read_dir(path)
            .with_context(|| format!("read staged extension directory {}", path.display()))?
        {
            make_tree_read_only(&entry?.path())?;
        }
    } else if !metadata.file_type().is_file() {
        bail!(
            "staged extension unexpectedly contains special file {}",
            path.display()
        );
    }
    let mode = metadata.permissions().mode() & 0o555;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("make staged extension entry read-only {}", path.display()))
}

fn hash_package(root: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"fut-extension-package-v1\0");
    let mut stats = PackageStats::default();
    hash_directory(root, root, &mut stats, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_directory(
    root: &Path,
    directory: &Path,
    stats: &mut PackageStats,
    hasher: &mut Sha256,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("read extension package directory {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by(|left, right| {
        left.file_name()
            .as_bytes()
            .cmp(right.file_name().as_bytes())
    });
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect extension package entry {}", path.display()))?;
        stats.add_entry(&path)?;
        let relative = path
            .strip_prefix(root)
            .expect("walked package entries stay beneath their root");
        if metadata.file_type().is_symlink() {
            bail!(
                "extension package contains symbolic link {}; symbolic links are not accepted",
                path.display()
            );
        }
        if metadata.file_type().is_dir() {
            hash_entry_header(hasher, b'd', relative, &metadata);
            hash_directory(root, &path, stats, hasher)?;
        } else if metadata.file_type().is_file() {
            stats.add_file(&path, metadata.len())?;
            hash_entry_header(hasher, b'f', relative, &metadata);
            hasher.update(metadata.len().to_le_bytes());
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW)
                .open(&path)
                .with_context(|| format!("open extension package file {}", path.display()))?;
            let opened = file
                .metadata()
                .with_context(|| format!("inspect extension package file {}", path.display()))?;
            if !opened.file_type().is_file() || opened.len() != metadata.len() {
                bail!(
                    "extension package file {} changed while hashing",
                    path.display()
                );
            }
            let mut read = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = file
                    .read(&mut buffer)
                    .with_context(|| format!("hash extension package file {}", path.display()))?;
                if count == 0 {
                    break;
                }
                read += count as u64;
                if read > metadata.len() || read > MAX_PACKAGE_FILE_BYTES {
                    bail!(
                        "extension package file {} changed while hashing",
                        path.display()
                    );
                }
                hasher.update(&buffer[..count]);
            }
            if read != metadata.len() {
                bail!(
                    "extension package file {} changed while hashing",
                    path.display()
                );
            }
        } else {
            bail!(
                "extension package contains special file {}; only directories and regular files are accepted",
                path.display()
            );
        }
    }
    Ok(())
}

fn hash_entry_header(hasher: &mut Sha256, kind: u8, path: &Path, metadata: &fs::Metadata) {
    let bytes = path.as_os_str().as_bytes();
    hasher.update([kind]);
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
    hasher.update((metadata.permissions().mode() & 0o777).to_le_bytes());
}

fn remove_tree(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect removal path {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || metadata.file_type().is_file() {
        fs::remove_file(path).with_context(|| format!("remove file {}", path.display()))?;
        return Ok(());
    }
    if !metadata.file_type().is_dir() {
        bail!(
            "refuse to recursively remove special file {}",
            path.display()
        );
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("make removal directory writable {}", path.display()))?;
    for entry in
        fs::read_dir(path).with_context(|| format!("read removal directory {}", path.display()))?
    {
        remove_tree(&entry?.path())?;
    }
    fs::remove_dir(path).with_context(|| format!("remove directory {}", path.display()))
}

fn validate_id(id: &str) -> Result<()> {
    extensions::validate_identifier("extension ID", id)
}

fn validate_remote_url(remote_url: &str) -> Result<()> {
    if remote_url.is_empty() || remote_url.len() > MAX_REMOTE_URL_BYTES {
        bail!("Git remote URL must be 1 through {MAX_REMOTE_URL_BYTES} bytes");
    }
    if remote_url.bytes().any(|byte| byte.is_ascii_control()) {
        bail!("Git remote URL must not contain control characters");
    }
    if !(remote_url.starts_with("https://") || remote_url.starts_with("file:///")) {
        bail!("Git remote URL must use https:// or an absolute file:/// URL");
    }
    if remote_url.contains(['?', '#']) {
        bail!("Git remote URL must not contain a query or fragment");
    }
    if let Some(authority) = remote_url
        .strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        && authority.contains('@')
    {
        bail!("Git remote URL must not embed credentials");
    }
    Ok(())
}

fn normalize_commit(revision: &str) -> Result<String> {
    if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("Git revision must be an exact full 40- or 64-character hexadecimal commit SHA");
    }
    Ok(revision.to_ascii_lowercase())
}

fn validate_commit(commit: &str) -> Result<()> {
    if !matches!(commit.len(), 40 | 64)
        || !commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        bail!("Git commit must be a canonical full lowercase hexadecimal SHA");
    }
    Ok(())
}

#[derive(Debug)]
struct GitOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn run_git(cwd: &Path, arguments: &[&str]) -> Result<GitOutput> {
    run_git_with(OsStr::new("git"), GIT_COMMAND_TIMEOUT, cwd, arguments).await
}

async fn run_git_with(
    git_executable: &OsStr,
    command_timeout: Duration,
    cwd: &Path,
    arguments: &[&str],
) -> Result<GitOutput> {
    let mut command = Command::new(git_executable);
    sanitize_git_environment(&mut command, env::vars_os());
    command
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(["-c", "credential.helper="])
        .args(["-c", "filter.lfs.smudge="])
        .args(["-c", "filter.lfs.required=false"])
        .args(arguments)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("GIT_ALLOW_PROTOCOL", "https:file")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_ATTRIBUTES_FILE", null_device())
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PROTOCOL_FROM_USER", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => anyhow::anyhow!("Git executable was not found"),
        _ => anyhow::anyhow!(error).context("start bounded Git command"),
    })?;
    let process_group = child.id();
    let stdout = child.stdout.take().expect("piped Git stdout");
    let stderr = child.stderr.take().expect("piped Git stderr");
    let mut stdout = tokio::spawn(read_git_output(stdout));
    let mut stderr = tokio::spawn(read_git_output(stderr));
    let result = time::timeout(command_timeout, async {
        let status = child.wait().await.context("wait for Git command")?;
        let stdout = (&mut stdout)
            .await
            .context("Git stdout reader stopped unexpectedly")??;
        let stderr = (&mut stderr)
            .await
            .context("Git stderr reader stopped unexpectedly")??;
        Ok(GitOutput {
            status,
            stdout,
            stderr,
        })
    })
    .await;
    match result {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => {
            clean_up_git(&mut child, process_group, &mut stdout, &mut stderr).await;
            Err(error)
        }
        Err(_) => {
            clean_up_git(&mut child, process_group, &mut stdout, &mut stderr).await;
            bail!("Git command timed out after {command_timeout:?}")
        }
    }
}

fn sanitize_git_environment(
    command: &mut Command,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) {
    const GIT_ENVIRONMENT: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_INDEX_VERSION",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_EXEC_PATH",
        "GIT_TEMPLATE_DIR",
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_ATTRIBUTES_FILE",
        "GIT_ATTR_NOSYSTEM",
        "GIT_ALLOW_PROTOCOL",
        "GIT_PROTOCOL_FROM_USER",
        "GIT_LFS_SKIP_SMUDGE",
        "GIT_OPTIONAL_LOCKS",
        "GIT_TERMINAL_PROMPT",
        "GIT_ASKPASS",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_PROXY_COMMAND",
        "GIT_EXTERNAL_DIFF",
    ];
    for variable in GIT_ENVIRONMENT {
        command.env_remove(variable);
    }
    for (name, _) in inherited {
        if name.to_str().is_some_and(|name| {
            name.starts_with("GIT_CONFIG_KEY_") || name.starts_with("GIT_CONFIG_VALUE_")
        }) {
            command.env_remove(name);
        }
    }
}

async fn read_git_output(mut reader: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut oversized = false;
    loop {
        let read = reader.read(&mut chunk).await.context("read Git output")?;
        if read == 0 {
            break;
        }
        if output.len() + read > GIT_OUTPUT_LIMIT {
            oversized = true;
        } else if !oversized {
            output.extend_from_slice(&chunk[..read]);
        }
    }
    if oversized {
        bail!("Git output exceeded {GIT_OUTPUT_LIMIT} bytes");
    }
    Ok(output)
}

async fn clean_up_git<R>(
    child: &mut Child,
    process_group: Option<u32>,
    stdout: &mut JoinHandle<R>,
    stderr: &mut JoinHandle<R>,
) {
    #[cfg(unix)]
    if let Some(pid) = process_group {
        // Each Git process is its group's leader, so this also terminates
        // remote helpers and any descendants retaining inherited pipes.
        unsafe {
            let pgid = pid as libc::pid_t;
            if pgid > 0 && pgid != libc::getpgrp() {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
    stdout.abort();
    stderr.abort();
    let _ = stdout.await;
    let _ = stderr.await;
}

fn git_success(output: GitOutput, operation: &str) -> Result<GitOutput> {
    if output.status.success() {
        Ok(output)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "could not {operation} (Git status {:?}): {}",
            output.status.code(),
            stderr.trim_end()
        )
    }
}

fn required_git_line<'a>(output: &'a [u8], kind: &str) -> Result<&'a [u8]> {
    let line = output.strip_suffix(b"\n").unwrap_or(output);
    if line.is_empty() || line.contains(&0) || line.contains(&b'\n') {
        bail!("Git returned invalid or missing {kind}");
    }
    Ok(line)
}

#[cfg(unix)]
fn null_device() -> &'static OsStr {
    OsStr::new("/dev/null")
}

#[cfg(not(unix))]
fn null_device() -> &'static OsStr {
    OsStr::new("NUL")
}

fn validate_digest(digest: &str) -> Result<()> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        bail!("content digest must be 64 lowercase hexadecimal characters");
    }
    Ok(())
}

fn validate_path(label: &str, path: &Path) -> Result<()> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > MAX_PATH_BYTES {
        bail!("{label} exceeds {MAX_PATH_BYTES} bytes");
    }
    if path.to_str().is_none() {
        bail!("{label} must be valid UTF-8 for the JSON store index");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::{
        fs::{PermissionsExt, symlink},
        net::UnixListener,
    };

    use serde_json::Value;

    use super::*;

    fn package(parent: &Path, id: &str, version: &str) -> PathBuf {
        let package = parent.join(format!("{id}-source"));
        fs::create_dir(&package).unwrap();
        fs::write(
            package.join(extensions::MANIFEST_FILE_NAME),
            format!(
                "api_version = 1\nversion = {version:?}\nfut = \">=0.7.0, <1.0.0\"\ncapabilities = []\nid = {id:?}\n"
            ),
        )
        .unwrap();
        fs::create_dir(package.join("assets")).unwrap();
        fs::write(package.join("assets/readme.txt"), "managed content\n").unwrap();
        package
    }

    #[test]
    fn install_enable_disable_and_remove_are_atomic_store_operations() {
        let temporary = tempfile::tempdir().unwrap();
        let source = package(temporary.path(), "managed", "1.2.3");
        let store_root = temporary.path().join("data/fut/extensions");

        assert!(enabled_roots_at(&store_root).unwrap().is_empty());
        assert!(!store_root.exists(), "a missing store should remain absent");

        let installed = install_at(&source, &store_root).unwrap();
        assert!(installed.changed);
        assert!(!installed.extension.enabled);
        assert_eq!(installed.extension.id, "managed");
        assert_eq!(installed.extension.version, "1.2.3");
        assert_eq!(
            installed.extension.source,
            Some(fs::canonicalize(&source).unwrap())
        );
        assert_eq!(installed.extension.provenance, None);
        assert!(installed.extension.install_path.is_dir());
        assert_eq!(installed.extension.content_sha256.len(), 64);
        assert_eq!(
            fs::metadata(
                installed
                    .extension
                    .install_path
                    .join(extensions::MANIFEST_FILE_NAME)
            )
            .unwrap()
            .permissions()
            .mode()
                & 0o222,
            0
        );
        assert!(enabled_roots_at(&store_root).unwrap().is_empty());

        let index: Value =
            serde_json::from_slice(&fs::read(store_root.join(INDEX_FILE_NAME)).unwrap()).unwrap();
        assert_eq!(index["schema_version"], INDEX_SCHEMA_VERSION);
        assert_eq!(index["extensions"][0]["id"], "managed");
        assert_eq!(index["extensions"][0]["enabled"], false);
        assert_eq!(
            index["extensions"][0]["content_sha256"],
            installed.extension.content_sha256
        );

        let enabled = set_enabled_at(&store_root, "managed", true).unwrap();
        assert!(enabled.changed);
        assert!(enabled.extension.enabled);
        assert_eq!(
            enabled_roots_at(&store_root).unwrap().as_slice(),
            std::slice::from_ref(&installed.extension.install_path)
        );
        assert!(
            !set_enabled_at(&store_root, "managed", true)
                .unwrap()
                .changed
        );
        assert!(matches!(
            remove_at(&store_root, "managed"),
            Err(StoreMutationError::Enabled { .. })
        ));
        assert!(installed.extension.install_path.exists());

        let disabled = set_enabled_at(&store_root, "managed", false).unwrap();
        assert!(disabled.changed);
        let removed = remove_at(&store_root, "managed").unwrap();
        assert!(removed.changed);
        assert!(installed.extension.install_path.exists());
        assert!(
            read_index(&fs::canonicalize(&store_root).unwrap())
                .unwrap()
                .extensions
                .is_empty()
        );
    }

    #[test]
    fn install_rejects_links_and_special_files_without_replacing_existing_content() {
        let temporary = tempfile::tempdir().unwrap();
        let source = package(temporary.path(), "safe", "1.0.0");
        let store_root = temporary.path().join("store");
        let installed = install_at(&source, &store_root).unwrap();

        symlink("assets/readme.txt", source.join("linked")).unwrap();
        let error = install_at(&source, &store_root).unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
        assert!(installed.extension.install_path.exists());
        let canonical_store = fs::canonicalize(&store_root).unwrap();
        let index = read_index(&canonical_store).unwrap();
        assert_eq!(
            index.extensions.as_slice(),
            std::slice::from_ref(&installed.extension)
        );
        fs::remove_file(source.join("linked")).unwrap();

        let socket_path = source.join("special.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let error = install_at(&source, &store_root).unwrap_err().to_string();
        assert!(error.contains("special file"), "{error}");
        assert_eq!(
            read_index(&canonical_store).unwrap().extensions[0],
            installed.extension
        );

        let linked_source = temporary.path().join("linked-source");
        symlink(&source, &linked_source).unwrap();
        let error = install_at(&linked_source, &store_root)
            .unwrap_err()
            .to_string();
        assert!(error.contains("symbolic link"), "{error}");
    }

    #[test]
    fn enabled_managed_content_is_verified_against_its_digest() {
        let temporary = tempfile::tempdir().unwrap();
        let source = package(temporary.path(), "verified", "1.0.0");
        let store_root = temporary.path().join("store");
        let installed = install_at(&source, &store_root).unwrap();
        set_enabled_at(&store_root, "verified", true).unwrap();

        let manifest = installed
            .extension
            .install_path
            .join(extensions::MANIFEST_FILE_NAME);
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o644)).unwrap();
        let error = format!("{:#}", enabled_roots_at(&store_root).unwrap_err());
        assert!(error.contains("content hash mismatch"), "{error}");
    }

    #[test]
    fn index_is_strict_versioned_and_package_parents_cannot_redirect_through_symlinks() {
        let temporary = tempfile::tempdir().unwrap();
        let store_root = temporary.path().join("store");
        fs::create_dir(&store_root).unwrap();
        fs::write(
            store_root.join(INDEX_FILE_NAME),
            r#"{"schema_version":1,"extensions":[],"unknown":true}"#,
        )
        .unwrap();
        let error = format!("{:#}", enabled_roots_at(&store_root).unwrap_err());
        assert!(error.contains("unknown field"), "{error}");

        fs::remove_file(store_root.join(INDEX_FILE_NAME)).unwrap();
        let outside = temporary.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, store_root.join("packages")).unwrap();
        let source = package(temporary.path(), "redirect", "1.0.0");
        let error = install_at(&source, &store_root).unwrap_err().to_string();
        assert!(error.contains("not a real directory"), "{error}");
        assert!(fs::read_dir(outside).unwrap().next().is_none());
    }

    #[test]
    fn git_revisions_urls_and_trees_are_narrowly_validated() {
        assert_eq!(
            normalize_commit("ABCDEF0123456789ABCDEF0123456789ABCDEF01").unwrap(),
            "abcdef0123456789abcdef0123456789abcdef01"
        );
        for revision in ["main", "HEAD", "abcdef0", "refs/tags/v1"] {
            assert!(normalize_commit(revision).is_err(), "accepted {revision:?}");
        }
        assert!(validate_remote_url("https://example.invalid/extension.git").is_ok());
        assert!(validate_remote_url("file:///tmp/extension.git").is_ok());
        for url in [
            "/tmp/extension.git",
            "file://relative/path",
            "ssh://example.invalid/extension.git",
            "ext::sh -c bad",
            "https://token@example.invalid/extension.git",
            "https://example.invalid/extension.git?token=secret",
            "https://example.invalid/extension.git#main",
        ] {
            assert!(validate_remote_url(url).is_err(), "accepted {url:?}");
        }

        let error = inspect_git_tree(b"040000 12\n160000 0\n", &"a".repeat(40))
            .unwrap_err()
            .to_string();
        assert!(error.contains("submodule"), "{error}");
    }

    #[tokio::test]
    async fn timed_out_git_process_kills_its_descendants() {
        let temporary = tempfile::tempdir().unwrap();
        let fake_git = temporary.path().join("git");
        fs::write(
            &fake_git,
            "#!/bin/sh\nsleep 10 &\necho $! > descendant.pid\nwait\n",
        )
        .unwrap();
        fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755)).unwrap();

        let error = run_git_with(
            fake_git.as_os_str(),
            Duration::from_millis(500),
            temporary.path(),
            &["fetch"],
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("timed out"), "{error}");

        let pid: libc::pid_t = fs::read_to_string(temporary.path().join("descendant.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        for _ in 0..100 {
            if unsafe { libc::kill(pid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        unsafe { libc::kill(pid, libc::SIGKILL) };
        panic!("Git descendant {pid} survived process-group cleanup");
    }
}
