use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use tokio::{
    sync::{Mutex, mpsc, watch},
    task::JoinHandle,
};

use crate::{
    domain::{PaneId, SessionId, SplitId, TabId, TerminalId, TerminalSize, WorkspaceId},
    layout::{
        Checkpoint, LayoutAction, LayoutReply, RestartCommand, RestoreReport, SavedPane,
        SavedProject, SavedTab, SavedWorkspace, Store,
    },
    resources::{
        InitialPath, Project, ResolvedTerminalPath, ResourceSnapshot, ResourceTree, TabPath,
        WorkspacePath,
    },
    splits::SplitTree,
    terminal::{SpawnSpec, TerminalHandle, spawn_terminal},
};

use super::{
    RuntimeEntry, Shared, lease::AttachmentLease, process_cwd, terminal_env, watch_terminal,
};

pub(super) struct LayoutRuntime {
    store: Store,
    pub enabled: bool,
    gate: Arc<Mutex<()>>,
    previously_live: Vec<Project>,
    pub focus: HashMap<SessionId, TerminalId>,
    pub last_focus: Option<TerminalId>,
    pub fallbacks: HashMap<TerminalId, SavedPane>,
    pub provenance: HashMap<TerminalId, crate::layout::RecipePosition>,
    pub roots: HashMap<SessionId, PathBuf>,
}

impl LayoutRuntime {
    pub fn load(socket: &Path) -> Result<Self> {
        let store = Store::for_socket(socket)?;
        let checkpoint = store.read()?;
        Ok(Self {
            store,
            enabled: checkpoint.enabled,
            gate: Arc::new(Mutex::new(())),
            previously_live: Vec::new(),
            focus: HashMap::new(),
            last_focus: None,
            fallbacks: HashMap::new(),
            provenance: HashMap::new(),
            roots: HashMap::new(),
        })
    }

    pub(super) fn remember_project(&mut self, project: &Project) {
        if !self.previously_live.contains(project) {
            self.previously_live.push(project.clone());
        }
    }

    pub fn has_startup_layout(&self) -> bool {
        self.enabled
            && self
                .store
                .read()
                .is_ok_and(|saved| !saved.projects.is_empty())
    }
}

async fn store_and_gate(shared: &Shared) -> Result<(Store, Arc<Mutex<()>>)> {
    let state = shared.lock().await;
    let layout = state
        .layout
        .as_ref()
        .context("layout persistence is unavailable; check daemon startup diagnostics")?;
    Ok((layout.store.clone(), Arc::clone(&layout.gate)))
}

pub(super) fn watch(shared: Shared, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => return,
                _ = interval.tick() => {
                    if shared.lock().await.layout.as_ref().is_some_and(|l| l.enabled)
                        && let Err(error) = save(&shared).await {
                        tracing::warn!(%error, "automatic layout checkpoint failed; previous checkpoint retained");
                    }
                }
            }
        }
    })
}

pub(super) async fn list(shared: &Shared) -> Result<LayoutReply> {
    let (store, gate) = store_and_gate(shared).await?;
    let _guard = gate.lock().await;
    let saved = store.read()?;
    Ok(reply(&store, &saved))
}

fn reply(store: &Store, saved: &Checkpoint) -> LayoutReply {
    LayoutReply {
        enabled: saved.enabled,
        path: store.path.clone(),
        projects: saved.summaries(),
        commands: Vec::new(),
        export: None,
    }
}

pub(super) async fn save(shared: &Shared) -> Result<LayoutReply> {
    let (store, gate) = store_and_gate(shared).await?;
    let _guard = gate.lock().await;
    capture(shared, &store).await?;
    Ok(reply(&store, &store.read()?))
}

pub(super) async fn action(shared: &Shared, action: LayoutAction) -> Result<LayoutReply> {
    let (store, gate) = store_and_gate(shared).await?;
    let _guard = gate.lock().await;
    if matches!(action, LayoutAction::Save | LayoutAction::Enable) {
        capture(shared, &store).await?;
    }
    let mut result = store.update(|saved| {
        match &action {
            LayoutAction::Enable => saved.enabled = true,
            LayoutAction::Disable => saved.enabled = false,
            LayoutAction::Approve { fingerprints } => saved.approve(fingerprints)?,
            _ => {}
        }
        let mut result = reply(&store, saved);
        if matches!(action, LayoutAction::Review) {
            result.commands = saved.review();
        }
        if let LayoutAction::Export { project } = &action {
            result.export = Some(crate::layout::export_recipe(
                &saved.select(Some(project))?[0],
            )?);
        }
        Ok(result)
    })?;
    if matches!(action, LayoutAction::Enable | LayoutAction::Disable) {
        shared.lock().await.layout.as_mut().unwrap().enabled =
            matches!(action, LayoutAction::Enable);
        result.enabled = matches!(action, LayoutAction::Enable);
    }
    Ok(result)
}

async fn observe(
    handle: &TerminalHandle,
    groups: Option<&super::process::ProcessGroups>,
) -> PaneObservation {
    let result = tokio::time::timeout(Duration::from_millis(500), async {
        let group = handle.foreground_process_id().await.ok()?;
        let pid = groups?.program_pid(group)?;
        if pid != handle.child_pid() && !super::process::is_descendant(pid, handle.child_pid()) {
            return None;
        }
        let first = super::process::restart_process(pid);
        if first.is_none() && !super::process::shell_process(pid) {
            return None;
        }
        let cwd = process_cwd(pid).await?;
        let second = super::process::restart_process(pid);
        if first != second || handle.foreground_process_id().await.ok()? != group {
            return None;
        }
        Some((
            first.map(|p| RestartCommand {
                program: p.program,
                args: p.args,
            }),
            cwd,
            pid != handle.child_pid(),
        ))
    })
    .await;
    match result {
        Ok(Some((command, cwd, return_to_shell))) => PaneObservation {
            command,
            cwd,
            warning: None,
            return_to_shell,
        },
        _ => {
            let cwd = tokio::time::timeout(Duration::from_millis(500), async {
                let group = handle.foreground_process_id().await.ok()?;
                if !super::process::is_descendant(group, handle.child_pid()) {
                    return None;
                }
                let cwd = process_cwd(group).await?;
                (handle.foreground_process_id().await.ok()? == group).then_some(cwd)
            })
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| handle.spawn_cwd().to_owned());
            PaneObservation {
                command: None,
                cwd,
                warning: Some(
                    "foreground program could not be identified reliably; restore as a shell"
                        .into(),
                ),
                return_to_shell: false,
            }
        }
    }
}

async fn capture(shared: &Shared, store: &Store) -> Result<()> {
    let (snapshot, handles, fallbacks, provenance, roots, focus, last_focus, previously_live) = {
        let state = shared.lock().await;
        let layout = state
            .layout
            .as_ref()
            .context("layout persistence unavailable")?;
        (
            state.resources.persistence_snapshot(),
            state
                .runtimes
                .iter()
                .map(|(id, runtime)| (*id, Arc::clone(&runtime.handle)))
                .collect::<HashMap<_, _>>(),
            layout.fallbacks.clone(),
            layout.provenance.clone(),
            layout.roots.clone(),
            layout.focus.clone(),
            layout.last_focus,
            layout.previously_live.clone(),
        )
    };
    let groups = super::process::ProcessGroups::observe().await.map(Arc::new);
    let observations: HashMap<_, _> = futures_util::stream::iter(handles.clone())
        .map(|(id, handle)| {
            let groups = groups.clone();
            async move { (id, observe(&handle, groups.as_deref()).await) }
        })
        .buffer_unordered(8)
        .collect()
        .await;
    let projects = captured_projects(
        &snapshot,
        &observations,
        &fallbacks,
        &provenance,
        &roots,
        &focus,
    );
    let selected_project = projects
        .iter()
        .find(|p| p.panes().any(|pane| Some(pane.terminal_id) == last_focus))
        .map(|p| p.project.clone());
    let live: Vec<_> = projects.iter().map(|p| p.project.clone()).collect();
    let mut state = shared.lock().await;
    ensure!(
        state.resources.persistence_snapshot().sessions == snapshot.sessions
            && handles.iter().all(|(id, handle)| state
                .runtimes
                .get(id)
                .is_some_and(|runtime| Arc::ptr_eq(handle, &runtime.handle))),
        "layout changed while capturing; retry Save"
    );
    store.update(|saved| {
        let dormant = saved
            .projects
            .iter()
            .filter(|p| !live.contains(&p.project) && !previously_live.contains(&p.project))
            .cloned()
            .collect::<Vec<_>>();
        saved.projects = projects;
        saved.projects.extend(dormant);
        saved.selected_project = selected_project;
        Ok(())
    })?;
    let runtime_ids: HashSet<_> = state.runtimes.keys().copied().collect();
    let layout = state.layout.as_mut().unwrap();
    layout.previously_live = live;
    layout.fallbacks.retain(|id, _| {
        runtime_ids.contains(id)
            && observations
                .get(id)
                .is_none_or(|observation| observation.command.is_none())
    });
    layout.provenance.retain(|id, _| {
        runtime_ids.contains(id)
            && !(fallbacks.contains_key(id)
                && observations
                    .get(id)
                    .is_some_and(|observation| observation.command.is_some()))
    });
    let session_ids: HashSet<_> = snapshot.sessions.iter().map(|s| s.id).collect();
    layout.roots.retain(|id, _| session_ids.contains(id));
    layout.focus.retain(|id, _| session_ids.contains(id));
    Ok(())
}

#[derive(Clone)]
struct PaneObservation {
    command: Option<RestartCommand>,
    cwd: PathBuf,
    warning: Option<String>,
    return_to_shell: bool,
}

fn captured_projects(
    snapshot: &ResourceSnapshot,
    observations: &HashMap<TerminalId, PaneObservation>,
    fallbacks: &HashMap<TerminalId, SavedPane>,
    provenance: &HashMap<TerminalId, crate::layout::RecipePosition>,
    roots: &HashMap<SessionId, PathBuf>,
    focus: &HashMap<crate::domain::SessionId, TerminalId>,
) -> Vec<SavedProject> {
    let mut projects = Vec::new();
    for session in snapshot.sessions.iter().filter(|s| !s.closing) {
        let mut workspaces = Vec::new();
        for workspace in session.workspaces.iter().filter(|w| !w.closing) {
            let mut tabs = Vec::new();
            for tab in workspace.tabs.iter().filter(|t| !t.closing) {
                let mut panes = Vec::new();
                for pane in tab.panes.iter().filter(|p| !p.closing) {
                    let Some(PaneObservation {
                        command,
                        cwd,
                        warning,
                        return_to_shell,
                    }) = observations.get(&pane.terminal_id)
                    else {
                        continue;
                    };
                    let mut saved = SavedPane {
                        id: pane.id,
                        terminal_id: pane.terminal_id,
                        name: pane.name.clone().unwrap_or_default(),
                        cwd: cwd.clone(),
                        command: command.clone(),
                        warning: warning.clone(),
                        return_to_shell: *return_to_shell,
                        recipe_position: provenance.get(&pane.terminal_id).cloned(),
                    };
                    if command.is_some() && fallbacks.contains_key(&pane.terminal_id) {
                        saved.recipe_position = None;
                    }
                    if command.is_none()
                        && let Some(intended) = fallbacks.get(&pane.terminal_id)
                    {
                        saved.cwd = intended.cwd.clone();
                        saved.command = intended.command.clone();
                        saved.warning = intended.warning.clone();
                        saved.return_to_shell = intended.return_to_shell;
                        saved.recipe_position = intended.recipe_position.clone();
                    }
                    panes.push(saved);
                }
                if panes.is_empty() {
                    continue;
                }
                let keep = |id| panes.iter().any(|p| p.id == id);
                let Some(layout) = tab.layout.retained(keep) else {
                    continue;
                };
                tabs.push(SavedTab {
                    name: tab.name.clone(),
                    layout,
                    panes,
                });
            }
            if !tabs.is_empty() {
                workspaces.push(SavedWorkspace {
                    id: workspace.id,
                    parent: workspace.parent_workspace_id,
                    name: workspace.name.clone(),
                    root: workspace.root.clone(),
                    tabs,
                });
            }
        }
        if workspaces.is_empty() {
            continue;
        }
        let remaining: HashSet<_> = workspaces.iter().map(|w| w.id).collect();
        for workspace in &mut workspaces {
            while let Some(parent) = workspace.parent.filter(|p| !remaining.contains(p)) {
                workspace.parent = session
                    .workspaces
                    .iter()
                    .find(|w| w.id == parent)
                    .and_then(|w| w.parent_workspace_id);
            }
        }
        let focused = focus.get(&session.id).copied();
        let focus = workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.panes)
            .find(|p| Some(p.terminal_id) == focused)
            .map(|p| p.id);
        let source_root =
            roots
                .get(&session.id)
                .cloned()
                .unwrap_or_else(|| match &session.project.identity {
                    crate::resources::ProjectIdentity::CanonicalDirectory(path) => path.clone(),
                    _ => workspaces[0].root.clone(),
                });
        projects.push(SavedProject {
            project: session.project.clone(),
            source_root,
            name: session.name.clone(),
            focus,
            workspaces,
        });
    }
    projects
}

pub(crate) async fn compatible_projects(
    snapshot: &ResourceSnapshot,
    daemon_pid: u32,
) -> Vec<SavedProject> {
    let groups = super::process::ProcessGroups::observe().await;
    let mut roots = HashMap::new();
    if let Some(groups) = &groups {
        for pid in groups.children(daemon_pid) {
            if let Some(id) = super::process::terminal_context(pid) {
                roots.insert(id, pid);
            }
        }
    }
    let mut observations = HashMap::new();
    for session in &snapshot.sessions {
        for workspace in &session.workspaces {
            for pane in workspace.tabs.iter().flat_map(|t| &t.panes) {
                let observation = async {
                    let root = *roots.get(&pane.terminal_id)?;
                    let group = super::process::foreground_group(root)?;
                    let pid = groups.as_ref()?.program_pid(group)?;
                    if !super::process::is_descendant(pid, root) { return None; }
                    let first = super::process::restart_process(pid);
                    if first.is_none() && !super::process::shell_process(pid) { return None; }
                    let cwd = super::process_cwd(pid).await?;
                    if first != super::process::restart_process(pid) || super::process::foreground_group(root)? != group { return None; }
                    Some(PaneObservation { command: first.map(|p| RestartCommand { program: p.program, args: p.args }), cwd, warning: None, return_to_shell: pid != root })
                }.await.unwrap_or_else(|| PaneObservation { command: None, cwd: pane.cwd.clone().unwrap_or_else(|| workspace.root.clone()), warning: Some("program could not be identified reliably during older-daemon migration; restore a shell".into()), return_to_shell: false });
                observations.insert(pane.terminal_id, observation);
            }
        }
    }
    let mut focus = HashMap::new();
    if let Some(id) = std::env::var("FUT_TERMINAL_ID")
        .ok()
        .and_then(|s| s.parse::<TerminalId>().ok())
    {
        for session in &snapshot.sessions {
            if session
                .workspaces
                .iter()
                .flat_map(|w| &w.tabs)
                .flat_map(|t| &t.panes)
                .any(|p| p.terminal_id == id)
            {
                focus.insert(session.id, id);
            }
        }
    }
    captured_projects(
        snapshot,
        &observations,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &focus,
    )
}

pub(super) async fn restore(
    shared: &Shared,
    exited: &mpsc::UnboundedSender<TerminalId>,
    project: Option<&str>,
) -> Result<RestoreReport> {
    let (store, gate) = store_and_gate(shared).await?;
    let _guard = gate.lock().await;
    let saved = store.read()?;
    let projects = saved.select(project)?;
    let mut report = RestoreReport {
        restored: Vec::new(),
        already_live: Vec::new(),
        warnings: Vec::new(),
        selected: None,
    };
    for project in projects {
        let snapshot = shared.lock().await.resources.snapshot();
        if let Some(existing) = snapshot
            .sessions
            .iter()
            .find(|s| s.project == project.project)
        {
            report.already_live.push(project.name.clone());
            if report.selected.is_none() {
                report.selected = existing
                    .workspaces
                    .first()
                    .and_then(|w| w.tabs.first())
                    .and_then(|t| t.panes.first())
                    .map(|p| p.terminal_id);
            }
            continue;
        }
        let (config_location, extensions) = {
            let state = shared.lock().await;
            (
                state.config_location.clone(),
                state.extension_registry.extensions().to_vec(),
            )
        };
        let configuration = super::recipe::restore_configuration(
            &config_location,
            &extensions,
            &project.project,
            &project.source_root,
        )
        .await;
        let (trusted, recipe, allow_programs) = match configuration {
            Ok((trusted, recipe)) => (trusted, recipe, true),
            Err(error) => {
                report
                    .warnings
                    .push(format!("{}: {error}; shell-only recovery", project.name));
                (None, None, false)
            }
        };
        let mut state = shared.lock().await;
        ensure!(state.accepting, "daemon is shutting down");
        if state
            .resources
            .snapshot()
            .sessions
            .iter()
            .any(|s| s.project == project.project)
        {
            report.already_live.push(project.name.clone());
            continue;
        }
        let mut planned = state.resources.clone();
        let built = plan_project(&mut planned, &project, trusted);
        let (paths, selected) = match built {
            Ok(plan) => plan,
            Err(error) => {
                report.warnings.push(format!(
                    "{}: {error}; checkpoint retained for retry",
                    project.name
                ));
                continue;
            }
        };
        let mut terminals = Vec::new();
        let mut fallbacks = Vec::new();
        let mut failed = None;
        for (pane, path) in &paths {
            let cwd_exists = pane.cwd.is_dir();
            let cwd = if cwd_exists {
                pane.cwd.clone()
            } else {
                fallback_directory(&state.child_env)
            };
            let approved = pane
                .command
                .as_ref()
                .is_none_or(|c| saved.approved.contains(&c.fingerprint(&pane.cwd)));
            let executable_available = pane
                .command
                .as_ref()
                .is_none_or(|command| executable_available(&command.program));
            let mut run_program = cwd_exists && approved && allow_programs && executable_available;
            let mut environment = terminal_env(&state.child_env, *path);
            if let Some(recipe) = &recipe {
                environment.extend(
                    recipe
                        .recipe
                        .environment()
                        .iter()
                        .map(|(k, v)| (k.into(), v.into())),
                );
                if let Some(position) = &pane.recipe_position {
                    if let Some((tab, source)) = recipe_pane(recipe, position) {
                        environment
                            .extend(tab.environment().iter().map(|(k, v)| (k.into(), v.into())));
                        environment.extend(
                            source
                                .environment()
                                .iter()
                                .map(|(k, v)| (k.into(), v.into())),
                        );
                    } else {
                        run_program = false;
                        report.warnings.push(format!("{}: saved recipe pane no longer matches current settings; opened a shell", project.name));
                    }
                }
                // Host-owned terminal identity always wins over project settings.
                environment.extend(terminal_env(&HashMap::new(), *path));
            }
            let command = pane.command.as_ref().filter(|_| run_program);
            let (program, argv) = match command {
                Some(command) if pane.return_to_shell => {
                    let mut argv = vec![command.program.to_string_lossy().into_owned()];
                    argv.extend(command.args.clone());
                    super::recipe::returning_shell_command(&argv)
                }
                Some(command) => (command.program.clone(), command.args.clone()),
                None => (state.default_shell.clone(), Vec::new()),
            };
            let spec = SpawnSpec {
                id: path.terminal_id,
                program,
                argv,
                cwd: cwd.clone(),
                env: environment.clone(),
                size: TerminalSize {
                    columns: 80,
                    rows: 24,
                },
                terminal: state.terminal_config.clone(),
            };
            let mut fallback = !run_program || pane.warning.is_some();
            let handle = match spawn_terminal(spec) {
                Ok(handle) => handle,
                Err(error) if command.is_some() => {
                    fallback = true;
                    report.warnings.push(format!(
                        "{} / {}: {error}; opened a shell",
                        project.name,
                        pane.cwd.display()
                    ));
                    match spawn_terminal(SpawnSpec {
                        id: path.terminal_id,
                        program: state.default_shell.clone(),
                        argv: Vec::new(),
                        cwd,
                        env: environment,
                        size: TerminalSize {
                            columns: 80,
                            rows: 24,
                        },
                        terminal: state.terminal_config.clone(),
                    }) {
                        Ok(handle) => handle,
                        Err(error) => {
                            failed = Some(error.to_string());
                            break;
                        }
                    }
                }
                Err(error) => {
                    failed = Some(error.to_string());
                    break;
                }
            };
            if !cwd_exists {
                report.warnings.push(format!(
                    "{}: missing directory {}; opened a fallback shell, saved command retained",
                    project.name,
                    pane.cwd.display()
                ));
            }
            if !executable_available {
                report.warnings.push(format!("{}: executable is missing or not executable; opened a shell, saved command retained", project.name));
            }
            if !approved {
                report.warnings.push(format!(
                    "{}: command at {} needs `fut layout review` and host approval; opened a shell",
                    project.name,
                    pane.cwd.display()
                ));
            }
            if let Some(warning) = &pane.warning {
                report.warnings.push(format!("{}: {warning}", project.name));
            }
            if fallback {
                fallbacks.push((path.terminal_id, (*pane).clone()));
            }
            terminals.push(Arc::new(handle));
        }
        if let Some(error) = failed {
            drop(state);
            for handle in terminals {
                let _ = handle.close().await;
            }
            report.warnings.push(format!(
                "{}: {error}; checkpoint retained for retry",
                project.name
            ));
            continue;
        }
        state.resources = planned;
        for handle in &terminals {
            state.alerts.register_terminal(handle.id());
            state.runtimes.insert(
                handle.id(),
                RuntimeEntry {
                    handle: Arc::clone(handle),
                    lease: AttachmentLease::default(),
                },
            );
        }
        let layout = state.layout.as_mut().unwrap();
        layout.fallbacks.extend(fallbacks);
        layout
            .roots
            .insert(selected.session_id, project.source_root.clone());
        for (pane, path) in &paths {
            if let Some(position) = &pane.recipe_position {
                layout.provenance.insert(path.terminal_id, position.clone());
            }
        }
        layout
            .focus
            .insert(selected.session_id, selected.terminal_id);
        layout.remember_project(&project.project);
        if report.selected.is_none() || saved.selected_project.as_ref() == Some(&project.project) {
            report.selected = Some(selected.terminal_id);
        }
        let revision = state.resources.revision();
        // Restoration is not creation by a user: do not replay recipe bootstrap or hooks.
        state.resource_changes.send_replace(revision);
        drop(state);
        for terminal in terminals {
            watch_terminal(terminal, Arc::clone(shared), exited.clone());
        }
        report.restored.push(project.name);
    }
    if let Some(layout) = &mut shared.lock().await.layout {
        layout.last_focus = report.selected.or(layout.last_focus);
    }
    Ok(report)
}

fn recipe_pane<'a>(
    loaded: &'a crate::project_definition::LoadedRecipe,
    position: &crate::layout::RecipePosition,
) -> Option<(
    &'a crate::project_definition::RecipeTab,
    &'a crate::project_definition::RecipePane,
)> {
    let (tab, pane) = if loaded.digest == position.digest {
        let [wi, ti, pi] = position.indexes;
        let tab = loaded.recipe.workspaces().get(wi)?.tabs().get(ti)?;
        (tab, tab.panes().get(pi)?)
    } else {
        let [Some(workspace), Some(tab), Some(pane)] = &position.ids else {
            return None;
        };
        let workspace = loaded
            .recipe
            .workspaces()
            .iter()
            .find(|w| w.id() == Some(workspace.as_str()))?;
        let tab = workspace
            .tabs()
            .iter()
            .find(|t| t.id() == Some(tab.as_str()))?;
        (
            tab,
            tab.panes().iter().find(|p| p.id() == Some(pane.as_str()))?,
        )
    };
    Some((tab, pane))
}

fn fallback_directory(environment: &HashMap<std::ffi::OsString, std::ffi::OsString>) -> PathBuf {
    environment
        .get(std::ffi::OsStr::new("HOME"))
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

fn plan_project<'a>(
    resources: &mut ResourceTree,
    project: &'a SavedProject,
    trusted: Option<crate::resources::TrustedProjectConfig>,
) -> Result<(
    Vec<(&'a SavedPane, ResolvedTerminalPath)>,
    ResolvedTerminalPath,
)> {
    let sid = SessionId::new();
    let workspace_ids: HashMap<_, _> = project
        .workspaces
        .iter()
        .map(|w| (w.id, WorkspaceId::new()))
        .collect();
    let mut paths = Vec::new();
    let mut focused = None;
    for (wi, workspace) in project.workspaces.iter().enumerate() {
        let wid = workspace_ids[&workspace.id];
        for (ti, tab) in workspace.tabs.iter().enumerate() {
            let tid = TabId::new();
            let mut panes = HashMap::new();
            for (pi, pane) in tab.panes.iter().enumerate() {
                let pid = PaneId::new();
                let terminal_id = TerminalId::new();
                let path = ResolvedTerminalPath {
                    session_id: sid,
                    workspace_id: wid,
                    tab_id: tid,
                    pane_id: pid,
                    terminal_id,
                };
                if pi == 0 {
                    if wi == 0 && ti == 0 {
                        resources.create_session(InitialPath {
                            session_id: sid,
                            session_name: project.name.clone(),
                            project: project.project.clone(),
                            trusted_project_config: trusted.clone(),
                            workspace_id: wid,
                            workspace_name: workspace.name.clone(),
                            root: workspace.root.clone(),
                            tab_id: tid,
                            tab_name: tab.name.clone(),
                            pane_id: pid,
                            terminal_id,
                        })?;
                    } else if ti == 0 {
                        resources.add_workspace(
                            sid,
                            WorkspacePath {
                                workspace_id: wid,
                                parent_workspace_id: None,
                                workspace_name: workspace.name.clone(),
                                root: workspace.root.clone(),
                                tab_id: tid,
                                tab_name: tab.name.clone(),
                                pane_id: pid,
                                terminal_id,
                            },
                        )?;
                    } else {
                        resources.add_tab(
                            wid,
                            TabPath {
                                tab_id: tid,
                                tab_name: tab.name.clone(),
                                pane_id: pid,
                                terminal_id,
                            },
                        )?;
                    }
                } else {
                    resources.add_pane(tid, pid, terminal_id)?;
                }
                resources.rename_pane(pid, pane.name.clone())?;
                panes.insert(pane.id, pid);
                if focused.is_none() || project.focus == Some(pane.id) {
                    focused = Some(path);
                }
                paths.push((pane, path));
            }
            resources.restore_tab_layout(tid, remap_tree(&tab.layout, &panes))?;
        }
    }
    for workspace in &project.workspaces {
        resources.restore_workspace_parent(
            workspace_ids[&workspace.id],
            workspace.parent.map(|id| workspace_ids[&id]),
        )?;
    }
    resources.validate()?;
    Ok((paths, focused.context("saved project has no pane")?))
}

fn executable_available(path: &Path) -> bool {
    path.is_file()
        && std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0)
}

fn remap_tree(tree: &SplitTree, panes: &HashMap<PaneId, PaneId>) -> SplitTree {
    match tree {
        SplitTree::Leaf { pane_id } => SplitTree::leaf(panes[pane_id]),
        SplitTree::Branch {
            axis,
            ratio,
            first,
            second,
            ..
        } => SplitTree::Branch {
            split_id: SplitId::new(),
            axis: *axis,
            ratio: *ratio,
            first: Box::new(remap_tree(first, panes)),
            second: Box::new(remap_tree(second, panes)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{layout::RecipePosition, project_definition::LoadedRecipe};

    fn loaded(digest: &str, source: &str) -> LoadedRecipe {
        let loaded = LoadedRecipe {
            source: "/tmp/project/.fut/project.toml".into(),
            digest: digest.into(),
            recipe: toml::from_str(source).unwrap(),
        };
        crate::project_definition::validate(&loaded.recipe, &[]).unwrap();
        loaded
    }

    #[test]
    fn changed_recipe_settings_follow_stable_ids_not_old_positions() {
        let original = loaded(
            "original",
            r#"
workspaces = [{ id = "workspace", tabs = [{ id = "tab", panes = [
  { id = "first", environment = { ROLE = "first" } },
  { id = "second", environment = { ROLE = "original" }, split = { target = "first", direction = "right" } },
] }] }]
"#,
        );
        let changed = loaded(
            "changed",
            r#"
workspaces = [{ id = "workspace", tabs = [{ id = "tab", environment = { GROUP = "current" }, panes = [
  { id = "second", environment = { ROLE = "matched" } },
  { id = "first", environment = { ROLE = "wrong" }, split = { target = "second", direction = "right" } },
] }] }]
"#,
        );
        let position = RecipePosition {
            digest: "original".into(),
            indexes: [0, 0, 1],
            ids: [
                Some("workspace".into()),
                Some("tab".into()),
                Some("second".into()),
            ],
        };
        assert_eq!(
            recipe_pane(&original, &position).unwrap().1.environment()["ROLE"],
            "original"
        );
        let (tab, pane) = recipe_pane(&changed, &position).unwrap();
        assert_eq!(tab.environment()["GROUP"], "current");
        assert_eq!(pane.environment()["ROLE"], "matched");
        let unnamed = RecipePosition {
            ids: [None, None, None],
            ..position.clone()
        };
        assert!(recipe_pane(&original, &unnamed).is_some());
        assert!(recipe_pane(&changed, &unnamed).is_none());
        let missing = RecipePosition {
            ids: [
                Some("workspace".into()),
                Some("tab".into()),
                Some("missing".into()),
            ],
            ..position
        };
        assert!(recipe_pane(&changed, &missing).is_none());
    }
}
