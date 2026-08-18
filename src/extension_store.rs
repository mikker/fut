//! Fut-owned, local managed extension packages and enablement state.

use std::{
    collections::HashSet,
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::extensions;

const INDEX_FILE_NAME: &str = "index.json";
const LOCK_FILE_NAME: &str = ".lock";
const INDEX_SCHEMA_VERSION: u8 = 1;
const MAX_INDEX_BYTES: u64 = 1024 * 1024;
const MAX_MANAGED_EXTENSIONS: usize = 32;
const MAX_PATH_BYTES: usize = 4096;
const MAX_VERSION_BYTES: usize = 128;

pub(crate) const MAX_PACKAGE_FILES: usize = 1024;
pub(crate) const MAX_PACKAGE_ENTRIES: usize = 2048;
pub(crate) const MAX_PACKAGE_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_PACKAGE_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedExtension {
    pub(crate) id: String,
    pub(crate) version: String,
    pub(crate) source: PathBuf,
    pub(crate) content_sha256: String,
    pub(crate) install_path: PathBuf,
    pub(crate) enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoreChange {
    pub(crate) extension: ManagedExtension,
    pub(crate) changed: bool,
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
        let lock = StoreLock::acquire(&root, libc::LOCK_SH)?;
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
    let initial = extensions::validate_package(&source)
        .with_context(|| format!("validate extension package {}", source.display()))?;
    if initial.version.len() > MAX_VERSION_BYTES {
        bail!(
            "extension {:?} version exceeds {MAX_VERSION_BYTES} bytes",
            initial.id
        );
    }
    if configured_store.starts_with(&source) {
        bail!(
            "managed extension store {} cannot be inside source package {}",
            configured_store.display(),
            source.display()
        );
    }
    let store = Store::open_for_write(configured_store)?;
    let mut index = store.read_index()?;
    let previous = index
        .extensions
        .iter()
        .find(|extension| extension.id == initial.id)
        .cloned();
    if previous.is_none() && index.extensions.len() >= MAX_MANAGED_EXTENSIONS {
        bail!(
            "managed extension store contains {} extensions; maximum is {MAX_MANAGED_EXTENSIONS}",
            index.extensions.len()
        );
    }

    let version_parent = ensure_package_parent(&store.root, &initial.id, &initial.version)?;
    let staging = version_parent.join(format!(".install-{}", Uuid::new_v4()));
    fs::create_dir(&staging)
        .with_context(|| format!("create extension staging directory {}", staging.display()))?;

    let staged_result = (|| -> Result<(ManagedExtension, bool)> {
        copy_package(&source, &staging)?;
        make_tree_read_only(&staging)?;
        let staged = extensions::validate_package(&staging)
            .with_context(|| format!("validate staged extension {}", staging.display()))?;
        if staged.id != initial.id || staged.version != initial.version {
            bail!(
                "extension manifest changed while installing (expected {} {}, copied {} {})",
                initial.id,
                initial.version,
                staged.id,
                staged.version
            );
        }
        let digest = hash_package(&staging)?;
        let install_path = version_parent.join(&digest);
        let metadata = ManagedExtension {
            id: staged.id,
            version: staged.version,
            source: source.clone(),
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
                fs::rename(&staging, &install_path).with_context(|| {
                    format!(
                        "atomically install staged extension {} at {}",
                        staging.display(),
                        install_path.display()
                    )
                })?;
                true
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect extension install path {}", install_path.display())
                });
            }
        };
        Ok((metadata, created))
    })();

    let (metadata, created) = match staged_result {
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
    let mut store = match Store::open_existing(configured_root)? {
        Some(store) => store,
        None => return Err(StoreMutationError::NotFound { id: id.to_owned() }),
    };
    // Upgrade the shared reader lock to an exclusive mutation lock while no
    // state is retained from before the upgrade.
    drop(store);
    store = Store::open_for_write(configured_root)?;
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
    if Store::open_existing(configured_root)?.is_none() {
        return Err(StoreMutationError::NotFound { id: id.to_owned() });
    }
    let store = Store::open_for_write(configured_root)?;
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
        validate_path("managed extension source", &extension.source)?;
        validate_path("managed extension install_path", &extension.install_path)?;
        if !extension.source.is_absolute() || !extension.install_path.is_absolute() {
            bail!(
                "managed extension {:?} source and install_path must be absolute",
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
    if id.is_empty() || id.len() > 64 {
        bail!("extension ID must be 1 through 64 bytes");
    }
    let bytes = id.as_bytes();
    if !bytes[0].is_ascii_lowercase() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        bail!(
            "extension ID {id:?} must start with a lowercase letter and end with a lowercase letter or digit"
        );
    }
    let mut separator = false;
    for &byte in bytes {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            separator = false;
        } else if matches!(byte, b'.' | b'_' | b'-') && !separator {
            separator = true;
        } else {
            bail!(
                "extension ID {id:?} may contain lowercase ASCII letters, digits, and single '.', '_', or '-' separators"
            );
        }
    }
    Ok(())
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
            fs::canonicalize(&source).unwrap()
        );
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
}
