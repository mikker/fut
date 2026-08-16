//! Loading and validation for explicitly configured local Fut extensions.

use std::{
    collections::{BTreeMap, HashSet},
    ffi::{OsStr, OsString},
    fs,
    io::Read,
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{mpsc, watch},
    task::JoinHandle,
    time::timeout,
};

use crate::{
    command::PopupSize,
    domain::{SessionId, WorkspaceId},
    resources::{MAX_MATERIALIZED_TOKEN_VALUE_BYTES, Mutation, ResourceEvent},
};

pub(crate) const MANIFEST_FILE_NAME: &str = "fut-extension.toml";
const FUT_BIN: &str = "FUT_BIN";
const FUT_EXTENSION_ID: &str = "FUT_EXTENSION_ID";
const FUT_EXTENSION_ROOT: &str = "FUT_EXTENSION_ROOT";
const EVENT_VERSION: u8 = 1;

const SUPPORTED_HOOKS: [&str; 3] = ["workspace.created", "workspace.renamed", "workspace.closed"];
const HOOK_QUEUE_CAPACITY: usize = 128;
const HOOK_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_EVENT_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 16 * 1024;

const MAX_EXTENSIONS: usize = 32;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_HOOKS: usize = 32;
const MAX_COMMANDS: usize = 32;
const MAX_PRESENTATION_TOKENS: usize = 64;
const MAX_ARGV: usize = 64;
const MAX_IDENTIFIER_BYTES: usize = 64;
const MAX_PATH_BYTES: usize = 4096;
const MAX_ARG_BYTES: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Extension {
    id: String,
    root: PathBuf,
    hooks: BTreeMap<String, ExtensionCommand>,
    commands: BTreeMap<String, ExtensionLauncher>,
    presentation_tokens: Vec<PresentationToken>,
}

impl Extension {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn commands(&self) -> impl Iterator<Item = &ExtensionLauncher> {
        self.commands.values()
    }

    pub(crate) fn presentation_tokens(&self) -> &[PresentationToken] {
        &self.presentation_tokens
    }

    pub(crate) fn presentation_token(&self, name: &str) -> Option<&PresentationToken> {
        self.presentation_tokens
            .iter()
            .find(|token| token.name == name)
    }

    /// Stable extension identity entries added to every hook environment.
    fn command_environment(&self) -> [(OsString, OsString); 2] {
        [
            (OsString::from(FUT_EXTENSION_ID), OsString::from(&self.id)),
            (
                OsString::from(FUT_EXTENSION_ROOT),
                self.root.as_os_str().to_owned(),
            ),
        ]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExtensionCommand {
    argv: Vec<OsString>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtensionLauncher {
    name: String,
    title: String,
    command: ExtensionCommand,
    size: PopupSize,
    activate_opened: bool,
}

impl ExtensionLauncher {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn argv(&self) -> &[OsString] {
        &self.command.argv
    }

    pub(crate) fn size(&self) -> PopupSize {
        self.size
    }

    pub(crate) fn activate_opened(&self) -> bool {
        self.activate_opened
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HookEvent {
    revision: u64,
    kind: &'static str,
    session_id: SessionId,
    workspace_id: WorkspaceId,
    workspace_name: String,
    workspace_root: PathBuf,
    previous_name: Option<String>,
}

impl HookEvent {
    fn from_mutation(mutation: &Mutation) -> impl Iterator<Item = Self> + '_ {
        mutation.events.iter().filter_map(|event| match event {
            ResourceEvent::WorkspaceCreated {
                session_id,
                id,
                name,
                root,
            } => Some(Self {
                revision: mutation.revision,
                kind: "workspace.created",
                session_id: *session_id,
                workspace_id: *id,
                workspace_name: name.clone(),
                workspace_root: root.clone(),
                previous_name: None,
            }),
            ResourceEvent::WorkspaceRenamed {
                session_id,
                id,
                root,
                old_name,
                new_name,
            } => Some(Self {
                revision: mutation.revision,
                kind: "workspace.renamed",
                session_id: *session_id,
                workspace_id: *id,
                workspace_name: new_name.clone(),
                workspace_root: root.clone(),
                previous_name: Some(old_name.clone()),
            }),
            ResourceEvent::WorkspaceClosed {
                session_id,
                workspace_id,
                name,
                root,
            } => Some(Self {
                revision: mutation.revision,
                kind: "workspace.closed",
                session_id: *session_id,
                workspace_id: *workspace_id,
                workspace_name: name.clone(),
                workspace_root: root.clone(),
                previous_name: None,
            }),
            _ => None,
        })
    }

    fn payload(&self) -> Result<Vec<u8>> {
        #[derive(Serialize)]
        struct Workspace<'a> {
            id: WorkspaceId,
            name: &'a str,
            root: &'a Path,
        }

        #[derive(Serialize)]
        struct Payload<'a> {
            version: u8,
            event: &'a str,
            resource_revision: u64,
            session_id: SessionId,
            workspace: Workspace<'a>,
            #[serde(skip_serializing_if = "Option::is_none")]
            previous_name: Option<&'a str>,
        }

        let mut payload = serde_json::to_vec(&Payload {
            version: EVENT_VERSION,
            event: self.kind,
            resource_revision: self.revision,
            session_id: self.session_id,
            workspace: Workspace {
                id: self.workspace_id,
                name: &self.workspace_name,
                root: &self.workspace_root,
            },
            previous_name: self.previous_name.as_deref(),
        })?;
        payload.push(b'\n');
        if payload.len() > MAX_EVENT_PAYLOAD_BYTES {
            bail!(
                "serialized hook payload is {} bytes; maximum is {MAX_EVENT_PAYLOAD_BYTES}",
                payload.len()
            );
        }
        Ok(payload)
    }
}

pub(crate) struct HookQueue {
    sender: mpsc::Sender<HookEvent>,
    dropped: AtomicU64,
}

pub(crate) fn hook_queue() -> (HookQueue, HookReceiver) {
    let (sender, receiver) = mpsc::channel(HOOK_QUEUE_CAPACITY);
    (
        HookQueue {
            sender,
            dropped: AtomicU64::new(0),
        },
        HookReceiver { receiver },
    )
}

impl HookQueue {
    /// Queue committed events without ever applying backpressure to the daemon.
    pub(crate) fn enqueue(&self, mutation: &Mutation) {
        for event in HookEvent::from_mutation(mutation) {
            if self.sender.try_send(event).is_err() {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped.is_power_of_two() {
                    tracing::warn!(
                        dropped,
                        "extension hook queue full; committed event dropped"
                    );
                }
            }
        }
    }
}

pub(crate) struct HookReceiver {
    receiver: mpsc::Receiver<HookEvent>,
}

pub(crate) async fn run_hooks(
    mut queue: HookReceiver,
    extensions: Vec<Extension>,
    fut_bin: PathBuf,
    socket: PathBuf,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let event = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
                continue;
            }
            event = queue.receiver.recv() => match event {
                Some(event) => event,
                None => return,
            },
        };
        run_event(&extensions, &fut_bin, &socket, &event).await;
    }
}

async fn run_event(extensions: &[Extension], fut_bin: &Path, socket: &Path, event: &HookEvent) {
    let payload = match event.payload() {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!(%error, event = event.kind, "extension hook payload rejected");
            return;
        }
    };
    for extension in extensions {
        let Some(command) = extension.hooks.get(event.kind) else {
            continue;
        };
        run_command(extension, command, fut_bin, socket, event, &payload).await;
    }
}

async fn run_command(
    extension: &Extension,
    hook: &ExtensionCommand,
    fut_bin: &Path,
    socket: &Path,
    event: &HookEvent,
    payload: &[u8],
) {
    let mut command = Command::new(&hook.argv[0]);
    command
        .args(&hook.argv[1..])
        .current_dir(&extension.root)
        .envs(extension.command_environment())
        .env(FUT_BIN, fut_bin)
        .env("FUT_EVENT", event.kind)
        .env("FUT_EVENT_VERSION", EVENT_VERSION.to_string())
        .env("FUT_SOCKET", socket)
        .env("FUT_SESSION_ID", event.session_id.to_string())
        .env("FUT_WORKSPACE_ID", event.workspace_id.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(
                extension = extension.id,
                event = event.kind,
                %error,
                "extension hook failed to start"
            );
            return;
        }
    };
    let mut stdin = child.stdin.take().expect("hook stdin was piped");
    let payload = payload.to_owned();
    let input = tokio::spawn(async move {
        stdin.write_all(&payload).await?;
        stdin.shutdown().await
    });
    let stdout = tokio::spawn(drain_output(
        child.stdout.take().expect("hook stdout was piped"),
    ));
    let stderr = tokio::spawn(drain_output(
        child.stderr.take().expect("hook stderr was piped"),
    ));

    let status = match timeout(HOOK_TIMEOUT, child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            let _ = child.start_kill();
            let _ = timeout(Duration::from_millis(100), child.wait()).await;
            input.abort();
            let stdout = finish_output(stdout).await;
            let stderr = finish_output(stderr).await;
            tracing::warn!(
                extension = extension.id,
                event = event.kind,
                timeout_ms = HOOK_TIMEOUT.as_millis(),
                stdout = %stdout,
                stderr = %stderr,
                "extension hook timed out"
            );
            return;
        }
    };
    let input_error = finish_input(input).await;
    let stdout = finish_output(stdout).await;
    let stderr = finish_output(stderr).await;
    match status {
        Ok(status) if status.success() && input_error.is_none() => {
            tracing::debug!(
                extension = extension.id,
                event = event.kind,
                stdout = %stdout,
                stderr = %stderr,
                "extension hook completed"
            );
        }
        Ok(status) => tracing::warn!(
            extension = extension.id,
            event = event.kind,
            exit_status = %status,
            input_error = ?input_error,
            stdout = %stdout,
            stderr = %stderr,
            "extension hook failed"
        ),
        Err(error) => tracing::warn!(
            extension = extension.id,
            event = event.kind,
            %error,
            stdout = %stdout,
            stderr = %stderr,
            "extension hook wait failed"
        ),
    }
}

async fn finish_input(mut task: JoinHandle<std::io::Result<()>>) -> Option<String> {
    match timeout(Duration::from_millis(100), &mut task).await {
        Ok(Ok(Ok(()))) => None,
        Ok(Ok(Err(error))) => Some(error.to_string()),
        Ok(Err(error)) => Some(error.to_string()),
        Err(_) => {
            task.abort();
            Some("stdin writer did not finish".into())
        }
    }
}

struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl std::fmt::Display for CapturedOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let output = String::from_utf8_lossy(&self.bytes);
        if self.truncated {
            write!(formatter, "{output}…[truncated]")
        } else {
            formatter.write_str(&output)
        }
    }
}

async fn drain_output(mut stream: impl AsyncRead + Unpin) -> CapturedOutput {
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut truncated = false;
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let remaining = MAX_OUTPUT_BYTES.saturating_sub(captured.len());
                let retained = remaining.min(read);
                captured.extend_from_slice(&buffer[..retained]);
                truncated |= retained < read;
            }
        }
    }
    CapturedOutput {
        bytes: captured,
        truncated,
    }
}

async fn finish_output(mut task: JoinHandle<CapturedOutput>) -> CapturedOutput {
    match timeout(Duration::from_millis(100), &mut task).await {
        Ok(Ok(output)) => output,
        _ => {
            task.abort();
            CapturedOutput {
                bytes: Vec::new(),
                truncated: true,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PresentationScope {
    Session,
    Workspace,
    Tab,
    Pane,
}

impl PresentationScope {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Workspace => "workspace",
            Self::Tab => "tab",
            Self::Pane => "pane",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PresentationToken {
    name: String,
    scope: PresentationScope,
    qualified_name: String,
}

impl PresentationToken {
    pub(crate) const fn scope(&self) -> PresentationScope {
        self.scope
    }

    /// The collision-free name later publication and rendering work can expose.
    pub(crate) fn qualified_name(&self) -> &str {
        &self.qualified_name
    }
}

/// Validate unstyled materialized presentation text before it enters the
/// resource tree or protocol snapshots.
pub(crate) fn validate_presentation_value(value: &str) -> Result<()> {
    if value.len() > MAX_MATERIALIZED_TOKEN_VALUE_BYTES {
        bail!(
            "presentation token value is {} bytes; maximum is {MAX_MATERIALIZED_TOKEN_VALUE_BYTES}",
            value.len()
        );
    }
    if value.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
    }) {
        bail!("presentation token value contains a control or bidirectional formatting character");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    id: String,
    #[serde(default)]
    hooks: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    commands: BTreeMap<String, CommandDeclaration>,
    #[serde(default)]
    presentation_tokens: Vec<TokenDeclaration>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandDeclaration {
    title: String,
    argv: Vec<String>,
    #[serde(default)]
    size: PopupSize,
    #[serde(default)]
    activate_opened: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenDeclaration {
    name: String,
    scope: PresentationScope,
}

/// Load all configured roots or return an error without exposing a partial set.
pub(crate) fn load(roots: &[PathBuf]) -> Result<Vec<Extension>> {
    if roots.len() > MAX_EXTENSIONS {
        bail!(
            "configured {} extensions; maximum is {MAX_EXTENSIONS}",
            roots.len()
        );
    }

    let mut loaded = Vec::with_capacity(roots.len());
    let mut seen_roots = HashSet::new();
    let mut seen_ids = HashSet::new();

    for (index, configured_root) in roots.iter().enumerate() {
        let root = validate_root(configured_root).with_context(|| {
            format!("validate extensions[{index}] {}", configured_root.display())
        })?;
        if !seen_roots.insert(root.clone()) {
            bail!(
                "extensions[{index}] repeats extension root {}",
                root.display()
            );
        }

        let extension =
            load_one(&root).with_context(|| format!("load extension from {}", root.display()))?;
        if !seen_ids.insert(extension.id.clone()) {
            bail!(
                "extension id {:?} is declared by more than one configured root",
                extension.id
            );
        }
        loaded.push(extension);
    }

    Ok(loaded)
}

fn validate_root(configured_root: &Path) -> Result<PathBuf> {
    if !configured_root.is_absolute() {
        bail!("extension root must be an absolute path");
    }
    validate_os_value(
        "extension root",
        configured_root.as_os_str(),
        MAX_PATH_BYTES,
    )?;
    let root = fs::canonicalize(configured_root)
        .with_context(|| format!("resolve extension root {}", configured_root.display()))?;
    validate_os_value("canonical extension root", root.as_os_str(), MAX_PATH_BYTES)?;
    let metadata = fs::metadata(&root)
        .with_context(|| format!("inspect extension root {}", root.display()))?;
    if !metadata.is_dir() {
        bail!("extension root {} is not a directory", root.display());
    }
    Ok(root)
}

fn load_one(root: &Path) -> Result<Extension> {
    let manifest_path = root.join(MANIFEST_FILE_NAME);
    let source = read_manifest(&manifest_path)?;
    let manifest = toml::from_str::<Manifest>(&source)
        .with_context(|| format!("parse extension manifest {}", manifest_path.display()))?;
    validate_identifier("extension id", &manifest.id)?;
    if manifest.hooks.len() > MAX_HOOKS {
        bail!(
            "extension {:?} declares {} hooks; maximum is {MAX_HOOKS}",
            manifest.id,
            manifest.hooks.len()
        );
    }
    if manifest.commands.len() > MAX_COMMANDS {
        bail!(
            "extension {:?} declares {} commands; maximum is {MAX_COMMANDS}",
            manifest.id,
            manifest.commands.len()
        );
    }
    if manifest.presentation_tokens.len() > MAX_PRESENTATION_TOKENS {
        bail!(
            "extension {:?} declares {} presentation tokens; maximum is {MAX_PRESENTATION_TOKENS}",
            manifest.id,
            manifest.presentation_tokens.len()
        );
    }

    let hooks = manifest
        .hooks
        .into_iter()
        .map(|(name, argv)| {
            validate_identifier("hook name", &name)
                .with_context(|| format!("validate extension {:?} hook {name:?}", manifest.id))?;
            if !SUPPORTED_HOOKS.contains(&name.as_str()) {
                bail!(
                    "extension {:?} declares unsupported hook {name:?}; supported hooks are {}",
                    manifest.id,
                    SUPPORTED_HOOKS.join(", ")
                );
            }
            let command = validate_command(root, argv).with_context(|| {
                format!("validate extension {:?} hook {name:?} argv", manifest.id)
            })?;
            Ok((name, command))
        })
        .collect::<Result<_>>()?;

    let commands = manifest
        .commands
        .into_iter()
        .map(|(name, declaration)| {
            validate_identifier("command name", &name).with_context(|| {
                format!("validate extension {:?} command {name:?}", manifest.id)
            })?;
            validate_title(&declaration.title).with_context(|| {
                format!(
                    "validate extension {:?} command {name:?} title",
                    manifest.id
                )
            })?;
            declaration.size.validate().with_context(|| {
                format!("validate extension {:?} command {name:?} size", manifest.id)
            })?;
            let command = validate_command(root, declaration.argv).with_context(|| {
                format!("validate extension {:?} command {name:?} argv", manifest.id)
            })?;
            let launcher_name = name.clone();
            Ok((
                name,
                ExtensionLauncher {
                    name: launcher_name,
                    title: declaration.title,
                    command,
                    size: declaration.size,
                    activate_opened: declaration.activate_opened,
                },
            ))
        })
        .collect::<Result<_>>()?;

    let mut token_names = HashSet::new();
    let mut presentation_tokens = Vec::with_capacity(manifest.presentation_tokens.len());
    for declaration in manifest.presentation_tokens {
        validate_identifier("presentation token name", &declaration.name).with_context(|| {
            format!(
                "validate extension {:?} presentation token {:?}",
                manifest.id, declaration.name
            )
        })?;
        if !token_names.insert(declaration.name.clone()) {
            bail!(
                "extension {:?} declares presentation token {:?} more than once",
                manifest.id,
                declaration.name
            );
        }
        let qualified_name = format!(
            "{}.extension.{}.{}",
            declaration.scope.as_str(),
            manifest.id,
            declaration.name
        );
        presentation_tokens.push(PresentationToken {
            name: declaration.name,
            scope: declaration.scope,
            qualified_name,
        });
    }

    Ok(Extension {
        id: manifest.id,
        root: root.to_owned(),
        hooks,
        commands,
        presentation_tokens,
    })
}

fn read_manifest(path: &Path) -> Result<String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("read extension manifest {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect extension manifest {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "extension manifest {} is not a regular file",
            path.display()
        );
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        bail!(
            "extension manifest {} is {} bytes; maximum is {MAX_MANIFEST_BYTES}",
            path.display(),
            metadata.len()
        );
    }

    let mut source = String::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_string(&mut source)
        .with_context(|| format!("read extension manifest {} as UTF-8", path.display()))?;
    if source.len() as u64 > MAX_MANIFEST_BYTES {
        bail!(
            "extension manifest {} exceeds the {MAX_MANIFEST_BYTES}-byte maximum",
            path.display()
        );
    }
    Ok(source)
}

fn validate_command(root: &Path, argv: Vec<String>) -> Result<ExtensionCommand> {
    if argv.is_empty() {
        bail!("argv must contain an executable");
    }
    if argv.len() > MAX_ARGV {
        bail!("argv contains {} values; maximum is {MAX_ARGV}", argv.len());
    }
    for (index, value) in argv.iter().enumerate() {
        if value.len() > MAX_ARG_BYTES {
            bail!("argv[{index}] exceeds {MAX_ARG_BYTES} bytes");
        }
        if value.as_bytes().contains(&0) {
            bail!("argv[{index}] contains a NUL byte");
        }
    }
    if argv[0].is_empty() {
        bail!("argv[0] executable must not be empty");
    }

    let mut argv = argv.into_iter().map(OsString::from).collect::<Vec<_>>();
    let executable = Path::new(&argv[0]);
    if executable.as_os_str().as_bytes().starts_with(b"./") {
        let relative = executable.strip_prefix("./").expect("prefix was checked");
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            bail!("argv[0] ./relative executable must stay within the extension root");
        }
        argv[0] = root.join(relative).into_os_string();
    } else if !executable.is_absolute()
        && (executable.components().count() != 1
            || !matches!(executable.components().next(), Some(Component::Normal(_))))
    {
        bail!("argv[0] must be an absolute path, a PATH executable name, or start with ./");
    }
    validate_os_value("resolved argv[0] executable", &argv[0], MAX_PATH_BYTES)?;

    Ok(ExtensionCommand { argv })
}

fn validate_identifier(label: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
        bail!("{label} must be 1 through {MAX_IDENTIFIER_BYTES} bytes");
    }
    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_lowercase() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        bail!(
            "{label} {value:?} must start with a lowercase letter and end with a lowercase letter or digit"
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
                "{label} {value:?} may contain lowercase ASCII letters, digits, and single '.', '_', or '-' separators"
            );
        }
    }
    Ok(())
}

fn validate_title(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_ARG_BYTES {
        bail!("title must be 1 through {MAX_ARG_BYTES} bytes");
    }
    if value.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
    }) {
        bail!("title contains a control or bidirectional formatting character");
    }
    Ok(())
}

fn validate_os_value(label: &str, value: &OsStr, maximum: usize) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.len() > maximum {
        bail!("{label} exceeds {maximum} bytes");
    }
    if bytes.contains(&0) {
        bail!("{label} contains a NUL byte");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use serde_json::json;

    use super::*;

    fn extension_root(manifest: &str) -> tempfile::TempDir {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join(MANIFEST_FILE_NAME), manifest).unwrap();
        temporary
    }

    fn executable_hook(manifest: &str, script: &str) -> tempfile::TempDir {
        let temporary = extension_root(manifest);
        let hook = temporary.path().join("hook");
        fs::write(&hook, script).unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        temporary
    }

    #[test]
    fn loads_namespaced_declarations_and_resolves_packaged_argv() {
        let temporary = extension_root(
            r#"
id = "acme.git-status"

[hooks]
"workspace.created" = ["./bin/refresh", "--quiet"]
"workspace.closed" = ["helper", ""]

[commands.open-review]
title = "Open review"
argv = ["./bin/review", "--interactive"]
size = { width = 100, height = 30 }
activate_opened = true

[[presentation_tokens]]
name = "branch"
scope = "workspace"

[[presentation_tokens]]
name = "review_state"
scope = "tab"
"#,
        );
        fs::create_dir(temporary.path().join("bin")).unwrap();
        fs::write(temporary.path().join("bin/refresh"), "#!/bin/sh\n").unwrap();

        let extensions = load(&[temporary.path().to_owned()]).unwrap();
        let extension = &extensions[0];
        let canonical_root = fs::canonicalize(temporary.path()).unwrap();
        assert_eq!(extension.id(), "acme.git-status");
        assert_eq!(extension.root, canonical_root);
        assert_eq!(
            extension.hooks["workspace.created"].argv,
            [
                canonical_root.join("bin/refresh").into_os_string(),
                OsString::from("--quiet")
            ]
        );
        assert_eq!(
            extension.hooks["workspace.closed"].argv,
            [OsString::from("helper"), OsString::from("")]
        );
        let launcher = &extension.commands["open-review"];
        assert_eq!(launcher.title(), "Open review");
        assert_eq!(launcher.size().width, Some(100));
        assert_eq!(launcher.size().height, Some(30));
        assert!(launcher.activate_opened());
        assert_eq!(
            launcher.argv(),
            [
                canonical_root.join("bin/review").into_os_string(),
                OsString::from("--interactive")
            ]
        );
        assert_eq!(
            extension.presentation_tokens()[0].qualified_name(),
            "workspace.extension.acme.git-status.branch"
        );
        assert_eq!(
            extension.command_environment(),
            [
                (
                    OsString::from("FUT_EXTENSION_ID"),
                    OsString::from("acme.git-status")
                ),
                (
                    OsString::from("FUT_EXTENSION_ROOT"),
                    canonical_root.into_os_string()
                )
            ]
        );
    }

    #[test]
    fn rejects_relative_roots_duplicate_ids_and_duplicate_tokens_atomically() {
        assert!(load(&[PathBuf::from("relative")]).is_err());

        let one = extension_root("id = 'same'\n");
        let two = extension_root("id = 'same'\n");
        let error = load(&[one.path().to_owned(), two.path().to_owned()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("more than one configured root"), "{error}");

        let duplicate = extension_root(
            r#"
id = "duplicate-token"
[[presentation_tokens]]
name = "status"
scope = "workspace"
[[presentation_tokens]]
name = "status"
scope = "tab"
"#,
        );
        let error = format!("{:#}", load(&[duplicate.path().to_owned()]).unwrap_err());
        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn rejects_invalid_ids_manifests_and_argv_with_context() {
        for (manifest, expected) in [
            ("id = 'Uppercase'\n", "extension id"),
            ("id = 'valid'\nunknown = true\n", "parse extension manifest"),
            (
                "id = 'valid'\n[hooks]\n'workspace.created' = []\n",
                "argv must contain an executable",
            ),
            (
                "id = 'valid'\n[commands.launch]\ntitle = 'Launch'\nargv = []\n",
                "argv must contain an executable",
            ),
            (
                "id = 'valid'\n[commands.launch]\ntitle = ''\nargv = ['/bin/true']\n",
                "title must be",
            ),
            (
                "id = 'valid'\n[commands.launch]\ntitle = 'Launch'\nargv = ['/bin/true']\nsize = { width = 3 }\n",
                "size.width must be at least 4",
            ),
            (
                "id = 'valid'\n[hooks]\n'workspace.created' = ['./../escape']\n",
                "must stay within the extension root",
            ),
            (
                "id = 'valid'\n[hooks]\n'workspace.created' = ['../escape']\n",
                "must be an absolute path, a PATH executable name, or start with ./",
            ),
            (
                "id = 'valid'\n[hooks]\n'workspace.changed' = ['helper']\n",
                "unsupported hook",
            ),
        ] {
            let temporary = extension_root(manifest);
            let error = format!("{:#}", load(&[temporary.path().to_owned()]).unwrap_err());
            assert!(error.contains(expected), "{error}");
            assert!(
                error.contains(&temporary.path().display().to_string()),
                "{error}"
            );
        }
    }

    #[test]
    fn bounds_extension_and_manifest_sizes() {
        let roots = (0..=MAX_EXTENSIONS)
            .map(|_| PathBuf::from("/not-loaded"))
            .collect::<Vec<_>>();
        assert!(load(&roots).unwrap_err().to_string().contains("maximum"));

        let temporary = tempfile::tempdir().unwrap();
        fs::write(
            temporary.path().join(MANIFEST_FILE_NAME),
            vec![b' '; MAX_MANIFEST_BYTES as usize + 1],
        )
        .unwrap();
        let error = format!("{:#}", load(&[temporary.path().to_owned()]).unwrap_err());
        assert!(error.contains("maximum"), "{error}");
    }

    #[test]
    fn hook_payload_is_exact_and_versioned() {
        let event = HookEvent {
            revision: 42,
            kind: "workspace.renamed",
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            workspace_name: "new name".into(),
            workspace_root: "/work/tree".into(),
            previous_name: Some("old name".into()),
        };

        let payload: serde_json::Value = serde_json::from_slice(&event.payload().unwrap()).unwrap();
        assert_eq!(
            payload,
            json!({
                "version": 1,
                "event": "workspace.renamed",
                "resource_revision": 42,
                "session_id": event.session_id,
                "workspace": {
                    "id": event.workspace_id,
                    "name": "new name",
                    "root": "/work/tree",
                },
                "previous_name": "old name",
            })
        );
    }

    #[tokio::test]
    async fn hook_receives_direct_argv_cwd_environment_and_stdin() {
        let temporary = executable_hook(
            "id = 'capture'\n[hooks]\n'workspace.created' = ['./hook', 'literal argument']\n",
            "#!/bin/sh\npwd > cwd\nenv | sort > environment\nprintf '%s' \"$1\" > argument\ncat > payload\n",
        );
        let extension = load(&[temporary.path().to_owned()]).unwrap().remove(0);
        let event = HookEvent {
            revision: 7,
            kind: "workspace.created",
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            workspace_name: "main".into(),
            workspace_root: "/workspace".into(),
            previous_name: None,
        };
        let socket = temporary.path().join("fut.sock");

        let fut_bin = Path::new("/opt/fut/bin/fut");
        run_event(&[extension], fut_bin, &socket, &event).await;

        assert_eq!(
            fs::read_to_string(temporary.path().join("cwd"))
                .unwrap()
                .trim(),
            fs::canonicalize(temporary.path())
                .unwrap()
                .to_str()
                .unwrap()
        );
        assert_eq!(
            fs::read_to_string(temporary.path().join("argument")).unwrap(),
            "literal argument"
        );
        let environment = fs::read_to_string(temporary.path().join("environment")).unwrap();
        for expected in [
            format!("FUT_BIN={}", fut_bin.display()),
            format!("FUT_EVENT={}", event.kind),
            format!("FUT_EVENT_VERSION={EVENT_VERSION}"),
            "FUT_EXTENSION_ID=capture".to_owned(),
            format!(
                "FUT_EXTENSION_ROOT={}",
                fs::canonicalize(temporary.path()).unwrap().display()
            ),
            format!("FUT_SESSION_ID={}", event.session_id),
            format!("FUT_SOCKET={}", socket.display()),
            format!("FUT_WORKSPACE_ID={}", event.workspace_id),
        ] {
            assert!(
                environment.lines().any(|line| line == expected),
                "{expected}"
            );
        }
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &fs::read_to_string(temporary.path().join("payload")).unwrap()
            )
            .unwrap()["event"],
            event.kind
        );
    }

    #[tokio::test]
    async fn nonzero_hook_does_not_stop_later_extensions() {
        let output = tempfile::NamedTempFile::new().unwrap();
        let output_path = output.path().display().to_string();
        let first = executable_hook(
            &format!(
                "id = 'first'\n[hooks]\n'workspace.closed' = ['./hook', {:?}]\n",
                output_path
            ),
            "#!/bin/sh\nprintf 'first\\n' >> \"$1\"\nexit 9\n",
        );
        let second = executable_hook(
            &format!(
                "id = 'second'\n[hooks]\n'workspace.closed' = ['./hook', {:?}]\n",
                output_path
            ),
            "#!/bin/sh\nprintf 'second\\n' >> \"$1\"\n",
        );
        let extensions = load(&[first.path().to_owned(), second.path().to_owned()]).unwrap();
        let event = HookEvent {
            revision: 9,
            kind: "workspace.closed",
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            workspace_name: "main".into(),
            workspace_root: "/workspace".into(),
            previous_name: None,
        };

        run_event(
            &extensions,
            Path::new("/opt/fut/bin/fut"),
            Path::new("/tmp/fut.sock"),
            &event,
        )
        .await;

        assert_eq!(
            fs::read_to_string(output.path()).unwrap(),
            "first\nsecond\n"
        );
    }

    #[tokio::test]
    async fn timed_out_hook_is_killed_and_does_not_stop_later_extensions() {
        let output = tempfile::NamedTempFile::new().unwrap();
        let output_path = output.path().display().to_string();
        let slow = executable_hook(
            &format!(
                "id = 'slow'\n[hooks]\n'workspace.created' = ['./hook', {:?}]\n",
                output_path
            ),
            "#!/bin/sh\nprintf 'slow\\n' >> \"$1\"\nsleep 30\n",
        );
        let later = executable_hook(
            &format!(
                "id = 'later'\n[hooks]\n'workspace.created' = ['./hook', {:?}]\n",
                output_path
            ),
            "#!/bin/sh\nprintf 'later\\n' >> \"$1\"\n",
        );
        let extensions = load(&[slow.path().to_owned(), later.path().to_owned()]).unwrap();
        let event = HookEvent {
            revision: 10,
            kind: "workspace.created",
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            workspace_name: "main".into(),
            workspace_root: "/workspace".into(),
            previous_name: None,
        };
        let started = tokio::time::Instant::now();

        run_event(
            &extensions,
            Path::new("/opt/fut/bin/fut"),
            Path::new("/tmp/fut.sock"),
            &event,
        )
        .await;

        assert!(started.elapsed() >= HOOK_TIMEOUT);
        assert_eq!(fs::read_to_string(output.path()).unwrap(), "slow\nlater\n");
    }

    #[test]
    fn materialized_presentation_text_is_bounded_and_safe() {
        validate_presentation_value("ready λ").unwrap();
        assert!(validate_presentation_value("line\nbreak").is_err());
        assert!(validate_presentation_value("unsafe \u{202e} text").is_err());
        assert!(
            validate_presentation_value(&"x".repeat(MAX_MATERIALIZED_TOKEN_VALUE_BYTES + 1))
                .is_err()
        );
    }
}
