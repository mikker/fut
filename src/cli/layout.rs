use super::*;
use crate::layout::{LayoutAction, LayoutReply, Store};
use anyhow::ensure;

#[derive(Subcommand)]
pub(super) enum LayoutCommand {
    /// Save every currently open project on this daemon host.
    Save,
    /// List this daemon's latest saved projects, including when it is stopped.
    List,
    /// Restore one project, or the entire host's working setup, then attach.
    Restore {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        project: Option<String>,
        #[arg(long)]
        all: bool,
        /// Restore without opening the interactive client.
        #[arg(short = 'b', long)]
        background: bool,
    },
    /// Enable periodic checkpoints and automatic daemon-start restoration.
    Enable,
    /// Disable automatic checkpoints and restoration, keeping saved layouts.
    Disable,
    /// Show exact executable, arguments, directory, and approval fingerprint.
    Review,
    /// Approve only the reviewed, unchanged command fingerprints on this host.
    Approve {
        #[arg(required = true, num_args = 1..)]
        fingerprints: Vec<String>,
    },
    /// Export a saved project as a recipe without changing .fut/project.toml.
    Export {
        project: String,
        /// Write a new file; never overwrite an existing file. Default: stdout.
        #[arg(short, long, value_hint = ValueHint::FilePath)]
        output: Option<PathBuf>,
    },
}

pub(super) async fn execute(cli: &Cli, command: &LayoutCommand) -> Result<()> {
    let location = cli.config_location()?;
    let remote = cli
        .remote_machine
        .as_deref()
        .map(|name| crate::machines::Catalog::resolve()?.attachment_target(name))
        .transpose()?;
    let socket = socket_path(cli.socket.as_deref())?;
    let message = match command {
        LayoutCommand::Save => ClientMessage::SaveLayout,
        LayoutCommand::List => ClientMessage::ListLayouts,
        LayoutCommand::Restore {
            project,
            background,
            ..
        } => {
            if !background && !cli.json {
                reject_nested_client(cli)?;
            }
            if remote.is_none() {
                let store = Store::for_socket(&socket)?;
                ensure_saved(&store, project.as_deref())?;
                crate::daemon::autostart::ensure_daemon_for_restore(
                    &socket,
                    &std::env::current_dir()?,
                    &location,
                )
                .await?;
            }
            ClientMessage::RestoreLayout {
                project: project.clone(),
            }
        }
        LayoutCommand::Enable => ClientMessage::Layout {
            action: LayoutAction::Enable,
        },
        LayoutCommand::Disable => ClientMessage::Layout {
            action: LayoutAction::Disable,
        },
        LayoutCommand::Review => ClientMessage::Layout {
            action: LayoutAction::Review,
        },
        LayoutCommand::Approve { fingerprints } => ClientMessage::Layout {
            action: LayoutAction::Approve {
                fingerprints: fingerprints.clone(),
            },
        },
        LayoutCommand::Export { project, .. } => ClientMessage::Layout {
            action: LayoutAction::Export {
                project: project.clone(),
            },
        },
    };
    let response = if let Some(host) = &remote {
        client::layout_request(host, &location, cli.attach_only, message).await?
    } else if !matches!(command, LayoutCommand::Save | LayoutCommand::Restore { .. })
        && !socket_is_available(&socket).await
    {
        offline(&Store::for_socket(&socket)?, command)?
    } else {
        let hello =
            crate::protocol::local::LocalHello::new(ClientMode::Control, env!("CARGO_PKG_VERSION"));
        let (mut connection, capabilities) = connected_control_with_hello(&socket, hello).await?;
        if capabilities.layout_restoration() {
            request(connection, message).await?
        } else {
            match command {
                LayoutCommand::Save => compatible_save(&mut connection, &socket).await?,
                LayoutCommand::Restore { .. } => bail!(
                    "this daemon cannot restore layouts; save with this client before an explicitly approved daemon restart"
                ),
                _ => {
                    if matches!(command, LayoutCommand::Enable | LayoutCommand::Disable) {
                        eprintln!(
                            "this older daemon cannot checkpoint automatically; settings take effect after its next explicit restart"
                        );
                    }
                    offline(&Store::for_socket(&socket)?, command)?
                }
            }
        }
    };
    match response {
        ServerMessage::SavedLayouts { projects } => {
            output(cli.json, "layout.list", &projects, summaries(&projects))
        }
        ServerMessage::LayoutSaved { reply } => render_reply(cli, command, reply),
        ServerMessage::LayoutRestored { report } => {
            output(cli.json, "layout.restore", &report, report.message())?;
            let LayoutCommand::Restore {
                project,
                background,
                ..
            } = command
            else {
                unreachable!()
            };
            if *background || cli.json {
                return Ok(());
            }
            if let Some(host) = remote {
                client::attach_remote_restored(
                    &host,
                    &location,
                    cli.attach_only,
                    project.as_ref().and(report.selected),
                )
                .await
            } else {
                let selector = project
                    .as_ref()
                    .and(report.selected)
                    .map(TargetSelector::Terminal);
                match selector {
                    Some(selector) => client::attach(&socket, Some(selector), &location).await,
                    None => client::attach_navigator(&socket, &location, false).await,
                }
            }
        }
        ServerMessage::EndpointError { error } => Err(error.into()),
        ServerMessage::Error { code, message } => bail!("{code}: {message}"),
        other => unexpected(other),
    }
}

fn ensure_saved(store: &Store, project: Option<&str>) -> Result<()> {
    if store.read()?.select(project)?.is_empty() {
        bail!("no saved layouts; run `fut layout save` first");
    }
    Ok(())
}

async fn socket_is_available(socket: &Path) -> bool {
    UnixStream::connect(socket).await.is_ok()
}

async fn connected_control_with_hello(
    socket: &Path,
    hello: crate::protocol::local::LocalHello,
) -> Result<(
    Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    crate::protocol::local::Capabilities,
)> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to {}", socket.display()))?;
    let mut framed = Framed::new(stream, codec());
    let id = send(&mut framed, ClientMessage::LocalHello(hello.clone())).await?;
    let frame = tokio::time::timeout(Duration::from_secs(2), framed.next())
        .await
        .context("layout handshake timed out")?
        .context("daemon disconnected")??;
    let envelope: Envelope<ServerMessage> = crate::protocol::remote::decode_handshake(&frame)?;
    match correlated_message(envelope, id)? {
        ServerMessage::LocalWelcome(welcome) => { let capabilities = hello.accept(&welcome)?; Ok((framed, capabilities)) }
        ServerMessage::EndpointError { error } => Err(anyhow::Error::from(error).context("layout restore requires an updated daemon; do not restart it before saving your running work")),
        other => unexpected(other),
    }
}

async fn compatible_request(
    connection: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    command: ClientMessage,
) -> Result<ServerMessage> {
    let id = send(connection, command).await?;
    receive(
        connection,
        id,
        Duration::from_secs(15),
        "older-daemon snapshot timed out",
    )
    .await
}

async fn compatible_save(
    connection: &mut Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    socket: &Path,
) -> Result<ServerMessage> {
    let ServerMessage::Resources { snapshot, .. } =
        compatible_request(connection, ClientMessage::ListResources).await?
    else {
        bail!("older daemon did not return resources");
    };
    let ServerMessage::Pong { daemon_pid } =
        compatible_request(connection, ClientMessage::Ping).await?
    else {
        bail!("older daemon did not identify its process");
    };
    let projects = crate::daemon::compatible_projects(&snapshot, daemon_pid).await;
    let ServerMessage::Resources {
        snapshot: after, ..
    } = compatible_request(connection, ClientMessage::ListResources).await?
    else {
        bail!("older daemon did not return resources");
    };
    ensure!(
        snapshot.layout_structure() == after.layout_structure(),
        "layout changed while capturing; retry Save"
    );
    let store = Store::for_socket(socket)?;
    let reply = store.update(|saved| {
        saved.selected_project = projects
            .iter()
            .find(|p| p.focus.is_some())
            .map(|p| p.project.clone());
        saved.projects = projects;
        Ok(LayoutReply {
            enabled: saved.enabled,
            path: store.path.clone(),
            projects: saved.summaries(),
            commands: Vec::new(),
            export: None,
        })
    })?;
    eprintln!(
        "saved through the stable older-daemon adapter without restarting or attaching; displayed names are retained, and unavailable focus defaults to the first pane"
    );
    Ok(ServerMessage::LayoutSaved { reply })
}

fn offline(store: &Store, command: &LayoutCommand) -> Result<ServerMessage> {
    if matches!(command, LayoutCommand::List) {
        return Ok(ServerMessage::SavedLayouts {
            projects: store.read()?.summaries(),
        });
    }
    store.update(|saved| {
        match command {
            LayoutCommand::Enable => saved.enabled = true,
            LayoutCommand::Disable => saved.enabled = false,
            LayoutCommand::Approve { fingerprints } => saved.approve(fingerprints)?,
            _ => {}
        }
        Ok(ServerMessage::LayoutSaved {
            reply: LayoutReply {
                enabled: saved.enabled,
                path: store.path.clone(),
                projects: saved.summaries(),
                commands: if matches!(command, LayoutCommand::Review) {
                    saved.review()
                } else {
                    Vec::new()
                },
                export: match command {
                    LayoutCommand::Export { project, .. } => Some(crate::layout::export_recipe(
                        &saved.select(Some(project))?[0],
                    )?),
                    _ => None,
                },
            },
        })
    })
}

fn render_reply(cli: &Cli, command: &LayoutCommand, reply: LayoutReply) -> Result<()> {
    if let Some(export) = &reply.export {
        let LayoutCommand::Export {
            output: destination,
            ..
        } = command
        else {
            unreachable!()
        };
        if let Some(path) = destination {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .with_context(|| format!("create new recipe {}", path.display()))?;
            file.write_all(export.as_bytes())?;
            file.sync_all()?;
            return output(
                cli.json,
                "layout.export",
                &reply,
                format!("exported {}", path.display()),
            );
        }
        if !cli.json {
            print!("{export}");
            return Ok(());
        }
    }
    let text = match command {
        LayoutCommand::Enable => format!(
            "automatic checkpoints and startup restoration enabled\n{}",
            reply.path.display()
        ),
        LayoutCommand::Disable => {
            "automatic checkpoints and startup restoration disabled; saved layouts kept".into()
        }
        LayoutCommand::Review => reply
            .commands
            .iter()
            .map(|review| {
                format!(
                    "{}  {}  {}\n  cwd: {}\n  executable: {}\n  argv: {}",
                    if review.approved {
                        "approved"
                    } else {
                        "PENDING"
                    },
                    review.fingerprint,
                    review.project,
                    review.cwd.display(),
                    review.command.program.display(),
                    serde_json::to_string(&review.command.args).unwrap()
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        LayoutCommand::Approve { .. } => {
            "reviewed command fingerprints approved on this host".into()
        }
        _ => format!(
            "saved {} project(s) to {}\n{}",
            reply.projects.len(),
            reply.path.display(),
            summaries(&reply.projects)
        ),
    };
    output(cli.json, "layout", &reply, text)
}

fn summaries(projects: &[crate::layout::LayoutSummary]) -> String {
    if projects.is_empty() {
        return "no saved projects".into();
    }
    projects
        .iter()
        .map(|p| {
            format!(
                "{}: {} workspaces, {} tabs, {} panes; {} command(s) need approval",
                p.name, p.workspaces, p.tabs, p.panes, p.pending_commands
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
