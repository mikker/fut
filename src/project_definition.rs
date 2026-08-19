//! Durable project recipe loading, trust verification, and validation.

use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    client::config::{
        ProjectConfig, deserialize_extension_config_catalog, validate_extension_config_catalog,
    },
    extensions::Extension,
    resources::ExtensionConfigTable,
    splits::SplitDirection,
};

const MAX_RECIPE_BYTES: u64 = 64 * 1024;
const MAX_TRUST_STORE_BYTES: u64 = 1024 * 1024;
const MAX_TRUSTED_RECIPES: usize = 4_096;
const TRUST_STORE_VERSION: u8 = 1;
const MAX_TABS: usize = 32;
const MAX_PANES_PER_TAB: usize = 32;
const MAX_PANES: usize = 128;
const MAX_ARGUMENTS: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 4 * 1024;
const MAX_ENVIRONMENT: usize = 128;
const MAX_ENVIRONMENT_VALUE_BYTES: usize = 4 * 1024;
const MAX_ID_BYTES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceRecipe {
    #[serde(default, deserialize_with = "deserialize_extension_config_catalog")]
    extension: BTreeMap<String, ExtensionConfigTable>,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    #[serde(default)]
    focus: Option<String>,
    tabs: Vec<RecipeTab>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecipeTab {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    panes: Vec<RecipePane>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecipePane {
    id: String,
    #[serde(default)]
    command: Option<Vec<String>>,
    #[serde(default)]
    exec: bool,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    #[serde(default)]
    split: Option<RecipeSplit>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecipeSplit {
    target: String,
    direction: SplitDirection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LoadedRecipe {
    pub source: PathBuf,
    pub digest: String,
    pub recipe: WorkspaceRecipe,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecipeTrustChange {
    pub source: PathBuf,
    pub digest: Option<String>,
    pub changed: bool,
    pub trusted: bool,
    pub inherently_trusted: bool,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProjectDefinitionError {
    #[error(
        "untrusted project recipe {} (SHA-256 {digest}); {instruction}",
        path.display(),
        instruction = trust_instruction(project.as_deref())
    )]
    UntrustedRecipe {
        project: Option<String>,
        path: PathBuf,
        digest: String,
    },
    #[error(
        "project {project:?} uses the explicitly configured recipe {}; it is inherently trusted and cannot be untrusted without removing `recipe` from global config",
        path.display()
    )]
    InherentlyTrusted { project: String, path: PathBuf },
    #[error(transparent)]
    Invalid(#[from] anyhow::Error),
}

fn trust_instruction(project: Option<&str>) -> String {
    project.map_or_else(
        || "approve it before opening a new workspace".into(),
        |project| format!("run `fut project trust {project}` after reviewing it"),
    )
}

#[derive(Debug)]
struct RecipeFile {
    source: PathBuf,
    digest: String,
    bytes: Vec<u8>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustStore {
    version: u8,
    #[serde(default)]
    recipes: Vec<TrustedRecipe>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustedRecipe {
    path: String,
    sha256: String,
}

impl TrustStore {
    fn empty() -> Self {
        Self {
            version: TRUST_STORE_VERSION,
            recipes: Vec::new(),
        }
    }

    fn trusted(&self, path: &str, digest: &str) -> bool {
        self.recipes
            .iter()
            .any(|recipe| recipe.path == path && recipe.sha256 == digest)
    }
}

struct TrustStoreLock {
    _file: fs::File,
}

impl TrustStoreLock {
    fn acquire(store_path: &Path) -> Result<Self> {
        prepare_store_parent(store_path)?;
        let lock_path = store_path.with_file_name("trusted-recipes.lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lock_path)
            .with_context(|| format!("open project recipe trust lock {}", lock_path.display()))?;
        if !file
            .metadata()
            .with_context(|| format!("inspect project recipe trust lock {}", lock_path.display()))?
            .file_type()
            .is_file()
        {
            bail!(
                "project recipe trust lock {} is not a regular file",
                lock_path.display()
            );
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("secure project recipe trust lock {}", lock_path.display()))?;
        loop {
            // SAFETY: `file` owns a valid descriptor and `LOCK_EX` has no pointer arguments.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).with_context(|| {
                    format!("lock project recipe trust store {}", store_path.display())
                });
            }
        }
        Ok(Self { _file: file })
    }
}

impl WorkspaceRecipe {
    pub(crate) fn extension(&self) -> &BTreeMap<String, ExtensionConfigTable> {
        &self.extension
    }

    pub(crate) fn tabs(&self) -> &[RecipeTab] {
        &self.tabs
    }

    pub(crate) fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    pub(crate) fn focus(&self) -> Option<&str> {
        self.focus.as_deref()
    }
}

impl RecipeTab {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    pub(crate) fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    pub(crate) fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    pub(crate) fn panes(&self) -> &[RecipePane] {
        &self.panes
    }
}

impl RecipePane {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn command(&self) -> Option<&[String]> {
        self.command.as_deref()
    }

    pub(crate) const fn exec(&self) -> bool {
        self.exec
    }

    pub(crate) fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    pub(crate) fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    pub(crate) fn split(&self) -> Option<&RecipeSplit> {
        self.split.as_ref()
    }
}

impl RecipeSplit {
    pub(crate) fn target(&self) -> &str {
        &self.target
    }

    pub(crate) const fn direction(&self) -> SplitDirection {
        self.direction
    }
}

/// Load a configured recipe. An explicit global recipe path is trusted by the
/// global configuration boundary. A conventional repository recipe must match
/// the current machine-local approval before its parsed commands are returned.
pub(crate) fn load(
    project_name: Option<&str>,
    project: &ProjectConfig,
    extensions: &[Extension],
) -> std::result::Result<Option<LoadedRecipe>, ProjectDefinitionError> {
    load_with_trust_store(project_name, project, extensions, None)
}

fn load_with_trust_store(
    project_name: Option<&str>,
    project: &ProjectConfig,
    extensions: &[Extension],
    state_path: Option<&Path>,
) -> std::result::Result<Option<LoadedRecipe>, ProjectDefinitionError> {
    let explicit = project.recipe().is_some();
    let configured_source = project
        .recipe()
        .map_or_else(|| repository_recipe_path(project), Path::to_path_buf);
    let Some(file) = read_recipe(&configured_source, explicit)? else {
        return Ok(None);
    };
    if !explicit {
        let resolved_state_path;
        let state_path = match state_path {
            Some(state_path) => state_path,
            None => {
                resolved_state_path = trust_store_path()?;
                &resolved_state_path
            }
        };
        let store = read_trust_store(state_path)?;
        let source = storable_path(&file.source)?;
        if !store.trusted(source, &file.digest) {
            return Err(ProjectDefinitionError::UntrustedRecipe {
                project: project_name.map(str::to_owned),
                path: file.source,
                digest: file.digest,
            });
        }
    }
    Ok(Some(parse_recipe(file, extensions)?))
}

/// Validate and approve the exact current bytes of a repository recipe.
/// Explicit global recipe paths are already trusted and therefore produce a
/// validated no-op without creating machine-local state.
pub(crate) fn trust(
    project: &ProjectConfig,
    extensions: &[Extension],
) -> std::result::Result<RecipeTrustChange, ProjectDefinitionError> {
    trust_with_store(project, extensions, None)
}

fn trust_with_store(
    project: &ProjectConfig,
    extensions: &[Extension],
    state_path: Option<&Path>,
) -> std::result::Result<RecipeTrustChange, ProjectDefinitionError> {
    if let Some(source) = project.recipe() {
        let file = read_recipe(source, true)?.ok_or_else(|| {
            anyhow::anyhow!(
                "configured project recipe {} does not exist",
                source.display()
            )
        })?;
        let loaded = parse_recipe(file, extensions)?;
        return Ok(RecipeTrustChange {
            source: loaded.source,
            digest: Some(loaded.digest),
            changed: false,
            trusted: true,
            inherently_trusted: true,
        });
    }

    let configured_source = repository_recipe_path(project);
    let file = read_recipe(&configured_source, true)?.ok_or_else(|| {
        anyhow::anyhow!(
            "repository project recipe {} does not exist",
            configured_source.display()
        )
    })?;
    // Approval is only written after strict UTF-8, TOML, version, and semantic
    // validation of the exact bytes whose digest will be stored.
    parse_recipe_ref(&file, extensions)?;
    let resolved_state_path;
    let state_path = match state_path {
        Some(state_path) => state_path,
        None => {
            resolved_state_path = trust_store_path()?;
            &resolved_state_path
        }
    };
    trust_at(state_path, file).map_err(Into::into)
}

/// Revoke the machine-local approval for a repository recipe. Explicit global
/// recipe paths cannot be made untrusted through the local approval store.
pub(crate) fn untrust(
    project_name: &str,
    project: &ProjectConfig,
) -> std::result::Result<RecipeTrustChange, ProjectDefinitionError> {
    untrust_with_store(project_name, project, None)
}

fn untrust_with_store(
    project_name: &str,
    project: &ProjectConfig,
    state_path: Option<&Path>,
) -> std::result::Result<RecipeTrustChange, ProjectDefinitionError> {
    if let Some(path) = project.recipe() {
        return Err(ProjectDefinitionError::InherentlyTrusted {
            project: project_name.to_owned(),
            path: path.to_owned(),
        });
    }
    let source = canonical_repository_recipe_path(project)?;
    let resolved_state_path;
    let state_path = match state_path {
        Some(state_path) => state_path,
        None => {
            resolved_state_path = trust_store_path()?;
            &resolved_state_path
        }
    };
    untrust_at(state_path, source).map_err(Into::into)
}

fn repository_recipe_path(project: &ProjectConfig) -> PathBuf {
    project.path().join(".fut/project.toml")
}

fn read_recipe(path: &Path, required: bool) -> Result<Option<RecipeFile>> {
    let source = match fs::canonicalize(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("resolve project recipe {}", path.display()));
        }
    };
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(&source)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read project recipe {}", source.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect project recipe {}", source.display()))?;
    if !metadata.file_type().is_file() {
        bail!("project recipe {} is not a regular file", source.display());
    }
    if metadata.len() > MAX_RECIPE_BYTES {
        bail!(
            "project recipe {} is {} bytes; maximum is {MAX_RECIPE_BYTES}",
            source.display(),
            metadata.len()
        );
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_RECIPE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read project recipe {}", source.display()))?;
    if bytes.len() as u64 > MAX_RECIPE_BYTES {
        bail!(
            "project recipe {} exceeds the {MAX_RECIPE_BYTES}-byte maximum",
            source.display()
        );
    }
    let digest = format!("{:x}", Sha256::digest(&bytes));
    Ok(Some(RecipeFile {
        source,
        digest,
        bytes,
    }))
}

fn parse_recipe(file: RecipeFile, extensions: &[Extension]) -> Result<LoadedRecipe> {
    let recipe = parse_recipe_ref(&file, extensions)?;
    Ok(LoadedRecipe {
        source: file.source,
        digest: file.digest,
        recipe,
    })
}

fn parse_recipe_ref(file: &RecipeFile, extensions: &[Extension]) -> Result<WorkspaceRecipe> {
    let text = std::str::from_utf8(&file.bytes)
        .with_context(|| format!("project recipe {} is not UTF-8", file.source.display()))?;
    let recipe = toml::from_str::<WorkspaceRecipe>(text)
        .with_context(|| format!("parse project recipe {}", file.source.display()))?;
    validate(&recipe, extensions)
        .with_context(|| format!("validate project recipe {}", file.source.display()))?;
    Ok(recipe)
}

fn trust_at(state_path: &Path, file: RecipeFile) -> Result<RecipeTrustChange> {
    let source = storable_path(&file.source)?.to_owned();
    let _lock = TrustStoreLock::acquire(state_path)?;
    let mut store = read_trust_store(state_path)?;
    let existing = store
        .recipes
        .iter_mut()
        .find(|recipe| recipe.path == source);
    let changed = match existing {
        Some(existing) if existing.sha256 == file.digest => false,
        Some(existing) => {
            existing.sha256.clone_from(&file.digest);
            true
        }
        None => {
            if store.recipes.len() >= MAX_TRUSTED_RECIPES {
                bail!(
                    "project recipe trust store cannot contain more than {MAX_TRUSTED_RECIPES} entries"
                );
            }
            store.recipes.push(TrustedRecipe {
                path: source,
                sha256: file.digest.clone(),
            });
            true
        }
    };
    if changed {
        store
            .recipes
            .sort_by(|left, right| left.path.cmp(&right.path));
        write_trust_store(state_path, &store)?;
    }
    Ok(RecipeTrustChange {
        source: file.source,
        digest: Some(file.digest),
        changed,
        trusted: true,
        inherently_trusted: false,
    })
}

fn untrust_at(state_path: &Path, source: PathBuf) -> Result<RecipeTrustChange> {
    let source_string = storable_path(&source)?;
    let _lock = TrustStoreLock::acquire(state_path)?;
    let mut store = read_trust_store(state_path)?;
    let mut removed_digest = None;
    store.recipes.retain(|recipe| {
        if recipe.path == source_string {
            removed_digest = Some(recipe.sha256.clone());
            false
        } else {
            true
        }
    });
    let changed = removed_digest.is_some();
    if changed {
        write_trust_store(state_path, &store)?;
    }
    Ok(RecipeTrustChange {
        source,
        digest: removed_digest,
        changed,
        trusted: false,
        inherently_trusted: false,
    })
}

fn canonical_repository_recipe_path(project: &ProjectConfig) -> Result<PathBuf> {
    let path = repository_recipe_path(project);
    match fs::canonicalize(&path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent()
                && let Ok(parent) = fs::canonicalize(parent)
            {
                return Ok(parent.join("project.toml"));
            }
            let root = fs::canonicalize(project.path()).with_context(|| {
                format!("resolve configured project {}", project.path().display())
            })?;
            Ok(root.join(".fut/project.toml"))
        }
        Err(error) => Err(error)
            .with_context(|| format!("resolve repository project recipe {}", path.display())),
    }
}

pub(crate) fn trust_store_path() -> Result<PathBuf> {
    trust_store_path_from(
        env::var_os("XDG_STATE_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
    )
}

fn trust_store_path_from(
    xdg_state_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    if let Some(directory) = xdg_state_home.filter(|value| !value.is_empty()) {
        let directory = PathBuf::from(directory);
        if !directory.is_absolute() {
            bail!(
                "XDG_STATE_HOME must be an absolute path when resolving project recipe trust state"
            );
        }
        return Ok(directory.join("fut/trusted-recipes.toml"));
    }
    let home = home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("HOME must be set when XDG_STATE_HOME is not set")?;
    if !home.is_absolute() {
        bail!("HOME must be an absolute path when resolving project recipe trust state");
    }
    Ok(home.join(".local/state/fut/trusted-recipes.toml"))
}

fn read_trust_store(path: &Path) -> Result<TrustStore> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TrustStore::empty());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read project recipe trust store {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect project recipe trust store {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "project recipe trust store {} is not a regular file",
            path.display()
        );
    }
    if metadata.permissions().mode() & 0o7777 != 0o600 {
        bail!(
            "project recipe trust store {} must have permissions 0600",
            path.display()
        );
    }
    if metadata.len() > MAX_TRUST_STORE_BYTES {
        bail!(
            "project recipe trust store {} is {} bytes; maximum is {MAX_TRUST_STORE_BYTES}",
            path.display(),
            metadata.len()
        );
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_TRUST_STORE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read project recipe trust store {}", path.display()))?;
    if bytes.len() as u64 > MAX_TRUST_STORE_BYTES {
        bail!(
            "project recipe trust store {} exceeds the {MAX_TRUST_STORE_BYTES}-byte maximum",
            path.display()
        );
    }
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("project recipe trust store {} is not UTF-8", path.display()))?;
    let store = toml::from_str::<TrustStore>(text)
        .with_context(|| format!("parse project recipe trust store {}", path.display()))?;
    validate_trust_store(&store)
        .with_context(|| format!("validate project recipe trust store {}", path.display()))?;
    Ok(store)
}

fn validate_trust_store(store: &TrustStore) -> Result<()> {
    if store.version != TRUST_STORE_VERSION {
        bail!(
            "unsupported trust store version {}; expected {TRUST_STORE_VERSION}",
            store.version
        );
    }
    if store.recipes.len() > MAX_TRUSTED_RECIPES {
        bail!("project recipe trust store contains more than {MAX_TRUSTED_RECIPES} entries");
    }
    let mut paths = HashSet::with_capacity(store.recipes.len());
    for recipe in &store.recipes {
        let path = Path::new(&recipe.path);
        if recipe.path.is_empty() || !path.is_absolute() {
            bail!("project recipe trust store contains a non-absolute recipe path");
        }
        if !paths.insert(&recipe.path) {
            bail!(
                "project recipe trust store contains duplicate path {:?}",
                recipe.path
            );
        }
        if recipe.sha256.len() != 64
            || !recipe
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("project recipe trust store contains an invalid SHA-256 digest");
        }
    }
    Ok(())
}

fn prepare_store_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("project recipe trust store path must have a parent directory")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "create project recipe trust store directory {}",
            parent.display()
        )
    })?;
    let metadata = fs::symlink_metadata(parent).with_context(|| {
        format!(
            "inspect project recipe trust store directory {}",
            parent.display()
        )
    })?;
    if !metadata.file_type().is_dir() {
        bail!(
            "project recipe trust store directory {} is not a directory",
            parent.display()
        );
    }
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "secure project recipe trust store directory {}",
            parent.display()
        )
    })?;
    Ok(())
}

fn write_trust_store(path: &Path, store: &TrustStore) -> Result<()> {
    prepare_store_parent(path)?;
    let contents = toml::to_string_pretty(store).context("serialize project recipe trust store")?;
    if contents.len() as u64 > MAX_TRUST_STORE_BYTES {
        bail!(
            "updated project recipe trust store exceeds the {MAX_TRUST_STORE_BYTES}-byte maximum"
        );
    }
    let parent = path
        .parent()
        .expect("validated project recipe trust store parent");
    let temporary_path = parent.join(format!(".trusted-recipes.{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut temporary = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary_path)
            .with_context(|| {
                format!(
                    "create temporary project recipe trust store {}",
                    temporary_path.display()
                )
            })?;
        temporary
            .set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!(
                    "secure temporary project recipe trust store {}",
                    temporary_path.display()
                )
            })?;
        temporary.write_all(contents.as_bytes()).with_context(|| {
            format!(
                "write temporary project recipe trust store {}",
                temporary_path.display()
            )
        })?;
        temporary.sync_all().with_context(|| {
            format!(
                "sync temporary project recipe trust store {}",
                temporary_path.display()
            )
        })?;
        fs::rename(&temporary_path, path).with_context(|| {
            format!(
                "atomically replace project recipe trust store {}",
                path.display()
            )
        })?;
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| {
                format!(
                    "sync project recipe trust store directory {}",
                    parent.display()
                )
            })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn storable_path(path: &Path) -> Result<&str> {
    path.to_str().with_context(|| {
        format!(
            "canonical project recipe path {} is not valid UTF-8",
            path.display()
        )
    })
}

fn validate(recipe: &WorkspaceRecipe, extensions: &[Extension]) -> Result<()> {
    if recipe.tabs.is_empty() || recipe.tabs.len() > MAX_TABS {
        bail!("recipe must contain between 1 and {MAX_TABS} tabs");
    }
    validate_extension_config_catalog(&recipe.extension, extensions, Path::new("project recipe"))?;
    validate_environment(&recipe.environment, "environment")?;
    let mut tab_ids = HashSet::new();
    let mut tab_names = HashSet::new();
    let mut pane_count = 0;
    let mut focus_targets = HashSet::new();
    for (tab_index, tab) in recipe.tabs.iter().enumerate() {
        let tab_path = format!("tabs[{tab_index}]");
        validate_id(&tab.id, &format!("{tab_path}.id"))?;
        if !tab_ids.insert(&tab.id) {
            bail!("duplicate tab ID {:?}", tab.id);
        }
        if tab.name.as_ref().is_some_and(|name| name.trim().is_empty()) {
            bail!("{tab_path}.name cannot be empty or whitespace-only");
        }
        if !tab_names.insert(tab.name()) {
            bail!("duplicate tab display name {:?}", tab.name());
        }
        validate_cwd(tab.cwd.as_deref(), &format!("{tab_path}.cwd"))?;
        validate_environment(&tab.environment, &format!("{tab_path}.environment"))?;
        if tab.panes.is_empty() || tab.panes.len() > MAX_PANES_PER_TAB {
            bail!("{tab_path} must contain between 1 and {MAX_PANES_PER_TAB} panes");
        }
        pane_count += tab.panes.len();
        if pane_count > MAX_PANES {
            bail!("recipe contains more than {MAX_PANES} panes");
        }
        let mut pane_ids = HashSet::new();
        for (pane_index, pane) in tab.panes.iter().enumerate() {
            let pane_path = format!("{tab_path}.panes[{pane_index}]");
            validate_id(&pane.id, &format!("{pane_path}.id"))?;
            if !pane_ids.insert(&pane.id) {
                bail!("duplicate pane ID {:?} in tab {:?}", pane.id, tab.id);
            }
            focus_targets.insert(format!("{}.{}", tab.id, pane.id));
            validate_command(pane.command.as_deref(), &format!("{pane_path}.command"))?;
            validate_cwd(pane.cwd.as_deref(), &format!("{pane_path}.cwd"))?;
            validate_environment(&pane.environment, &format!("{pane_path}.environment"))?;
            match (pane_index, &pane.split) {
                (0, None) => {}
                (0, Some(_)) => bail!("the first pane in tab {:?} cannot split", tab.id),
                (_, None) => bail!("{pane_path}.split is required"),
                (_, Some(split)) if !pane_ids.contains(&split.target) => bail!(
                    "{pane_path}.split target {:?} must name an earlier pane in the same tab",
                    split.target
                ),
                _ => {}
            }
        }
    }
    if let Some(focus) = &recipe.focus
        && !focus_targets.contains(focus)
    {
        bail!("focus {focus:?} does not name a recipe pane as TAB.PANE");
    }
    Ok(())
}

fn validate_id(value: &str, path: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("{path} must use 1-{MAX_ID_BYTES} ASCII letters, numbers, '-' or '_'");
    }
    Ok(())
}

fn validate_command(command: Option<&[String]>, path: &str) -> Result<()> {
    let Some(command) = command else {
        return Ok(());
    };
    if command.is_empty() || command.len() > MAX_ARGUMENTS {
        bail!("{path} must contain between 1 and {MAX_ARGUMENTS} arguments");
    }
    if command.iter().any(|argument| {
        argument.is_empty() || argument.len() > MAX_ARGUMENT_BYTES || argument.contains('\0')
    }) {
        bail!(
            "{path} arguments must be nonempty, NUL-free, and at most {MAX_ARGUMENT_BYTES} bytes"
        );
    }
    Ok(())
}

fn validate_cwd(cwd: Option<&Path>, path: &str) -> Result<()> {
    if cwd.is_some_and(|cwd| cwd.as_os_str().is_empty()) {
        bail!("{path} cannot be empty");
    }
    Ok(())
}

fn validate_environment(environment: &BTreeMap<String, String>, path: &str) -> Result<()> {
    if environment.len() > MAX_ENVIRONMENT {
        bail!("{path} contains more than {MAX_ENVIRONMENT} values");
    }
    for (name, value) in environment {
        if name.is_empty()
            || name.starts_with("FUT_")
            || !name.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
            })
        {
            bail!("{path} contains invalid or reserved variable {name:?}");
        }
        if value.len() > MAX_ENVIRONMENT_VALUE_BYTES || value.contains('\0') {
            bail!("{path}.{name} exceeds {MAX_ENVIRONMENT_VALUE_BYTES} bytes or contains NUL");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Result<WorkspaceRecipe> {
        let recipe = toml::from_str(source)?;
        validate(&recipe, &[])?;
        Ok(recipe)
    }

    #[test]
    fn validates_named_tabs_direct_commands_splits_environment_and_focus() {
        let recipe = parse(
            r#"
focus = "code.agent"
environment = { RUST_BACKTRACE = "1" }

[[tabs]]
id = "code"
cwd = "."
environment = { TAB = "code" }
panes = [
  { id = "editor", command = ["nvim", "."] },
  { id = "agent", command = ["pi", "--model", "fast"], exec = true, environment = { ROLE = "agent" }, split = { target = "editor", direction = "right" } },
]

[[tabs]]
id = "server"
name = "dev server"
panes = [{ id = "server", command = ["mise", "run", "fresh"] }]
"#,
        )
        .unwrap();
        assert_eq!(recipe.focus(), Some("code.agent"));
        assert_eq!(
            recipe.tabs()[0].panes()[1].split().unwrap().target(),
            "editor"
        );
        assert_eq!(recipe.tabs()[1].name(), "dev server");
        assert!(!recipe.tabs()[0].panes()[0].exec());
        assert!(recipe.tabs()[0].panes()[1].exec());
    }

    #[test]
    fn trusted_recipe_extension_config_requires_a_loaded_extension() {
        let source = r#"
tabs = [{ id = "code", panes = [{ id = "shell" }] }]
[extension.run]
command = ["mise", "run", "dev"]
auto_start = true
"#;
        let recipe: WorkspaceRecipe = toml::from_str(source).unwrap();
        assert!(validate(&recipe, &[]).is_err());

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/extensions/run");
        let extensions = crate::extensions::load(&[root]).unwrap();
        validate(&recipe, &extensions).unwrap();
        assert_eq!(recipe.extension()["run"]["auto_start"], true);
    }

    #[test]
    fn rejects_implicit_topology_reserved_environment_and_shellish_empty_commands() {
        for source in [
            r#"version = 1
tabs = [{ id = "code", panes = [{ id = "one" }] }]"#,
            r#"tabs = [{ id = "code", panes = [{ id = "one" }, { id = "two" }] }]"#,
            r#"environment = { FUT_SOCKET = "hostile" }
tabs = [{ id = "code", panes = [{ id = "one" }] }]"#,
            r#"tabs = [{ id = "code", panes = [{ id = "one", command = [] }] }]"#,
        ] {
            assert!(parse(source).is_err(), "accepted {source}");
        }
    }

    #[test]
    fn rejects_whitespace_only_and_duplicate_effective_tab_names() {
        for (source, expected) in [
            (
                r#"tabs = [
  { id = "one", name = "  \t", panes = [{ id = "one" }] },
]"#,
                "whitespace-only",
            ),
            (
                r#"tabs = [
  { id = "code", panes = [{ id = "one" }] },
  { id = "other", name = "code", panes = [{ id = "two" }] },
]"#,
                "duplicate tab display name \"code\"",
            ),
            (
                r#"tabs = [
  { id = "one", name = "shared", panes = [{ id = "one" }] },
  { id = "two", name = "shared", panes = [{ id = "two" }] },
]"#,
                "duplicate tab display name \"shared\"",
            ),
        ] {
            let error = parse(source).unwrap_err().to_string();
            assert!(error.contains(expected), "unexpected error: {error}");
        }
    }

    #[test]
    fn repository_recipe_trust_binds_canonical_path_and_exact_bytes() {
        let temporary = tempfile::tempdir().unwrap();
        let project_root = temporary.path().join("project");
        fs::create_dir_all(project_root.join(".fut")).unwrap();
        let source = b"tabs = [{ id = \"code\", panes = [{ id = \"shell\" }] }]\n";
        let recipe_path = project_root.join(".fut/project.toml");
        fs::write(&recipe_path, source).unwrap();
        let digest = format!("{:x}", Sha256::digest(source));
        let project = ProjectConfig {
            path: project_root.clone(),
            recipe: None,
        };
        let state_path = temporary.path().join("state/fut/trusted-recipes.toml");

        let error =
            load_with_trust_store(Some("fut"), &project, &[], Some(&state_path)).unwrap_err();
        assert!(matches!(
            error,
            ProjectDefinitionError::UntrustedRecipe { .. }
        ));
        let error = error.to_string();
        assert!(error.contains(&digest), "{error}");
        assert!(error.contains("fut project trust fut"), "{error}");

        let trusted = trust_with_store(&project, &[], Some(&state_path)).unwrap();
        assert!(trusted.changed);
        assert_eq!(trusted.source, recipe_path.canonicalize().unwrap());
        assert_eq!(trusted.digest.as_deref(), Some(digest.as_str()));
        assert_eq!(
            fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let loaded = load_with_trust_store(Some("fut"), &project, &[], Some(&state_path))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.digest, digest);
        assert_eq!(loaded.recipe.tabs()[0].id(), "code");

        fs::write(
            &recipe_path,
            "tabs = [{ id = \"changed\", panes = [{ id = \"shell\" }] }]\n",
        )
        .unwrap();
        assert!(matches!(
            load_with_trust_store(Some("fut"), &project, &[], Some(&state_path)),
            Err(ProjectDefinitionError::UntrustedRecipe { .. })
        ));
        let retrusted = trust_with_store(&project, &[], Some(&state_path)).unwrap();
        assert!(retrusted.changed);
        assert_ne!(retrusted.digest.as_deref(), Some(digest.as_str()));
        assert!(
            load_with_trust_store(Some("fut"), &project, &[], Some(&state_path))
                .unwrap()
                .is_some()
        );

        let untrusted = untrust_with_store("fut", &project, Some(&state_path)).unwrap();
        assert!(untrusted.changed);
        assert!(!untrusted.trusted);
        assert!(matches!(
            load_with_trust_store(Some("fut"), &project, &[], Some(&state_path)),
            Err(ProjectDefinitionError::UntrustedRecipe { .. })
        ));
    }

    #[test]
    fn explicit_global_recipe_is_trusted_but_still_strictly_validated() {
        let temporary = tempfile::tempdir().unwrap();
        let recipe_path = temporary.path().join("recipe.toml");
        fs::write(
            &recipe_path,
            "tabs = [{ id = \"code\", panes = [{ id = \"shell\" }] }]\n",
        )
        .unwrap();
        let project = ProjectConfig {
            path: temporary.path().join("project"),
            recipe: Some(recipe_path),
        };
        assert!(load(Some("fut"), &project, &[]).unwrap().is_some());
        let trusted = trust(&project, &[]).unwrap();
        assert!(!trusted.changed);
        assert!(trusted.trusted);
        assert!(trusted.inherently_trusted);
        assert!(matches!(
            untrust("fut", &project),
            Err(ProjectDefinitionError::InherentlyTrusted { .. })
        ));
    }

    #[test]
    fn malformed_or_insecure_trust_store_never_approves_a_recipe() {
        let temporary = tempfile::tempdir().unwrap();
        let project_root = temporary.path().join("project");
        fs::create_dir_all(project_root.join(".fut")).unwrap();
        fs::write(
            project_root.join(".fut/project.toml"),
            "tabs = [{ id = \"code\", panes = [{ id = \"shell\" }] }]\n",
        )
        .unwrap();
        let project = ProjectConfig {
            path: project_root,
            recipe: None,
        };
        let state_path = temporary.path().join("state/fut/trusted-recipes.toml");
        fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        fs::write(&state_path, "version = 1\nunknown = true\n").unwrap();
        fs::set_permissions(&state_path, fs::Permissions::from_mode(0o600)).unwrap();

        let error =
            load_with_trust_store(Some("fut"), &project, &[], Some(&state_path)).unwrap_err();
        assert!(matches!(error, ProjectDefinitionError::Invalid(_)));
        assert!(format!("{error:#}").contains("parse project recipe trust store"));

        fs::write(&state_path, "version = 1\nrecipes = []\n").unwrap();
        fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644)).unwrap();
        let error =
            load_with_trust_store(Some("fut"), &project, &[], Some(&state_path)).unwrap_err();
        assert!(format!("{error:#}").contains("permissions 0600"));

        let oversized = fs::File::create(&state_path).unwrap();
        oversized.set_len(MAX_TRUST_STORE_BYTES + 1).unwrap();
        fs::set_permissions(&state_path, fs::Permissions::from_mode(0o600)).unwrap();
        let error =
            load_with_trust_store(Some("fut"), &project, &[], Some(&state_path)).unwrap_err();
        assert!(format!("{error:#}").contains("maximum"));

        fs::remove_file(&state_path).unwrap();
        let target = temporary.path().join("state/other.toml");
        fs::write(&target, "version = 1\nrecipes = []\n").unwrap();
        std::os::unix::fs::symlink(&target, &state_path).unwrap();
        let error =
            load_with_trust_store(Some("fut"), &project, &[], Some(&state_path)).unwrap_err();
        assert!(format!("{error:#}").contains("read project recipe trust store"));
    }

    #[test]
    fn invalid_recipe_is_not_approved() {
        let temporary = tempfile::tempdir().unwrap();
        let project_root = temporary.path().join("project");
        fs::create_dir_all(project_root.join(".fut")).unwrap();
        fs::write(project_root.join(".fut/project.toml"), "").unwrap();
        let project = ProjectConfig {
            path: project_root,
            recipe: None,
        };
        let state_path = temporary.path().join("state/fut/trusted-recipes.toml");

        assert!(trust_with_store(&project, &[], Some(&state_path)).is_err());
        assert!(!state_path.exists());
    }

    #[test]
    fn trust_store_location_prefers_valid_xdg_and_validates_fallback_home() {
        assert_eq!(
            trust_store_path_from(Some(Path::new("/state").as_os_str()), None).unwrap(),
            Path::new("/state/fut/trusted-recipes.toml")
        );
        assert_eq!(
            trust_store_path_from(None, Some(Path::new("/home/user").as_os_str())).unwrap(),
            Path::new("/home/user/.local/state/fut/trusted-recipes.toml")
        );
        assert_eq!(
            trust_store_path_from(
                Some(std::ffi::OsStr::new("")),
                Some(Path::new("/home/user").as_os_str())
            )
            .unwrap(),
            Path::new("/home/user/.local/state/fut/trusted-recipes.toml")
        );
        assert!(trust_store_path_from(Some(Path::new("relative").as_os_str()), None).is_err());
        assert!(trust_store_path_from(None, Some(Path::new("relative").as_os_str())).is_err());
        assert!(trust_store_path_from(None, None).is_err());
    }
}
