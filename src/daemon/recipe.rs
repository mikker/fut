use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::mpsc;

use crate::{
    client::config as global_config,
    domain::{PaneId, SessionId, TabId, TerminalId, TerminalSize, WorkspaceId},
    project::{ProjectResolver, ResolvedLocation},
    project_definition::{LoadedRecipe, ProjectDefinitionError},
    protocol::{OpenDisposition, SelectedTarget},
    resources::{
        CheckoutDestination, InitialPath, Mutation, ResolvedTerminalPath, ResourceTree, TabPath,
        TrustedProjectConfig, WorkspacePath,
    },
    splits::SplitDirection,
    terminal::{SpawnSpec, TerminalHandle, TerminalLifecycle, spawn_terminal},
};

use super::{
    DaemonError, RuntimeEntry, Shared, SharedState, lease::AttachmentLease,
    open_location_without_recipe, resolve_spawn_cwd, selected_target, terminal_env, watch_terminal,
};

#[derive(Clone, Debug)]
pub(super) struct PreparedRecipe {
    workspaces: Vec<PreparedRecipeWorkspace>,
    trusted_project_config: Option<TrustedProjectConfig>,
}

#[derive(Clone, Debug)]
struct PreparedRecipeWorkspace {
    title: String,
    tabs: Vec<PreparedRecipeTab>,
}

#[derive(Clone, Debug)]
struct PreparedRecipeTab {
    title: String,
    panes: Vec<PreparedRecipePane>,
}

#[derive(Clone, Debug)]
struct PreparedRecipePane {
    placement: PreparedPanePlacement,
    focused: bool,
    program: PathBuf,
    argv: Vec<String>,
    cwd: PathBuf,
    environment: HashMap<String, String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreparedPanePlacement {
    Initial,
    Split {
        target: usize,
        direction: SplitDirection,
    },
}

struct PlannedRecipeTerminal {
    path: ResolvedTerminalPath,
    program: PathBuf,
    argv: Vec<String>,
    cwd: PathBuf,
    environment: HashMap<String, String>,
}

struct RecipeCreationPlan {
    resources: ResourceTree,
    mutations: Vec<Mutation>,
    terminals: Vec<PlannedRecipeTerminal>,
    selected: ResolvedTerminalPath,
    disposition: OpenDisposition,
    replacing: Option<TerminalId>,
}

type SpawnedRecipeTerminals = Vec<Arc<TerminalHandle>>;
type RecipeSpawnResult = Result<SpawnedRecipeTerminals, (DaemonError, SpawnedRecipeTerminals)>;

enum RecipeDestination {
    Existing(SelectedTarget),
    Create {
        destination: CheckoutDestination,
        resources: ResourceTree,
        mutations: Vec<Mutation>,
        replacing: Option<TerminalId>,
    },
}

#[derive(Clone, Debug)]
struct ConfiguredProject {
    name: String,
    config: global_config::ProjectConfig,
}

pub(super) async fn prepare_initial(
    catalog: &global_config::ProjectCatalog,
    extensions: &[crate::extensions::Extension],
    resolved: &ResolvedLocation,
    command_override: Option<(PathBuf, Vec<String>)>,
) -> Result<Option<PreparedRecipe>, DaemonError> {
    let configured = configured_project(catalog, None, resolved).await?;
    let Some(loaded) = load_project_recipe(configured.as_ref(), extensions)? else {
        return Ok(None);
    };
    prepare_recipe(loaded, &resolved.workspace_root, command_override)
        .await
        .map(Some)
}

pub(super) async fn create_initial(
    state: &mut SharedState,
    resolved: &ResolvedLocation,
    recipe: &PreparedRecipe,
) -> Result<SpawnedRecipeTerminals, DaemonError> {
    let plan = plan_recipe_session(
        ResourceTree::default(),
        Vec::new(),
        None,
        resolved,
        None,
        recipe,
    )?;
    let terminals = match spawn_recipe_terminals(&plan, &state.child_env) {
        Ok(terminals) => terminals,
        Err((error, terminals)) => {
            close_spawned_terminals(terminals).await;
            return Err(error);
        }
    };
    if let Err(error) = install_recipe_plan(state, plan, &terminals) {
        close_spawned_terminals(terminals).await;
        return Err(error);
    }
    Ok(terminals)
}

pub(super) async fn open_location(
    shared: &Shared,
    exited: &mpsc::UnboundedSender<TerminalId>,
    project: Option<String>,
    name: Option<String>,
    cwd: PathBuf,
    program: Option<PathBuf>,
    argv: Vec<String>,
) -> Result<(SelectedTarget, OpenDisposition), DaemonError> {
    let resolved = ProjectResolver::default().resolve(&cwd).await?;
    let catalog = {
        let state = shared.lock().await;
        if !state.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        state.projects.clone()
    };
    let configured = configured_project(&catalog, project.as_deref(), &resolved).await?;
    let extension_registry = {
        let state = shared.lock().await;
        Arc::clone(&state.extension_registry)
    };

    // Recipes bootstrap a project session from nothing. Once that session
    // exists, new workspaces start with a single ordinary terminal and inherit
    // only the trusted project extension configuration captured by the session.
    let add_workspace_without_recipe = {
        let mut state = shared.lock().await;
        if !state.accepting {
            return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
        }
        match recipe_destination(&mut state, &resolved)? {
            RecipeDestination::Existing(selected) => {
                return Ok((selected, OpenDisposition::Existing));
            }
            RecipeDestination::Create {
                destination: CheckoutDestination::AddWorkspace { .. },
                ..
            } => true,
            RecipeDestination::Create {
                destination: CheckoutDestination::CreateSession,
                ..
            } => false,
            RecipeDestination::Create {
                destination: CheckoutDestination::Existing(_),
                ..
            } => unreachable!("creation destination is new"),
        }
    };
    if add_workspace_without_recipe {
        return open_location_without_recipe(shared, exited, name, resolved, program, argv).await;
    }

    let loaded = match load_project_recipe(configured.as_ref(), extension_registry.extensions()) {
        Ok(loaded) => loaded,
        Err(error) => {
            let mut state = shared.lock().await;
            if state.accepting
                && let RecipeDestination::Existing(selected) =
                    recipe_destination(&mut state, &resolved)?
            {
                return Ok((selected, OpenDisposition::Existing));
            }
            return Err(error);
        }
    };
    let Some(loaded) = loaded else {
        return open_location_without_recipe(shared, exited, name, resolved, program, argv).await;
    };
    let command_override = program.clone().map(|program| (program, argv.clone()));
    let recipe = match prepare_recipe(loaded, &resolved.workspace_root, command_override).await {
        Ok(recipe) => recipe,
        Err(error) => {
            let mut state = shared.lock().await;
            if state.accepting
                && let RecipeDestination::Existing(selected) =
                    recipe_destination(&mut state, &resolved)?
            {
                return Ok((selected, OpenDisposition::Existing));
            }
            return Err(error);
        }
    };

    let mut state = shared.lock().await;
    if !state.accepting {
        return Err(DaemonError::new("shutting_down", "daemon is shutting down"));
    }
    let (resources, mutations, replacing) = match recipe_destination(&mut state, &resolved)? {
        RecipeDestination::Existing(selected) => {
            return Ok((selected, OpenDisposition::Existing));
        }
        RecipeDestination::Create {
            destination: CheckoutDestination::CreateSession,
            resources,
            mutations,
            replacing,
        } => (resources, mutations, replacing),
        RecipeDestination::Create {
            destination: CheckoutDestination::AddWorkspace { .. },
            ..
        } => {
            drop(state);
            return open_location_without_recipe(shared, exited, name, resolved, program, argv)
                .await;
        }
        RecipeDestination::Create {
            destination: CheckoutDestination::Existing(_),
            ..
        } => unreachable!("creation destination is new"),
    };
    let plan = plan_recipe_session(resources, mutations, replacing, &resolved, name, &recipe)?;
    let selected_path = plan.selected;
    let disposition = plan.disposition;
    let terminals = match spawn_recipe_terminals(&plan, &state.child_env) {
        Ok(terminals) => terminals,
        Err((error, terminals)) => {
            drop(state);
            close_spawned_terminals(terminals).await;
            return Err(error);
        }
    };
    if let Err(error) = install_recipe_plan(&mut state, plan, &terminals) {
        drop(state);
        close_spawned_terminals(terminals).await;
        return Err(error);
    }
    let selected_terminal = terminals
        .iter()
        .find(|terminal| terminal.id() == selected_path.terminal_id)
        .expect("validated recipe focus has a spawned terminal");
    let selected = selected_target(selected_path, selected_terminal);
    drop(state);
    for terminal in terminals {
        watch_terminal(terminal, Arc::clone(shared), exited.clone());
    }
    Ok((selected, disposition))
}

async fn configured_project(
    catalog: &global_config::ProjectCatalog,
    explicit_name: Option<&str>,
    resolved: &ResolvedLocation,
) -> Result<Option<ConfiguredProject>, DaemonError> {
    let resolver = ProjectResolver::default();
    if let Some(name) = explicit_name {
        let project = catalog.get(name).cloned().ok_or_else(|| {
            let available = catalog
                .iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>()
                .join(", ");
            let message = if available.is_empty() {
                format!("unknown project {name:?}; no projects are configured")
            } else {
                format!("unknown project {name:?}; configured projects: {available}")
            };
            DaemonError::new("unknown_project", message)
        })?;
        let configured = resolver.resolve(project.path()).await.map_err(|error| {
            DaemonError::new(
                "project_resolution",
                format!("resolve configured project {name:?}: {error}"),
            )
        })?;
        if configured.project != resolved.project {
            return Err(DaemonError::new(
                "project_mismatch",
                format!(
                    "{} does not belong to configured project {name:?} at {}",
                    resolved.cwd.display(),
                    project.path().display()
                ),
            ));
        }
        return Ok(Some(ConfiguredProject {
            name: name.to_owned(),
            config: project,
        }));
    }

    let mut matched = None;
    for (name, project) in catalog.iter() {
        let Ok(configured) = resolver.resolve(project.path()).await else {
            // A broken unrelated catalog entry must not prevent opening a
            // normal directory. Explicit selection reports its resolution
            // error above.
            continue;
        };
        if configured.project != resolved.project {
            continue;
        }
        if matched.is_some() {
            return Err(DaemonError::new(
                "ambiguous_project",
                format!(
                    "project identity for {} matches multiple configured projects, including {name:?}",
                    resolved.cwd.display()
                ),
            ));
        }
        matched = Some(ConfiguredProject {
            name: name.to_owned(),
            config: project.clone(),
        });
    }
    Ok(matched)
}

fn load_project_recipe(
    project: Option<&ConfiguredProject>,
    extensions: &[crate::extensions::Extension],
) -> Result<Option<LoadedRecipe>, DaemonError> {
    let Some(project) = project else {
        return Ok(None);
    };
    crate::project_definition::load(Some(&project.name), &project.config, extensions).map_err(
        |error| match error {
            error @ ProjectDefinitionError::UntrustedRecipe { .. } => {
                DaemonError::new("untrusted_recipe", error.to_string())
            }
            error @ ProjectDefinitionError::InherentlyTrusted { .. } => {
                DaemonError::new("invalid_recipe", error.to_string())
            }
            ProjectDefinitionError::Invalid(error) => {
                DaemonError::new("invalid_recipe", format!("{error:#}"))
            }
        },
    )
}

pub(super) async fn reload_project_config(
    shared: &Shared,
    session_id: SessionId,
) -> Result<(u64, bool), DaemonError> {
    let (catalog, extensions, project, workspace_root) = {
        let state = shared.lock().await;
        let (project, workspace_root) = state.resources.session_project_context(session_id)?;
        (
            state.projects.clone(),
            Arc::clone(&state.extension_registry),
            project,
            workspace_root,
        )
    };
    let resolved = ProjectResolver::default().resolve(&workspace_root).await?;
    if resolved.project != project {
        return Err(DaemonError::new(
            "project_mismatch",
            "the live session no longer matches its configured project",
        ));
    }
    let configured = configured_project(&catalog, None, &resolved).await?;
    let config = load_project_recipe(configured.as_ref(), extensions.extensions())?.and_then(
        |LoadedRecipe {
             source,
             digest,
             recipe,
         }| {
            tracing::debug!(path = %source.display(), %digest, "reloaded trusted project recipe");
            (!recipe.extension().is_empty()).then(|| TrustedProjectConfig {
                source,
                extension: recipe.extension().clone(),
            })
        },
    );

    let mut state = shared.lock().await;
    let (revision, changed) = state.resources.reload_project_config(session_id, config)?;
    if changed {
        state.publish_resource_change(revision);
    }
    Ok((revision, changed))
}

async fn prepare_recipe(
    loaded: LoadedRecipe,
    workspace_root: &Path,
    command_override: Option<(PathBuf, Vec<String>)>,
) -> Result<PreparedRecipe, DaemonError> {
    let LoadedRecipe {
        source,
        digest,
        recipe,
    } = loaded;
    tracing::debug!(path = %source.display(), %digest, "loaded trusted project recipe");
    let trusted_project_config = (!recipe.extension().is_empty()).then(|| TrustedProjectConfig {
        source: source.clone(),
        extension: recipe.extension().clone(),
    });
    let focus = recipe.focus().map(|focus| {
        let mut parts = focus.split('.');
        (
            parts.next().expect("validated recipe focus workspace"),
            parts.next().expect("validated recipe focus tab"),
            parts.next().expect("validated recipe focus pane"),
        )
    });
    let mut workspaces = Vec::with_capacity(recipe.workspaces().len());
    for (workspace_index, workspace) in recipe.workspaces().iter().enumerate() {
        let mut tabs = Vec::with_capacity(workspace.tabs().len());
        for (tab_index, tab) in workspace.tabs().iter().enumerate() {
            let tab_cwd =
                resolve_spawn_cwd(workspace_root, tab.cwd().map(Path::to_path_buf)).await?;
            let mut panes = Vec::with_capacity(tab.panes().len());
            let mut pane_indices = HashMap::with_capacity(tab.panes().len());
            for (pane_index, pane) in tab.panes().iter().enumerate() {
                let focused = match focus {
                    Some((workspace_id, tab_id, pane_id)) => {
                        Some(workspace_id) == workspace.id()
                            && Some(tab_id) == tab.id()
                            && Some(pane_id) == pane.id()
                    }
                    None => workspace_index == 0 && tab_index == 0 && pane_index == 0,
                };
                let placement = match pane.split() {
                    None => PreparedPanePlacement::Initial,
                    Some(split) => PreparedPanePlacement::Split {
                        target: pane_indices
                            .get(split.target())
                            .copied()
                            .expect("validated recipe split targets an earlier pane"),
                        direction: split.direction(),
                    },
                };
                if let Some(id) = pane.id() {
                    pane_indices.insert(id, pane_index);
                }
                let cwd = match pane.cwd() {
                    Some(cwd) => resolve_spawn_cwd(workspace_root, Some(cwd.to_owned())).await?,
                    None => tab_cwd.clone(),
                };
                let mut environment = recipe
                    .environment()
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect::<HashMap<_, _>>();
                environment.extend(
                    tab.environment()
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone())),
                );
                environment.extend(
                    pane.environment()
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone())),
                );
                let (mut program, mut argv) = match pane.command() {
                    None => default_shell_command(),
                    Some(command) if pane.exec() => {
                        (PathBuf::from(&command[0]), command[1..].to_vec())
                    }
                    Some(command) => returning_shell_command(command),
                };
                if focused && let Some((override_program, override_argv)) = &command_override {
                    program = override_program.clone();
                    argv = override_argv.clone();
                }
                panes.push(PreparedRecipePane {
                    placement,
                    focused,
                    program,
                    argv,
                    cwd,
                    environment,
                });
            }
            tabs.push(PreparedRecipeTab {
                title: tab.title().unwrap_or_default().to_owned(),
                panes,
            });
        }
        workspaces.push(PreparedRecipeWorkspace {
            title: workspace.title().unwrap_or_default().to_owned(),
            tabs,
        });
    }
    Ok(PreparedRecipe {
        workspaces,
        trusted_project_config,
    })
}

fn default_shell_command() -> (PathBuf, Vec<String>) {
    (
        std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/sh")),
        Vec::new(),
    )
}

fn returning_shell_command(command: &[String]) -> (PathBuf, Vec<String>) {
    let mut argv = vec![
        "-c".into(),
        r#""$@"; exec "${SHELL:-/bin/sh}""#.into(),
        "fut-project-command".into(),
    ];
    argv.extend_from_slice(command);
    (PathBuf::from("/bin/sh"), argv)
}

fn recipe_destination(
    state: &mut SharedState,
    resolved: &ResolvedLocation,
) -> Result<RecipeDestination, DaemonError> {
    let mut destination = state
        .resources
        .checkout_destination(&resolved.project, &resolved.workspace_root)?;
    let mut resources = state.resources.clone();
    let mut mutations = Vec::new();
    let mut replacing = None;
    if let CheckoutDestination::Existing(workspace_id) = destination {
        let path = state
            .resources
            .initial_terminal_for_workspace(workspace_id)?;
        let runtime = state
            .runtimes
            .get(&path.terminal_id)
            .ok_or_else(|| DaemonError::new("resource_error", "terminal runtime missing"))?;
        if matches!(
            runtime.handle.subscribe_lifecycle().borrow().clone(),
            TerminalLifecycle::Running
        ) {
            return Ok(RecipeDestination::Existing(selected_target(
                path,
                &runtime.handle,
            )));
        }

        mutations.push(resources.terminal_exited(path.terminal_id)?);
        destination =
            resources.checkout_destination(&resolved.project, &resolved.workspace_root)?;
        if let CheckoutDestination::Existing(workspace_id) = destination {
            state.finalize_terminal_for_replacement(path.terminal_id)?;
            state.accepting = true;
            let replacement = state
                .resources
                .initial_terminal_for_workspace(workspace_id)?;
            let runtime = state
                .runtimes
                .get(&replacement.terminal_id)
                .ok_or_else(|| DaemonError::new("resource_error", "terminal runtime missing"))?;
            return Ok(RecipeDestination::Existing(selected_target(
                replacement,
                &runtime.handle,
            )));
        }
        replacing = Some(path.terminal_id);
    }
    Ok(RecipeDestination::Create {
        destination,
        resources,
        mutations,
        replacing,
    })
}

fn plan_recipe_session(
    mut resources: ResourceTree,
    mut mutations: Vec<Mutation>,
    replacing: Option<TerminalId>,
    resolved: &ResolvedLocation,
    name: Option<String>,
    recipe: &PreparedRecipe,
) -> Result<RecipeCreationPlan, DaemonError> {
    let session_name =
        name.unwrap_or_else(|| resources.available_session_name(&resolved.suggested_session_name));
    let session_id = SessionId::new();
    let workspace_id = WorkspaceId::new();
    let first_workspace = &recipe.workspaces[0];
    let first_tab = &first_workspace.tabs[0];
    let first_path = ResolvedTerminalPath {
        session_id,
        workspace_id,
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
        terminal_id: TerminalId::new(),
    };
    let first_mutation = resources.create_session(InitialPath {
        session_id,
        session_name,
        project: resolved.project.clone(),
        trusted_project_config: recipe.trusted_project_config.clone(),
        workspace_id,
        workspace_name: first_workspace.title.clone(),
        root: resolved.workspace_root.clone(),
        tab_id: first_path.tab_id,
        tab_name: first_tab.title.clone(),
        pane_id: first_path.pane_id,
        terminal_id: first_path.terminal_id,
    })?;
    mutations.push(first_mutation);

    let mut terminals = Vec::new();
    let mut selected = first_path;
    for (workspace_index, workspace) in recipe.workspaces.iter().enumerate() {
        let workspace_path = if workspace_index == 0 {
            first_path
        } else {
            let path = ResolvedTerminalPath {
                session_id,
                workspace_id: WorkspaceId::new(),
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
                terminal_id: TerminalId::new(),
            };
            mutations.push(resources.add_workspace(
                session_id,
                WorkspacePath {
                    workspace_id: path.workspace_id,
                    workspace_name: workspace.title.clone(),
                    root: resolved.workspace_root.clone(),
                    tab_id: path.tab_id,
                    tab_name: workspace.tabs[0].title.clone(),
                    pane_id: path.pane_id,
                    terminal_id: path.terminal_id,
                },
            )?);
            path
        };
        for (tab_index, tab) in workspace.tabs.iter().enumerate() {
            let tab_path = if tab_index == 0 {
                workspace_path
            } else {
                let path = ResolvedTerminalPath {
                    session_id,
                    workspace_id: workspace_path.workspace_id,
                    tab_id: TabId::new(),
                    pane_id: PaneId::new(),
                    terminal_id: TerminalId::new(),
                };
                mutations.push(resources.add_tab(
                    workspace_path.workspace_id,
                    TabPath {
                        tab_id: path.tab_id,
                        tab_name: tab.title.clone(),
                        pane_id: path.pane_id,
                        terminal_id: path.terminal_id,
                    },
                )?);
                path
            };
            let mut panes = Vec::<ResolvedTerminalPath>::with_capacity(tab.panes.len());
            for pane in &tab.panes {
                let path = match pane.placement {
                    PreparedPanePlacement::Initial => tab_path,
                    PreparedPanePlacement::Split { target, direction } => {
                        let path = ResolvedTerminalPath {
                            session_id,
                            workspace_id: workspace_path.workspace_id,
                            tab_id: tab_path.tab_id,
                            pane_id: PaneId::new(),
                            terminal_id: TerminalId::new(),
                        };
                        mutations.push(resources.split_pane(
                            panes[target].pane_id,
                            direction,
                            path.pane_id,
                            path.terminal_id,
                        )?);
                        path
                    }
                };
                if pane.focused {
                    selected = path;
                }
                panes.push(path);
                terminals.push(PlannedRecipeTerminal {
                    path,
                    program: pane.program.clone(),
                    argv: pane.argv.clone(),
                    cwd: pane.cwd.clone(),
                    environment: pane.environment.clone(),
                });
            }
        }
    }
    resources.focus_pane(selected.pane_id)?;
    resources.validate()?;
    Ok(RecipeCreationPlan {
        resources,
        mutations,
        terminals,
        selected,
        disposition: OpenDisposition::SessionCreated,
        replacing,
    })
}

fn spawn_recipe_terminals(
    plan: &RecipeCreationPlan,
    child_env: &HashMap<std::ffi::OsString, std::ffi::OsString>,
) -> RecipeSpawnResult {
    let mut terminals = Vec::with_capacity(plan.terminals.len());
    for terminal in &plan.terminals {
        let mut env = child_env.clone();
        env.extend(
            terminal
                .environment
                .iter()
                .map(|(key, value)| (key.into(), value.into())),
        );
        let spec = SpawnSpec {
            id: terminal.path.terminal_id,
            program: terminal.program.clone(),
            argv: terminal.argv.clone(),
            cwd: terminal.cwd.clone(),
            env: terminal_env(&env, terminal.path),
            size: TerminalSize {
                columns: 80,
                rows: 24,
            },
        };
        match spawn_terminal(spec) {
            Ok(terminal) => terminals.push(Arc::new(terminal)),
            Err(error) => {
                return Err((
                    DaemonError::new("spawn_failed", error.to_string()),
                    terminals,
                ));
            }
        }
    }
    Ok(terminals)
}

async fn close_spawned_terminals(terminals: SpawnedRecipeTerminals) {
    for terminal in terminals {
        let _ = terminal.close().await;
    }
}

fn install_recipe_plan(
    state: &mut SharedState,
    plan: RecipeCreationPlan,
    terminals: &[Arc<TerminalHandle>],
) -> Result<(), DaemonError> {
    if terminals.len() != plan.terminals.len()
        || terminals
            .iter()
            .zip(&plan.terminals)
            .any(|(terminal, planned)| terminal.id() != planned.path.terminal_id)
    {
        return Err(DaemonError::new(
            "resource_error",
            "spawned recipe terminals do not match the validated topology",
        ));
    }
    if terminals
        .iter()
        .any(|terminal| state.runtimes.contains_key(&terminal.id()))
    {
        return Err(DaemonError::new(
            "resource_error",
            "recipe terminal identifier already exists",
        ));
    }
    let replacement_exit = if let Some(terminal_id) = plan.replacing {
        let runtime = state.runtimes.get(&terminal_id).ok_or_else(|| {
            DaemonError::new("resource_error", "replacement terminal runtime missing")
        })?;
        match runtime.handle.subscribe_lifecycle().borrow().clone() {
            TerminalLifecycle::Running => {
                return Err(DaemonError::new(
                    "resource_race",
                    "replacement terminal became live before recipe commit",
                ));
            }
            TerminalLifecycle::Exited { exit_code } => Some((terminal_id, exit_code)),
        }
    } else {
        None
    };

    state.resources = plan.resources;
    if let Some((terminal_id, exit_code)) = replacement_exit {
        state.runtimes.remove(&terminal_id);
        state.alerts.remove_terminal(terminal_id);
        state.expected_finalizations.insert(terminal_id);
        state.exited_terminals.push_back((terminal_id, exit_code));
        if state.exited_terminals.len() > 256 {
            state.exited_terminals.pop_front();
        }
    }
    for terminal in terminals {
        state.alerts.register_terminal(terminal.id());
        state.expected_finalizations.remove(&terminal.id());
        state
            .exited_terminals
            .retain(|(terminal_id, _)| *terminal_id != terminal.id());
        state.runtimes.insert(
            terminal.id(),
            RuntimeEntry {
                handle: Arc::clone(terminal),
                lease: AttachmentLease::default(),
            },
        );
    }
    state.accepting = true;
    for mutation in &plan.mutations {
        state.enqueue_committed_mutation(mutation);
    }
    state.publish_resource_change(state.resources.revision());
    state.publish_alert_change();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, time::Duration};

    use super::*;

    #[tokio::test]
    async fn planning_uses_prepared_placements_and_builds_the_focused_topology() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir(temporary.path().join("tab-cwd")).unwrap();
        fs::create_dir(temporary.path().join("pane-cwd")).unwrap();
        let source = temporary.path().join("recipe.toml");
        fs::write(
            &source,
            r#"focus = "main.code.agent"
environment = { LEVEL = "workspace" }
[[workspaces]]
id = "main"

[[workspaces.tabs]]
id = "code"
cwd = "tab-cwd"
environment = { TAB_VALUE = "code" }
panes = [
  { id = "editor", command = ["editor", "."] },
  { id = "agent", cwd = "pane-cwd", environment = { LEVEL = "pane" }, command = ["agent", "--fast"], exec = true, split = { target = "editor", direction = "down" } },
]
[[workspaces.tabs]]
panes = [{}]

[[workspaces]]
title = "tools"

[[workspaces.tabs]]
title = "logs"
panes = [{}]
[extension.run]
command = ["mise", "run", "dev"]
auto_start = true
"#,
        )
        .unwrap();
        let project = global_config::ProjectConfig {
            path: temporary.path().to_owned(),
            recipe: Some(source),
        };
        let extension_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions/run");
        let extensions = crate::extensions::load(&[extension_root]).unwrap();
        let loaded = crate::project_definition::load(Some("test"), &project, &extensions)
            .unwrap()
            .unwrap();
        let recipe = prepare_recipe(loaded, temporary.path(), None)
            .await
            .unwrap();
        assert_eq!(
            recipe.workspaces[0].tabs[0].panes[1].placement,
            PreparedPanePlacement::Split {
                target: 0,
                direction: SplitDirection::Down,
            }
        );
        assert!(recipe.workspaces[0].tabs[0].panes[1].focused);
        assert!(!recipe.workspaces[0].tabs[0].panes[0].focused);

        let resolved = ProjectResolver::default()
            .resolve(temporary.path())
            .await
            .unwrap();
        let plan = plan_recipe_session(
            ResourceTree::default(),
            Vec::new(),
            None,
            &resolved,
            Some("recipe".into()),
            &recipe,
        )
        .unwrap();

        plan.resources.validate().unwrap();
        let snapshot = plan.resources.snapshot();
        assert_eq!(
            snapshot.sessions[0]
                .trusted_project_config
                .as_ref()
                .unwrap()
                .extension["run"]["auto_start"],
            true
        );
        assert_eq!(snapshot.sessions[0].workspaces.len(), 2);
        let workspace = &snapshot.sessions[0].workspaces[0];
        assert_eq!(workspace.tabs.len(), 2);
        assert_eq!(workspace.tabs[1].name, "");
        assert_eq!(snapshot.sessions[0].workspaces[1].name, "tools");
        assert_eq!(snapshot.sessions[0].workspaces[1].tabs[0].name, "logs");
        assert_eq!(workspace.tabs[0].panes.len(), 2);
        assert_eq!(workspace.tabs[0].layout.leaf_ids().len(), 2);
        assert_eq!(workspace.tabs[0].panes[1].id, plan.selected.pane_id);
        assert_eq!(plan.terminals.len(), 4);
        assert_eq!(plan.terminals[0].program, Path::new("/bin/sh"));
        assert_eq!(plan.terminals[0].argv[3..], ["editor", "."]);
        assert_eq!(plan.terminals[1].program, Path::new("agent"));
        assert_eq!(plan.terminals[1].argv, ["--fast"]);
        assert_eq!(
            plan.terminals[0].cwd,
            temporary.path().join("tab-cwd").canonicalize().unwrap()
        );
        assert_eq!(
            plan.terminals[1].cwd,
            temporary.path().join("pane-cwd").canonicalize().unwrap()
        );
        assert_eq!(plan.terminals[1].environment["LEVEL"], "pane");
        assert_eq!(plan.terminals[1].environment["TAB_VALUE"], "code");
    }

    #[tokio::test]
    async fn unnamed_workspace_and_tabs_keep_titles_separate_from_recipe_ids() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("recipe.toml");
        fs::write(
            &source,
            r#"[[workspaces]]

[[workspaces.tabs]]
panes = [{ command = ["suite"] }]

[[workspaces.tabs]]
title = "agent"
panes = [{ command = ["pi"] }]
"#,
        )
        .unwrap();
        let project = global_config::ProjectConfig {
            path: temporary.path().to_owned(),
            recipe: Some(source),
        };
        let loaded = crate::project_definition::load(Some("test"), &project, &[])
            .unwrap()
            .unwrap();
        let recipe = prepare_recipe(loaded, temporary.path(), None)
            .await
            .unwrap();
        let resolved = ProjectResolver::default()
            .resolve(temporary.path())
            .await
            .unwrap();
        let plan = plan_recipe_session(
            ResourceTree::default(),
            Vec::new(),
            None,
            &resolved,
            None,
            &recipe,
        )
        .unwrap();

        assert_eq!(recipe.workspaces[0].title, "");
        let workspace = &plan.resources.snapshot().sessions[0].workspaces[0];
        assert_eq!(
            workspace
                .tabs
                .iter()
                .map(|tab| tab.name.as_str())
                .collect::<Vec<_>>(),
            ["", "agent"]
        );
        assert_eq!(plan.terminals[0].argv[3..], ["suite"]);
        assert_eq!(plan.terminals[1].argv[3..], ["pi"]);
    }

    #[tokio::test]
    async fn spawn_failure_returns_every_started_terminal_for_cleanup() {
        let temporary = tempfile::tempdir().unwrap();
        let path = ResolvedTerminalPath {
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
            terminal_id: TerminalId::new(),
        };
        let plan = RecipeCreationPlan {
            resources: ResourceTree::default(),
            mutations: Vec::new(),
            terminals: vec![
                PlannedRecipeTerminal {
                    path,
                    program: "/bin/sh".into(),
                    argv: vec!["-c".into(), "while :; do sleep 1; done".into()],
                    cwd: temporary.path().to_owned(),
                    environment: HashMap::new(),
                },
                PlannedRecipeTerminal {
                    path: ResolvedTerminalPath {
                        pane_id: PaneId::new(),
                        terminal_id: TerminalId::new(),
                        ..path
                    },
                    program: "/definitely/missing/fut-recipe-command".into(),
                    argv: Vec::new(),
                    cwd: temporary.path().to_owned(),
                    environment: HashMap::new(),
                },
            ],
            selected: path,
            disposition: OpenDisposition::SessionCreated,
            replacing: None,
        };
        let (error, spawned) = match spawn_recipe_terminals(&plan, &HashMap::new()) {
            Ok(_) => panic!("missing recipe command unexpectedly spawned"),
            Err(failure) => failure,
        };
        assert_eq!(error.code, "spawn_failed");
        assert_eq!(spawned.len(), 1);
        let pid = spawned[0].child_pid() as libc::pid_t;
        // SAFETY: signal zero only checks the numeric process identifier.
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);

        close_spawned_terminals(spawned).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                // SAFETY: signal zero only checks the numeric process identifier.
                if unsafe { libc::kill(pid, 0) } == -1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cleaned-up recipe terminal remained alive");
    }
}
