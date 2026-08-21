//! Filesystem boundary which turns an arbitrary working directory into Fut's
//! project and workspace identities.

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    task::JoinHandle,
    time,
};

use crate::resources::{Project, ProjectIdentity};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const OUTPUT_LIMIT: usize = 64 * 1024;
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
    "GIT_CONFIG",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_ATTRIBUTES_FILE",
    "GIT_ATTR_NOSYSTEM",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceKind {
    GitCheckout,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedLocation {
    pub cwd: PathBuf,
    pub project: Project,
    pub workspace_root: PathBuf,
    pub suggested_session_name: String,
    pub workspace_kind: WorkspaceKind,
}

#[derive(Clone, Debug)]
pub struct ProjectResolver {
    git_executable: OsString,
    command_timeout: Duration,
}

impl Default for ProjectResolver {
    fn default() -> Self {
        Self {
            git_executable: "git".into(),
            command_timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl ProjectResolver {
    pub fn with_git_executable(executable: impl Into<OsString>) -> Self {
        Self {
            git_executable: executable.into(),
            ..Self::default()
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    pub async fn resolve(
        &self,
        directory: impl AsRef<Path>,
    ) -> Result<ResolvedLocation, ProjectError> {
        let supplied = directory.as_ref();
        let metadata = std::fs::metadata(supplied).map_err(|source| ProjectError::Input {
            path: supplied.to_owned(),
            source,
        })?;
        if !metadata.is_dir() {
            return Err(ProjectError::NotDirectory(supplied.to_owned()));
        }
        let cwd = std::fs::canonicalize(supplied).map_err(|source| ProjectError::Canonicalize {
            path: supplied.to_owned(),
            source,
        })?;

        let classification = self
            .git(&cwd, &["rev-parse", "--is-bare-repository"])
            .await?;
        if !classification.success {
            if is_not_repository(&classification.stderr) {
                return Ok(directory_location(cwd));
            }
            return Err(ProjectError::GitFailure {
                status: classification.status,
                stderr: printable(&classification.stderr),
            });
        }
        let bare = required_line(&classification.stdout, "bare repository classification")?;
        if bare == b"true" {
            return Err(ProjectError::BareRepository(cwd));
        }
        if bare != b"false" {
            return Err(ProjectError::InvalidGitOutput(
                "bare repository classification",
            ));
        }

        let paths = self
            .git(
                &cwd,
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--show-toplevel",
                    "--git-common-dir",
                ],
            )
            .await?;
        if !paths.success {
            return Err(ProjectError::GitFailure {
                status: paths.status,
                stderr: printable(&paths.stderr),
            });
        }
        let mut lines = paths.stdout.split(|byte| *byte == b'\n');
        let root = required_path(lines.next().unwrap_or_default(), "worktree root")?;
        let common = required_path(lines.next().unwrap_or_default(), "Git common directory")?;
        if lines.any(|line| !line.is_empty()) {
            return Err(ProjectError::InvalidGitOutput("repository paths"));
        }
        let workspace_root = canonical_git_path(root, "worktree root")?;
        let common_dir = canonical_git_path(common, "Git common directory")?;

        Ok(ResolvedLocation {
            suggested_session_name: git_session_name(&common_dir, &workspace_root),
            cwd,
            project: Project {
                identity: ProjectIdentity::GitCommonDir(common_dir),
            },
            workspace_root,
            workspace_kind: WorkspaceKind::GitCheckout,
        })
    }

    async fn git(&self, cwd: &Path, arguments: &[&str]) -> Result<GitOutput, ProjectError> {
        let mut command = Command::new(&self.git_executable);
        command
            .args(arguments)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C");
        sanitize_git_environment(&mut command, std::env::vars_os());
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|source| match source.kind() {
            io::ErrorKind::NotFound => ProjectError::GitNotFound(self.git_executable.clone()),
            _ => ProjectError::GitSpawn(source),
        })?;
        let process_group = child.id();
        let stdout = child.stdout.take().expect("piped Git stdout");
        let stderr = child.stderr.take().expect("piped Git stderr");
        let mut stdout = tokio::spawn(read_bounded(stdout));
        let mut stderr = tokio::spawn(read_bounded(stderr));
        let result = time::timeout(self.command_timeout, async {
            let status = child.wait().await.map_err(ProjectError::GitWait)?;
            let stdout = (&mut stdout)
                .await
                .map_err(|_| ProjectError::GitOutputTask)??;
            let stderr = (&mut stderr)
                .await
                .map_err(|_| ProjectError::GitOutputTask)??;
            Ok(GitOutput {
                success: status.success(),
                status: status.code(),
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
                Err(ProjectError::GitTimeout(self.command_timeout))
            }
        }
    }
}

fn sanitize_git_environment(
    command: &mut Command,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) {
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
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device());
}

#[cfg(unix)]
fn null_device() -> &'static str {
    "/dev/null"
}

#[cfg(windows)]
fn null_device() -> &'static str {
    "NUL"
}

#[cfg(not(any(unix, windows)))]
fn null_device() -> &'static str {
    "/dev/null"
}

async fn clean_up_git<R>(
    child: &mut Child,
    process_group: Option<u32>,
    stdout: &mut JoinHandle<R>,
    stderr: &mut JoinHandle<R>,
) {
    #[cfg(unix)]
    if let Some(pid) = process_group {
        // Each command is spawned as the leader of this group. The guard makes
        // it impossible to signal Fut's own process group if spawning changes.
        // SAFETY: getpgrp/kill retain no pointers and pid came from Child.
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

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("cannot inspect input directory {path}: {source}")]
    Input {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("input path is not a directory: {0}")]
    NotDirectory(PathBuf),
    #[error("cannot canonicalize input directory {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Git executable was not found: {0:?}")]
    GitNotFound(OsString),
    #[error("could not start Git: {0}")]
    GitSpawn(#[source] io::Error),
    #[error("Git command timed out after {0:?}")]
    GitTimeout(Duration),
    #[error("Git command failed (status {status:?}): {stderr}")]
    GitFailure { status: Option<i32>, stderr: String },
    #[error("Git returned invalid or missing {0}")]
    InvalidGitOutput(&'static str),
    #[error("Git output exceeded {OUTPUT_LIMIT} bytes")]
    GitOutputTooLarge,
    #[error("could not read Git output: {0}")]
    GitOutput(#[source] io::Error),
    #[error("Git output reader stopped unexpectedly")]
    GitOutputTask,
    #[error("could not wait for Git: {0}")]
    GitWait(#[source] io::Error),
    #[error("bare repositories cannot be opened as workspaces: {0}")]
    BareRepository(PathBuf),
    #[error("cannot canonicalize Git {kind} {path}: {source}")]
    GitPath {
        kind: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

struct GitOutput {
    success: bool,
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin) -> Result<Vec<u8>, ProjectError> {
    let mut kept = Vec::new();
    let mut chunk = [0; 8192];
    let mut oversized = false;
    loop {
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(ProjectError::GitOutput)?;
        if read == 0 {
            break;
        }
        if kept.len() + read > OUTPUT_LIMIT {
            oversized = true;
        } else if !oversized {
            kept.extend_from_slice(&chunk[..read]);
        }
    }
    if oversized {
        Err(ProjectError::GitOutputTooLarge)
    } else {
        Ok(kept)
    }
}

fn is_not_repository(stderr: &[u8]) -> bool {
    stderr.starts_with(b"fatal: not a git repository (") && !stderr.contains(&0)
}

fn required_line<'a>(output: &'a [u8], kind: &'static str) -> Result<&'a [u8], ProjectError> {
    let line = output.strip_suffix(b"\n").unwrap_or(output);
    if line.is_empty() || line.contains(&0) || line.contains(&b'\n') {
        Err(ProjectError::InvalidGitOutput(kind))
    } else {
        Ok(line)
    }
}

fn required_path(bytes: &[u8], kind: &'static str) -> Result<PathBuf, ProjectError> {
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(ProjectError::InvalidGitOutput(kind));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes.to_vec())
            .map(PathBuf::from)
            .map_err(|_| ProjectError::InvalidGitOutput(kind))
    }
}

fn canonical_git_path(path: PathBuf, kind: &'static str) -> Result<PathBuf, ProjectError> {
    std::fs::canonicalize(&path).map_err(|source| ProjectError::GitPath { kind, path, source })
}

fn directory_location(cwd: PathBuf) -> ResolvedLocation {
    let name = basename(&cwd);
    ResolvedLocation {
        project: Project {
            identity: ProjectIdentity::CanonicalDirectory(cwd.clone()),
        },
        workspace_root: cwd.clone(),
        suggested_session_name: name,
        cwd,
        workspace_kind: WorkspaceKind::Directory,
    }
}

fn git_session_name(common_dir: &Path, root: &Path) -> String {
    if common_dir.file_name().is_some_and(|name| name == ".git") {
        common_dir
            .parent()
            .map(basename)
            .unwrap_or_else(|| basename(root))
    } else {
        basename(root)
    }
}

fn basename(path: &Path) -> String {
    path.file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn printable(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn git(root: &Path, arguments: &[&str]) {
        let status = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=Fut Test",
                "-c",
                "user.email=fut@example.invalid",
            ])
            .args(arguments)
            .current_dir(root)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn non_git_directory_uses_its_canonical_identity() {
        let temp = TempDir::new().unwrap();
        let resolved = ProjectResolver::default()
            .resolve(temp.path())
            .await
            .unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        assert_eq!(resolved.cwd, canonical);
        assert_eq!(resolved.workspace_root, canonical);
        assert_eq!(
            resolved.project.identity,
            ProjectIdentity::CanonicalDirectory(canonical)
        );
        assert_eq!(resolved.workspace_kind, WorkspaceKind::Directory);
    }

    #[tokio::test]
    async fn unborn_checkout_resolves_from_nested_directory() {
        let temp = TempDir::new().unwrap();
        git(temp.path(), &["init", "-b", "main"]);
        let nested = temp.path().join("one/two");
        std::fs::create_dir_all(&nested).unwrap();
        let resolved = ProjectResolver::default().resolve(&nested).await.unwrap();
        assert_eq!(resolved.cwd, nested.canonicalize().unwrap());
        assert_eq!(resolved.workspace_root, temp.path().canonicalize().unwrap());
        assert_eq!(resolved.suggested_session_name, basename(temp.path()));
    }

    #[tokio::test]
    async fn linked_worktrees_share_identity_but_not_roots() {
        let temp = TempDir::new().unwrap();
        let main = temp.path().join("main");
        let linked = temp.path().join("linked");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-b", "main"]);
        std::fs::write(main.join("file"), "content").unwrap();
        git(&main, &["add", "file"]);
        git(&main, &["commit", "-m", "initial"]);
        git(
            &main,
            &["worktree", "add", "-b", "topic", linked.to_str().unwrap()],
        );
        let resolver = ProjectResolver::default();
        let a = resolver.resolve(&main).await.unwrap();
        let b = resolver.resolve(&linked).await.unwrap();
        assert_eq!(a.project.identity, b.project.identity);
        assert_ne!(a.workspace_root, b.workspace_root);
        assert_eq!(a.suggested_session_name, b.suggested_session_name);
    }

    #[tokio::test]
    async fn nested_repository_has_an_independent_identity() {
        let outer = TempDir::new().unwrap();
        git(outer.path(), &["init", "-b", "main"]);
        let inner = outer.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        git(&inner, &["init", "-b", "main"]);
        let resolver = ProjectResolver::default();
        assert_ne!(
            resolver
                .resolve(outer.path())
                .await
                .unwrap()
                .project
                .identity,
            resolver.resolve(&inner).await.unwrap().project.identity
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_alias_preserves_canonical_cwd() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        git(temp.path(), &["init", "-b", "main"]);
        let alias = temp
            .path()
            .parent()
            .unwrap()
            .join(format!("fut-alias-{}", std::process::id()));
        symlink(temp.path(), &alias).unwrap();
        let resolved = ProjectResolver::default().resolve(&alias).await.unwrap();
        std::fs::remove_file(alias).unwrap();
        assert_eq!(resolved.cwd, temp.path().canonicalize().unwrap());
    }

    #[tokio::test]
    async fn bare_repository_is_rejected() {
        let temp = TempDir::new().unwrap();
        git(temp.path(), &["init", "--bare"]);
        assert!(matches!(
            ProjectResolver::default().resolve(temp.path()).await,
            Err(ProjectError::BareRepository(_))
        ));
    }

    #[tokio::test]
    async fn missing_and_file_inputs_are_rejected_before_git() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(matches!(
            ProjectResolver::default()
                .resolve(temp.path().join("missing"))
                .await,
            Err(ProjectError::Input { .. })
        ));
        assert!(matches!(
            ProjectResolver::default().resolve(file).await,
            Err(ProjectError::NotDirectory(_))
        ));
    }

    #[tokio::test]
    async fn configured_missing_git_is_typed() {
        let temp = TempDir::new().unwrap();
        let resolver = ProjectResolver::with_git_executable(temp.path().join("absent-git"));
        assert!(matches!(
            resolver.resolve(temp.path()).await,
            Err(ProjectError::GitNotFound(_))
        ));
    }

    #[cfg(unix)]
    fn script(contents: &str) -> (TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("git");
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        (temp, path)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unexpected_git_failure_is_not_a_directory_fallback() {
        let temp = TempDir::new().unwrap();
        let (_scripts, fake) = script("#!/bin/sh\necho 'fatal: corrupt repository' >&2\nexit 7\n");
        let result = ProjectResolver::with_git_executable(fake)
            .resolve(temp.path())
            .await;
        assert!(matches!(
            result,
            Err(ProjectError::GitFailure {
                status: Some(7),
                ..
            })
        ));
    }

    /// Far above worst-case shell startup on a loaded machine: the resolver
    /// must never kill the fake git before it records the descendant pid, or
    /// [`assert_process_dies`] has no pid file to read.
    #[cfg(unix)]
    const KILL_TIMEOUT: Duration = Duration::from_millis(500);

    #[cfg(unix)]
    #[tokio::test]
    async fn slow_git_is_killed_at_the_timeout() {
        let temp = TempDir::new().unwrap();
        let (_scripts, fake) = script("#!/bin/sh\nsleep 10 &\necho $! > descendant.pid\nwait\n");
        let resolver = ProjectResolver::with_git_executable(fake).with_timeout(KILL_TIMEOUT);
        assert!(matches!(
            resolver.resolve(temp.path()).await,
            Err(ProjectError::GitTimeout(_))
        ));
        assert_process_dies(&temp.path().join("descendant.pid")).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exited_git_with_inherited_pipe_is_killed_at_the_timeout() {
        let temp = TempDir::new().unwrap();
        let (_scripts, fake) = script("#!/bin/sh\nsleep 10 &\necho $! > descendant.pid\nexit 0\n");
        let resolver = ProjectResolver::with_git_executable(fake).with_timeout(KILL_TIMEOUT);
        assert!(matches!(
            resolver.resolve(temp.path()).await,
            Err(ProjectError::GitTimeout(_))
        ));
        assert_process_dies(&temp.path().join("descendant.pid")).await;
    }

    #[cfg(unix)]
    async fn assert_process_dies(pid_file: &Path) {
        let pid: libc::pid_t = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        for _ in 0..100 {
            // SAFETY: signal zero only checks a numeric process id.
            if unsafe { libc::kill(pid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        // Ensure a failed test cannot leave the injected sleeper behind.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        panic!("Git descendant {pid} survived process-group cleanup");
    }

    #[test]
    fn git_configuration_overrides_are_sanitized_without_mutating_the_environment() {
        let mut command = Command::new("git");
        sanitize_git_environment(
            &mut command,
            [
                (
                    OsString::from("GIT_CONFIG_KEY_7"),
                    OsString::from("core.hooksPath"),
                ),
                (
                    OsString::from("GIT_CONFIG_VALUE_7"),
                    OsString::from("hostile"),
                ),
            ],
        );
        let environment: std::collections::HashMap<_, _> = command
            .as_std()
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(OsString::from)))
            .collect();
        assert_eq!(
            environment.get(std::ffi::OsStr::new("GIT_CONFIG_KEY_7")),
            Some(&None)
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new("GIT_CONFIG_VALUE_7")),
            Some(&None)
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new("GIT_CONFIG_NOSYSTEM")),
            Some(&Some(OsString::from("1")))
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new("GIT_CONFIG_GLOBAL")),
            Some(&Some(OsString::from(null_device())))
        );
    }
}
