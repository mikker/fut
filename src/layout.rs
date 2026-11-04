use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    domain::{PaneId, TerminalId, WorkspaceId},
    resources::Project,
    splits::SplitTree,
};

const VERSION: u8 = 1;
const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PANES: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl RestartCommand {
    pub fn fingerprint(&self, cwd: &Path) -> String {
        let bytes = serde_json::to_vec(&(self, cwd)).expect("restart command serializes");
        format!("{:x}", Sha256::digest(bytes))
    }

    fn validate(&self) -> Result<()> {
        validate_path(&self.program)?;
        ensure!(self.args.len() <= 256, "too many restart arguments");
        for arg in &self.args {
            ensure!(
                arg.len() <= 64 * 1024 && !arg.contains('\0'),
                "invalid restart argument"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecipePosition {
    pub digest: String,
    pub indexes: [usize; 3],
    pub ids: [Option<String>; 3],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedPane {
    pub id: PaneId,
    pub terminal_id: TerminalId,
    pub name: String,
    pub cwd: PathBuf,
    pub command: Option<RestartCommand>,
    pub warning: Option<String>,
    #[serde(default)]
    pub return_to_shell: bool,
    #[serde(default)]
    pub recipe_position: Option<RecipePosition>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedTab {
    pub name: String,
    pub layout: SplitTree,
    pub panes: Vec<SavedPane>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedWorkspace {
    pub id: WorkspaceId,
    pub parent: Option<WorkspaceId>,
    pub name: String,
    pub root: PathBuf,
    pub tabs: Vec<SavedTab>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedProject {
    pub project: Project,
    pub source_root: PathBuf,
    pub name: String,
    pub focus: Option<PaneId>,
    pub workspaces: Vec<SavedWorkspace>,
}

impl SavedProject {
    pub(crate) fn panes(&self) -> impl Iterator<Item = &SavedPane> {
        self.workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.panes)
    }

    pub(crate) fn summary(&self, approved: &BTreeSet<String>) -> LayoutSummary {
        LayoutSummary {
            name: self.name.clone(),
            workspaces: self.workspaces.len(),
            tabs: self.workspaces.iter().map(|w| w.tabs.len()).sum(),
            panes: self.panes().count(),
            pending_commands: self
                .panes()
                .filter(|p| {
                    p.command
                        .as_ref()
                        .is_some_and(|c| !approved.contains(&c.fingerprint(&p.cwd)))
                })
                .count(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Checkpoint {
    version: u8,
    pub enabled: bool,
    pub projects: Vec<SavedProject>,
    pub approved: BTreeSet<String>,
    #[serde(default)]
    pub selected_project: Option<Project>,
}

impl Default for Checkpoint {
    fn default() -> Self {
        Self {
            version: VERSION,
            enabled: false,
            projects: Vec::new(),
            approved: BTreeSet::new(),
            selected_project: None,
        }
    }
}

impl Checkpoint {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "unsupported saved-layout version {}",
            self.version
        );
        ensure!(self.projects.len() <= 128, "too many saved projects");
        ensure!(
            self.approved.len() <= MAX_PANES * 4,
            "too many restart approvals"
        );
        ensure!(
            self.approved
                .iter()
                .all(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())),
            "invalid restart approval"
        );
        let mut count = 0;
        let mut names = HashSet::new();
        let mut identities = Vec::new();
        for project in &self.projects {
            validate_name(&project.name)?;
            validate_path(&project.source_root)?;
            match &project.project.identity {
                crate::resources::ProjectIdentity::GitCommonDir(path)
                | crate::resources::ProjectIdentity::CanonicalDirectory(path) => {
                    validate_path(path)?
                }
            }
            ensure!(names.insert(&project.name), "duplicate saved project name");
            ensure!(
                !identities.contains(&&project.project),
                "duplicate saved project identity"
            );
            identities.push(&project.project);
            ensure!(
                !project.workspaces.is_empty() && project.workspaces.len() <= 128,
                "invalid workspace count"
            );
            let ids: HashSet<_> = project.workspaces.iter().map(|w| w.id).collect();
            ensure!(
                ids.len() == project.workspaces.len(),
                "duplicate saved workspace"
            );
            let mut pane_ids = HashSet::new();
            for workspace in &project.workspaces {
                validate_name(&workspace.name)?;
                validate_path(&workspace.root)?;
                let mut parents = HashSet::from([workspace.id]);
                let mut parent = workspace.parent;
                while let Some(id) = parent {
                    ensure!(parents.insert(id), "cycle in saved workspaces");
                    parent = project
                        .workspaces
                        .iter()
                        .find(|w| w.id == id)
                        .context("missing saved workspace parent")?
                        .parent;
                }
                ensure!(
                    !workspace.tabs.is_empty() && workspace.tabs.len() <= 128,
                    "invalid saved tab count"
                );
                for tab in &workspace.tabs {
                    validate_name(&tab.name)?;
                    ensure!(tab.layout.validate(), "invalid saved split tree");
                    let leaves: HashSet<_> = tab.layout.leaf_ids().into_iter().collect();
                    ensure!(
                        leaves.len() == tab.panes.len() && tab.panes.len() <= 256,
                        "invalid saved pane count"
                    );
                    for pane in &tab.panes {
                        count += 1;
                        ensure!(count <= MAX_PANES, "too many saved panes");
                        ensure!(
                            leaves.contains(&pane.id) && pane_ids.insert(pane.id),
                            "duplicate or missing saved pane"
                        );
                        validate_name(&pane.name)?;
                        validate_path(&pane.cwd)?;
                        if let Some(position) = &pane.recipe_position {
                            ensure!(
                                position.digest.len() == 64
                                    && position.digest.bytes().all(|b| b.is_ascii_hexdigit()),
                                "invalid recipe provenance"
                            );
                            for id in position.ids.iter().flatten() {
                                ensure!(
                                    !id.is_empty()
                                        && id.len() <= 64
                                        && id.bytes().all(|b| b.is_ascii_alphanumeric()
                                            || b == b'-'
                                            || b == b'_'),
                                    "invalid recipe pane identity"
                                );
                            }
                        }
                        if let Some(command) = &pane.command {
                            command.validate()?;
                        }
                    }
                }
            }
            ensure!(
                project.focus.is_none_or(|id| pane_ids.contains(&id)),
                "saved focus is missing"
            );
        }
        Ok(())
    }

    pub(crate) fn summaries(&self) -> Vec<LayoutSummary> {
        self.projects
            .iter()
            .map(|p| p.summary(&self.approved))
            .collect()
    }

    pub(crate) fn review(&self) -> Vec<CommandReview> {
        self.projects
            .iter()
            .flat_map(|project| {
                project.panes().filter_map(|pane| {
                    let command = pane.command.as_ref()?;
                    let fingerprint = command.fingerprint(&pane.cwd);
                    Some(CommandReview {
                        project: project.name.clone(),
                        command: command.clone(),
                        cwd: pane.cwd.clone(),
                        approved: self.approved.contains(&fingerprint),
                        fingerprint,
                    })
                })
            })
            .collect()
    }

    pub(crate) fn approve(&mut self, fingerprints: &[String]) -> Result<()> {
        let available: BTreeSet<_> = self.review().into_iter().map(|c| c.fingerprint).collect();
        ensure!(
            fingerprints.iter().all(|s| available.contains(s)),
            "saved commands changed; review the checkpoint again"
        );
        self.approved.extend(fingerprints.iter().cloned());
        Ok(())
    }

    pub(crate) fn select(&self, project: Option<&str>) -> Result<Vec<SavedProject>> {
        match project {
            None => Ok(self.projects.clone()),
            Some(name) => Ok(vec![
                self.projects
                    .iter()
                    .find(|p| p.name == name)
                    .with_context(|| format!("no saved layout for project {name:?}"))?
                    .clone(),
            ]),
        }
    }
}

fn validate_path(path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    ensure!(
        path.is_absolute() && bytes.len() <= 4096 && !bytes.contains(&0),
        "invalid absolute saved path"
    );
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        name.len() <= 512 && !name.chars().any(char::is_control),
        "invalid saved resource name"
    );
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct Store {
    pub path: PathBuf,
}

impl Store {
    pub(crate) fn for_socket(socket: &Path) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        let socket = if socket.is_absolute() {
            socket.to_owned()
        } else {
            std::env::current_dir()?.join(socket)
        };
        let key = format!("{:x}", Sha256::digest(socket.as_os_str().as_bytes()));
        Ok(Self {
            path: crate::state_file::path(&format!("layouts-{key}.json"))?,
        })
    }

    pub(crate) fn read(&self) -> Result<Checkpoint> {
        let Some(contents) = crate::state_file::read(&self.path, MAX_BYTES)? else {
            return Ok(Checkpoint::default());
        };
        let checkpoint: Checkpoint =
            serde_json::from_str(&contents).context("invalid saved layout")?;
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    pub(crate) fn write(&self, checkpoint: &Checkpoint) -> Result<()> {
        checkpoint.validate()?;
        crate::state_file::write(
            &self.path,
            &serde_json::to_string_pretty(checkpoint)?,
            MAX_BYTES,
        )
    }

    pub(crate) fn update<T>(&self, change: impl FnOnce(&mut Checkpoint) -> Result<T>) -> Result<T> {
        let _lock = crate::state_file::Lock::acquire(&self.path)?;
        let mut checkpoint = self.read()?;
        let result = change(&mut checkpoint)?;
        self.write(&checkpoint)?;
        Ok(result)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutSummary {
    pub name: String,
    pub workspaces: usize,
    pub tabs: usize,
    pub panes: usize,
    pub pending_commands: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandReview {
    pub project: String,
    pub command: RestartCommand,
    pub cwd: PathBuf,
    pub fingerprint: String,
    pub approved: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum LayoutAction {
    Save,
    Enable,
    Disable,
    Review,
    Approve { fingerprints: Vec<String> },
    Export { project: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutReply {
    pub enabled: bool,
    pub path: PathBuf,
    pub projects: Vec<LayoutSummary>,
    pub commands: Vec<CommandReview>,
    pub export: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreReport {
    pub restored: Vec<String>,
    pub already_live: Vec<String>,
    pub warnings: Vec<String>,
    pub selected: Option<TerminalId>,
}

impl RestoreReport {
    pub fn message(&self) -> String {
        let mut text = format!(
            "restored {} project(s); {} already live",
            self.restored.len(),
            self.already_live.len()
        );
        for warning in &self.warnings {
            text.push('\n');
            text.push_str(warning);
        }
        text
    }
}

pub(crate) fn export_recipe(project: &SavedProject) -> Result<String> {
    let mut output = toml::Table::new();
    output.insert(
        "focus".into(),
        toml::Value::String(format!(
            "w0.t0.p{}",
            project.workspaces[0].tabs[0].panes[0].id
        )),
    );
    let mut workspaces = Vec::new();
    for (wi, workspace) in project.workspaces.iter().enumerate() {
        let mut w = toml::Table::new();
        w.insert("id".into(), format!("w{wi}").into());
        w.insert(
            "cwd".into(),
            workspace
                .root
                .to_str()
                .context("non-UTF-8 workspace root")?
                .into(),
        );
        if !workspace.name.is_empty() {
            w.insert("title".into(), workspace.name.clone().into());
        }
        if let Some(parent) = workspace.parent {
            let pi = project
                .workspaces
                .iter()
                .position(|w| w.id == parent)
                .context("missing workspace parent")?;
            w.insert("parent".into(), format!("w{pi}").into());
        }
        let mut tabs = Vec::new();
        for (ti, tab) in workspace.tabs.iter().enumerate() {
            let mut t = toml::Table::new();
            t.insert("id".into(), format!("t{ti}").into());
            if !tab.name.is_empty() {
                t.insert("title".into(), tab.name.clone().into());
            }
            let first = *tab.layout.leaf_ids().first().context("empty saved tab")?;
            let mut sequence = vec![(first, None)];
            export_splits(&tab.layout, &mut sequence);
            let mut panes = Vec::new();
            for (id, split) in sequence {
                let pane = tab
                    .panes
                    .iter()
                    .find(|p| p.id == id)
                    .context("missing export pane")?;
                let mut p = toml::Table::new();
                p.insert("id".into(), format!("p{id}").into());
                if !pane.name.is_empty() {
                    p.insert("title".into(), pane.name.clone().into());
                }
                p.insert(
                    "cwd".into(),
                    pane.cwd
                        .to_str()
                        .context("non-UTF-8 pane directory")?
                        .into(),
                );
                if let Some(command) = &pane.command {
                    let args = std::iter::once(
                        command
                            .program
                            .to_str()
                            .context("non-UTF-8 executable")?
                            .to_owned(),
                    )
                    .chain(command.args.clone())
                    .map(toml::Value::String)
                    .collect();
                    p.insert("command".into(), toml::Value::Array(args));
                    p.insert("exec".into(), (!pane.return_to_shell).into());
                }
                if let Some(split) = split {
                    p.insert("split".into(), toml::Value::Table(split));
                }
                if project.focus == Some(id) {
                    output.insert("focus".into(), format!("w{wi}.t{ti}.p{id}").into());
                }
                panes.push(toml::Value::Table(p));
            }
            t.insert("panes".into(), toml::Value::Array(panes));
            tabs.push(toml::Value::Table(t));
        }
        w.insert("tabs".into(), toml::Value::Array(tabs));
        workspaces.push(toml::Value::Table(w));
    }
    output.insert("workspaces".into(), toml::Value::Array(workspaces));
    let text = format!(
        "#:schema https://fut.sh/schemas/project.json\n\n{}",
        toml::to_string_pretty(&output)?
    );
    ensure!(
        text.len() as u64 <= crate::project_definition::MAX_RECIPE_BYTES,
        "saved layout exceeds the recipe size limit"
    );
    let recipe = toml::from_str(&text)?;
    crate::project_definition::validate(&recipe, &[])
        .context("saved layout cannot be represented within recipe limits")?;
    Ok(text)
}

fn export_splits(tree: &SplitTree, sequence: &mut Vec<(PaneId, Option<toml::Table>)>) {
    if let SplitTree::Branch {
        axis,
        ratio,
        first,
        second,
        ..
    } = tree
    {
        let anchor = first.leaf_ids()[0];
        let created = second.leaf_ids()[0];
        let mut split = toml::Table::new();
        split.insert("target".into(), format!("p{anchor}").into());
        split.insert(
            "direction".into(),
            match axis {
                crate::splits::SplitAxis::Horizontal => "right",
                crate::splits::SplitAxis::Vertical => "down",
            }
            .into(),
        );
        split.insert(
            "ratio".into(),
            toml::Value::try_from(ratio).expect("split ratio serializes"),
        );
        sequence.push((created, Some(split)));
        export_splits(first, sequence);
        export_splits(second, sequence);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        resources::ProjectIdentity,
        splits::{SplitDirection, SplitRatio},
    };

    fn checkpoint() -> Checkpoint {
        let pane = SavedPane {
            id: PaneId::new(),
            terminal_id: TerminalId::new(),
            name: String::new(),
            cwd: "/tmp/code".into(),
            command: Some(RestartCommand {
                program: "/bin/sleep".into(),
                args: vec!["30".into()],
            }),
            warning: None,
            return_to_shell: false,
            recipe_position: None,
        };
        Checkpoint {
            projects: vec![SavedProject {
                project: Project {
                    identity: ProjectIdentity::CanonicalDirectory("/tmp/code".into()),
                },
                source_root: "/tmp/code".into(),
                name: "code".into(),
                focus: Some(pane.id),
                workspaces: vec![SavedWorkspace {
                    id: WorkspaceId::new(),
                    parent: None,
                    name: String::new(),
                    root: "/tmp/code".into(),
                    tabs: vec![SavedTab {
                        name: String::new(),
                        layout: SplitTree::leaf(pane.id),
                        panes: vec![pane],
                    }],
                }],
            }],
            ..Checkpoint::default()
        }
    }

    #[test]
    fn approval_is_bound_to_program_arguments_and_directory() {
        let mut saved = checkpoint();
        let review = saved.review();
        saved.approve(&[review[0].fingerprint.clone()]).unwrap();
        assert!(saved.review()[0].approved);
        saved.projects[0].workspaces[0].tabs[0].panes[0].cwd = "/tmp/other".into();
        assert!(!saved.review()[0].approved);
        assert!(saved.approve(&[review[0].fingerprint.clone()]).is_err());
        saved.projects[0].workspaces[0].tabs[0].panes[0].cwd = "/tmp/code".into();
        saved.projects[0].workspaces[0].tabs[0].panes[0]
            .command
            .as_mut()
            .unwrap()
            .args
            .push("changed".into());
        assert!(!saved.review()[0].approved);
    }

    #[test]
    fn private_checkpoint_roundtrips_and_rejects_invalid_versions_or_topology() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store {
            path: temporary.path().join("layout.json"),
        };
        let mut saved = checkpoint();
        store.write(&saved).unwrap();
        assert_eq!(store.read().unwrap(), saved);
        saved.version += 1;
        assert!(store.write(&saved).is_err());
        saved.version = VERSION;
        saved.projects[0].workspaces[0].parent = Some(saved.projects[0].workspaces[0].id);
        assert!(store.write(&saved).is_err());
        assert_eq!(store.read().unwrap(), checkpoint_from_disk(&store));
        std::fs::write(&store.path, "broken").unwrap();
        assert!(store.read().is_err());
    }

    fn checkpoint_from_disk(store: &Store) -> Checkpoint {
        serde_json::from_str(&std::fs::read_to_string(&store.path).unwrap()).unwrap()
    }

    #[test]
    fn export_preserves_nested_split_proportions() {
        let mut saved = checkpoint();
        let tab = &mut saved.projects[0].workspaces[0].tabs[0];
        let mut next = tab.panes[0].clone();
        next.id = PaneId::new();
        next.terminal_id = TerminalId::new();
        tab.layout
            .split(tab.panes[0].id, SplitDirection::Down, next.id);
        if let SplitTree::Branch { ratio, .. } = &mut tab.layout {
            *ratio = SplitRatio::from_cells(2, 3).unwrap();
        }
        tab.panes.push(next);
        let text = export_recipe(&saved.projects[0]).unwrap();
        let recipe: crate::project_definition::WorkspaceRecipe = toml::from_str(&text).unwrap();
        assert_eq!(recipe.workspaces()[0].tabs()[0].panes().len(), 2);
        assert!(text.contains("numerator = 2"));
    }

    #[test]
    fn endpoints_have_independent_checkpoint_files() {
        assert_ne!(
            Store::for_socket(Path::new("/tmp/a.sock")).unwrap().path,
            Store::for_socket(Path::new("/tmp/b.sock")).unwrap().path
        );
    }
}
