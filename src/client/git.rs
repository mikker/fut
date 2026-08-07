use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::{process::Command, sync::mpsc, time};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct GitStatus {
    pub branch: String,
    pub insertions: usize,
    pub deletions: usize,
}

struct Entry {
    requested: Instant,
    status: Option<GitStatus>,
}

/// Branch and working-tree diff size per workspace root, resolved by bounded
/// background processes so neither the render nor the event loop waits for git.
#[derive(Clone, Default)]
pub(super) struct GitStatusCache {
    entries: Arc<Mutex<HashMap<PathBuf, Entry>>>,
    updates: Option<mpsc::Sender<()>>,
}

impl GitStatusCache {
    pub fn new(updates: mpsc::Sender<()>) -> Self {
        Self {
            entries: Arc::default(),
            updates: Some(updates),
        }
    }

    pub fn status(&self, root: &Path) -> Option<GitStatus> {
        self.entries.lock().ok()?.get(root)?.status.clone()
    }

    pub fn refresh<'a>(&self, roots: impl Iterator<Item = &'a Path>) {
        for root in roots {
            if !self.claim(root) {
                continue;
            }
            let cache = self.clone();
            let root = root.to_path_buf();
            tokio::spawn(async move {
                let status = status(&root).await;
                cache.store(&root, status);
            });
        }
    }

    fn claim(&self, root: &Path) -> bool {
        let Ok(mut entries) = self.entries.lock() else {
            return false;
        };
        match entries.get_mut(root) {
            Some(entry) if entry.requested.elapsed() < REFRESH_INTERVAL => false,
            Some(entry) => {
                entry.requested = Instant::now();
                true
            }
            None => {
                entries.insert(
                    root.to_owned(),
                    Entry {
                        requested: Instant::now(),
                        status: None,
                    },
                );
                true
            }
        }
    }

    fn store(&self, root: &Path, status: Option<GitStatus>) {
        if let Ok(mut entries) = self.entries.lock()
            && let Some(entry) = entries.get_mut(root)
        {
            entry.status = status;
        }
        if let Some(updates) = self.updates.as_ref() {
            let _ = updates.try_send(());
        }
    }
}

async fn status(root: &Path) -> Option<GitStatus> {
    let branch = run(root, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    let shortstat = run(root, &["diff", "HEAD", "--shortstat"])
        .await
        .unwrap_or_default();
    Some(GitStatus {
        branch: branch.trim().to_owned(),
        ..parse_shortstat(&shortstat)
    })
}

async fn run(root: &Path, arguments: &[&str]) -> Option<String> {
    let child = Command::new("git")
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let output = time::timeout(GIT_TIMEOUT, child.wait_with_output())
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
    use super::*;

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

    #[tokio::test]
    async fn missing_roots_resolve_to_nothing_and_refresh_is_throttled() {
        let cache = GitStatusCache::default();
        let root = Path::new("/definitely/missing/fut-workspace");
        cache.refresh(std::iter::once(root));
        assert!(!cache.claim(root), "a fresh request is not repeated");
        for _ in 0..50 {
            if cache.status(root).is_some() {
                break;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(cache.status(root), None);
    }
}
