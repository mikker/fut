use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
};

use tokio::{
    process::Command,
    sync::watch,
    task::{JoinHandle, JoinSet},
    time::{self, Duration, Instant},
};

use crate::domain::WorkspaceId;

use super::Shared;

pub(super) const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct GitStatus {
    branch: String,
    insertions: usize,
    deletions: usize,
}

#[derive(Debug)]
struct Entry {
    root: PathBuf,
    requested: Instant,
    in_flight: bool,
    token: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Claim {
    workspace_id: WorkspaceId,
    root: PathBuf,
    token: u64,
}

#[derive(Default)]
struct RefreshState {
    entries: HashMap<WorkspaceId, Entry>,
    next_token: u64,
}

impl RefreshState {
    /// Reconcile against the authoritative workspace set. The monotonically
    /// changing token binds a completion to one exact entry, including across
    /// close-and-recreate cycles that reuse a workspace ID or root.
    fn reconcile(&mut self, roots: &BTreeMap<WorkspaceId, PathBuf>) -> Vec<Claim> {
        self.entries.retain(|workspace_id, entry| {
            roots
                .get(workspace_id)
                .is_some_and(|root| root == &entry.root)
        });

        let mut claims = Vec::new();
        for (workspace_id, root) in roots {
            let due = self.entries.get(workspace_id).is_none_or(|entry| {
                !entry.in_flight && entry.requested.elapsed() >= REFRESH_INTERVAL
            });
            if !due {
                continue;
            }
            self.next_token = self.next_token.wrapping_add(1);
            let token = self.next_token;
            if let Some(entry) = self.entries.get_mut(workspace_id) {
                entry.requested = Instant::now();
                entry.in_flight = true;
                entry.token = token;
            } else {
                self.entries.insert(
                    *workspace_id,
                    Entry {
                        root: root.clone(),
                        requested: Instant::now(),
                        in_flight: true,
                        token,
                    },
                );
            }
            claims.push(Claim {
                workspace_id: *workspace_id,
                root: root.clone(),
                token,
            });
        }
        claims
    }

    fn complete(&mut self, claim: &Claim) -> bool {
        let Some(entry) = self.entries.get_mut(&claim.workspace_id) else {
            return false;
        };
        if !entry.in_flight || entry.root != claim.root || entry.token != claim.token {
            return false;
        }
        entry.in_flight = false;
        true
    }
}

pub(super) fn watch(
    shared: Shared,
    resource_changes: watch::Receiver<u64>,
    shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(run(shared, resource_changes, shutdown))
}

async fn run(
    shared: Shared,
    mut resource_changes: watch::Receiver<u64>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut interval = time::interval(REFRESH_INTERVAL);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut refreshes = RefreshState::default();
    let mut tasks = JoinSet::new();

    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
            changed = resource_changes.changed() => {
                if changed.is_err() {
                    return;
                }
                let _ = *resource_changes.borrow_and_update();
                start_due_refreshes(&shared, &mut refreshes, &mut tasks).await;
            }
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Ok((claim, status))) = completed
                    && refreshes.complete(&claim)
                {
                    publish(&shared, claim, status).await;
                }
            }
            _ = interval.tick() => {
                start_due_refreshes(&shared, &mut refreshes, &mut tasks).await;
            }
        }
    }
}

async fn start_due_refreshes(
    shared: &Shared,
    refreshes: &mut RefreshState,
    tasks: &mut JoinSet<(Claim, Option<GitStatus>)>,
) {
    // Status is resolved at each workspace's live location — the work tree
    // (or directory) every open pane is inside — so branch and diff counts
    // follow a `cd` instead of the creation-time root. Workspaces whose panes
    // disagree have no single location; they read "multiple" until the panes
    // converge again, and dropping them from the map prunes their entry so
    // convergence refreshes immediately.
    let (roots, divided) = {
        let snapshot = shared.lock().await.resources.snapshot();
        let mut roots = BTreeMap::new();
        let mut divided = Vec::new();
        for workspace in snapshot
            .sessions
            .iter()
            .filter(|session| !session.closing)
            .flat_map(|session| &session.workspaces)
            .filter(|workspace| !workspace.closing)
        {
            match crate::resources::shared_live_location(&workspace.root, &workspace.tabs) {
                Some(location) => {
                    roots.insert(workspace.id, location.to_path_buf());
                }
                None => divided.push(workspace.id),
            }
        }
        (roots, divided)
    };
    for workspace_id in divided {
        let mut state = shared.lock().await;
        if let Ok(publication) = state.resources.publish_workspace_git_tokens(
            workspace_id,
            Some(crate::resources::MULTIPLE_LOCATIONS.into()),
            None,
            None,
        ) && publication.changed
        {
            state.publish_resource_change(publication.revision);
        }
    }
    for claim in refreshes.reconcile(&roots) {
        tasks.spawn(async move {
            let status = status(&claim.root).await;
            (claim, status)
        });
    }
}

async fn publish(shared: &Shared, claim: Claim, status: Option<GitStatus>) {
    let (branch, added, deleted) = status.map_or((None, None, None), |status| {
        (
            (!status.branch.is_empty()).then_some(status.branch),
            (status.insertions > 0).then(|| format!("+{}", status.insertions)),
            (status.deletions > 0).then(|| format!("-{}", status.deletions)),
        )
    });
    let mut state = shared.lock().await;
    let snapshot = state.resources.snapshot();
    let current = snapshot
        .sessions
        .iter()
        .flat_map(|session| &session.workspaces)
        .find(|workspace| workspace.id == claim.workspace_id)
        .and_then(|workspace| {
            crate::resources::shared_live_location(&workspace.root, &workspace.tabs)
        });
    if current != Some(claim.root.as_path()) {
        return;
    }
    if let Ok(publication) =
        state
            .resources
            .publish_workspace_git_tokens(claim.workspace_id, branch, added, deleted)
        && publication.changed
    {
        state.publish_resource_change(publication.revision);
    }
}

async fn status(root: &Path) -> Option<GitStatus> {
    let branch = run_git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    let shortstat = run_git(root, &["diff", "HEAD", "--shortstat"]).await;
    parsed_status(branch, shortstat)
}

fn parsed_status(branch: Option<String>, shortstat: Option<String>) -> Option<GitStatus> {
    Some(GitStatus {
        branch: branch?.trim().to_owned(),
        ..parse_shortstat(&shortstat?)
    })
}

async fn run_git(root: &Path, arguments: &[&str]) -> Option<String> {
    let child = Command::new("git")
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let output = time::timeout(COMMAND_TIMEOUT, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_shortstat(summary: &str) -> GitStatus {
    let mut status = GitStatus::default();
    for field in summary.split(',') {
        let mut words = field.split_whitespace();
        let Some(count) = words.next().and_then(|word| word.parse::<usize>().ok()) else {
            continue;
        };
        match words.next() {
            Some(word) if word.starts_with("insertion") => status.insertions = count,
            Some(word) if word.starts_with("deletion") => status.deletions = count,
            _ => {}
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn git(root: &Path, arguments: &[&str]) {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(root)
            .output()
            .expect("git runs");
        assert!(output.status.success(), "git {arguments:?} failed");
    }

    #[test]
    fn shortstat_parsing_reads_counts_and_tolerates_missing_lanes() {
        assert_eq!(
            parse_shortstat(" 2 files changed, 5 insertions(+), 1 deletion(-)\n"),
            GitStatus {
                branch: String::new(),
                insertions: 5,
                deletions: 1,
            }
        );
        assert_eq!(
            parse_shortstat(" 1 file changed, 3 deletions(-)\n"),
            GitStatus {
                branch: String::new(),
                insertions: 0,
                deletions: 3,
            }
        );
        assert_eq!(parse_shortstat(""), GitStatus::default());
    }

    #[test]
    fn failed_git_lanes_do_not_produce_a_clean_status() {
        assert_eq!(parsed_status(Some("main\n".into()), None), None);
        assert_eq!(parsed_status(None, Some(String::new())), None);
    }

    #[test]
    fn new_workspaces_are_immediate_and_stale_completions_are_ignored() {
        let mut state = RefreshState::default();
        let workspace_id = WorkspaceId::new();
        let root = PathBuf::from("/repository");
        let old = state.reconcile(&BTreeMap::from([(workspace_id, root.clone())]))[0].clone();
        assert!(
            state
                .reconcile(&BTreeMap::from([(workspace_id, root.clone())]))
                .is_empty(),
            "a fresh request is not repeated"
        );

        state.reconcile(&BTreeMap::new());
        let current = state.reconcile(&BTreeMap::from([(workspace_id, root)]))[0].clone();
        assert_ne!(old.token, current.token);
        assert!(!state.complete(&old));
        assert!(state.entries[&workspace_id].in_flight);
        assert!(state.complete(&current));
    }

    #[tokio::test]
    async fn status_reads_a_repository_and_non_git_is_empty() {
        let repository = TempDir::new().expect("temporary repository");
        let root = repository.path();
        git(root, &["init", "--initial-branch=main"]);
        git(root, &["config", "user.name", "Fut Test"]);
        git(root, &["config", "user.email", "fut@example.test"]);
        fs::write(root.join("tracked"), "first\n").expect("write fixture");
        git(root, &["add", "tracked"]);
        git(root, &["commit", "-m", "fixture"]);
        fs::write(root.join("tracked"), "first\nsecond\n").expect("change fixture");

        assert_eq!(
            status(root).await,
            Some(GitStatus {
                branch: "main".into(),
                insertions: 1,
                deletions: 0,
            })
        );
        assert_eq!(status(&root.join("missing")).await, None);
    }
}
