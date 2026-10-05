//! Recovery of the active SSH attachment. Never replay input or resource commands.
use super::*;
use crate::protocol::remote::Capability;
use futures_util::FutureExt;

const QUIET_INTERVAL: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const STABLE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
#[error("connection lost: {0}")]
pub(super) struct Lost(#[source] pub std::io::Error);

impl From<std::io::Error> for Lost {
    fn from(error: std::io::Error) -> Self {
        Self(error)
    }
}

impl Lost {
    pub(super) fn closed() -> Self {
        Self(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "SSH connection closed",
        ))
    }

    pub(super) fn retryable(error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<Self>()
            .is_some_and(|lost| lost.0.kind() != std::io::ErrorKind::InvalidData)
    }
}

pub(super) struct Health {
    last_activity: time::Instant,
    pending: Option<(Uuid, time::Instant)>,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            last_activity: time::Instant::now(),
            pending: None,
        }
    }
}

impl Health {
    pub(super) fn receive(&mut self, request_id: Option<Uuid>, message: &ServerMessage) {
        self.last_activity = time::Instant::now();
        if self.pending.is_some_and(|(id, _)| Some(id) == request_id)
            && matches!(
                message,
                ServerMessage::Pong { .. } | ServerMessage::Resources { .. }
            )
        {
            self.pending = None;
        }
    }

    pub(super) fn check(
        &mut self,
        now: time::Instant,
        capabilities: crate::protocol::remote::Capabilities,
    ) -> Result<Option<(Uuid, ClientMessage)>, Lost> {
        if let Some((_, sent)) = self.pending {
            if now.duration_since(sent) >= PROBE_TIMEOUT {
                return Err(Lost(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "remote health check timed out",
                )));
            }
        } else if now.duration_since(self.last_activity) >= QUIET_INTERVAL {
            let id = Uuid::new_v4();
            self.pending = Some((id, now));
            let message = if capabilities.contains(Capability::Health) {
                ClientMessage::Ping
            } else {
                ClientMessage::ListResources
            };
            return Ok(Some((id, message)));
        }
        Ok(None)
    }
}

pub(super) struct Backoff {
    failures: u32,
    connected_at: Option<time::Instant>,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            failures: 0,
            connected_at: Some(time::Instant::now()),
        }
    }
}

impl Backoff {
    pub(super) fn connected(&mut self) {
        self.connected_at = Some(time::Instant::now());
    }

    fn next_delay(&mut self) -> Duration {
        if self
            .connected_at
            .take()
            .is_some_and(|at| at.elapsed() >= STABLE_INTERVAL)
        {
            self.failures = 0;
        }
        let seconds = (1_u64 << self.failures.min(7)).min(120);
        self.failures = self.failures.saturating_add(1);
        Duration::from_secs(seconds)
    }
}

/// Show a dimmed reconnect surface while accepting only cancellation.
/// Reconnects use BatchMode so credentials can never consume raw terminal input.
pub(super) async fn recover(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    events: &mut EventStream,
    target: &str,
    selector: TargetSelector,
    config_location: &config::ConfigLocation,
    backoff: &mut Backoff,
) -> anyhow::Result<Option<(PreparedMachineAttachment, ViewState, ResourceState)>> {
    let staged = stage_ui_config(config_location)?;
    terminal.draw(|frame| {
        let area = frame.area();
        // The reconnect surface deliberately contains no actionable cached panes.
        frame.render_widget(
            ratatui::widgets::Paragraph::new(format!(
                "Reconnecting to {}…\nInput paused · Esc or Ctrl+C to detach",
                sanitize(target)
            ))
            .style(Style::default().add_modifier(Modifier::DIM)),
            area,
        );
    })?;
    terminal.hide_cursor()?;
    let mut termination = TerminationSignals::subscribe()?;
    loop {
        let delay = backoff.next_delay();
        let attempt = async {
            time::sleep(delay).await;
            reconnect(target, selector.clone(), &staged, terminal.size()?.into()).await
        };
        tokio::pin!(attempt);
        loop {
            tokio::select! {
                name = termination.recv() => bail!("terminated by {name}"),
                event = events.next() => match event {
                    None => return Ok(None),
                    Some(Err(error)) => return Err(error.into()),
                    Some(Ok(event)) if cancels_recovery(&event) => return Ok(None),
                    _ => {} // Discard input received while disconnected; never replay it.
                },
                result = &mut attempt => {
                    match result {
                        Ok(ready) => {
                            // A completed connection and buffered keyboard events can be ready
                            // together. Drain those events before enabling input on the new view.
                            while let Some(event) = events.next().now_or_never() {
                                match event {
                                    None => return Ok(None),
                                    Some(Err(error)) => return Err(error.into()),
                                    Some(Ok(event)) if cancels_recovery(&event) => return Ok(None),
                                    _ => {}
                                }
                            }
                            backoff.connected();
                            return Ok(Some(ready));
                        }
                        Err(error) if error.downcast_ref::<crate::protocol::remote::EndpointError>().is_some()
                            || error.downcast_ref::<remote::OperationError>().is_some()
                            || error.downcast_ref::<Attention>().is_some() => return Err(error),
                        Err(_) => break,
                    }
                }
            }
        }
    }
}

fn cancels_recovery(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind != KeyEventKind::Release
        && (key.code == KeyCode::Esc || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))))
}

#[derive(Debug, thiserror::Error)]
#[error("SSH needs attention: {0}; verify `ssh HOST` in another terminal, then attach again")]
pub(super) struct Attention(pub(super) String);

async fn reconnect(
    target: &str,
    selector: TargetSelector,
    staged: &StagedUiConfig,
    size: Rect,
) -> anyhow::Result<(PreparedMachineAttachment, ViewState, ResourceState)> {
    time::timeout(Duration::from_secs(30), async {
        let mut prepared = remote::prepare_attachment(
            target,
            selector,
            TerminalSize {
                columns: size.width,
                rows: size.height,
            },
            staged,
        )
        .await?;
        let mut view = ViewState::new(Locality::Remote, prepared.selected.clone())?;
        let resources = stage_machine_view(&mut prepared, &mut view, size).await?;
        Ok((prepared, view, resources))
    })
    .await
    .context("remote reconnection timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::remote::Capabilities;

    #[test]
    fn quiet_connections_require_correlated_health_replies() {
        let now = time::Instant::now();
        let mut health = Health {
            last_activity: now - QUIET_INTERVAL,
            pending: None,
        };
        let (id, message) = health.check(now, Capabilities::ALL).unwrap().unwrap();
        assert_eq!(message, ClientMessage::Ping);
        health.receive(Some(Uuid::new_v4()), &ServerMessage::Pong { daemon_pid: 1 });
        assert!(health.pending.is_some());
        // Unrelated traffic cannot hide a lost probe response.
        health.receive(None, &ServerMessage::Pong { daemon_pid: 1 });
        assert!(
            health
                .check(now + PROBE_TIMEOUT, Capabilities::ALL)
                .is_err()
        );
        health.receive(Some(id), &ServerMessage::Pong { daemon_pid: 1 });
        assert!(health.pending.is_none());
        assert!(
            health
                .check(time::Instant::now(), Capabilities::ALL)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn optional_health_uses_metadata_fallback() {
        let now = time::Instant::now();
        let mut health = Health {
            last_activity: now - QUIET_INTERVAL,
            pending: None,
        };
        let (_, message) = health.check(now, Capabilities::default()).unwrap().unwrap();
        assert_eq!(message, ClientMessage::ListResources);
    }

    #[test]
    fn backoff_survives_brief_connections_and_resets_after_stability() {
        let mut retry = Backoff::default();
        assert_eq!(retry.next_delay(), Duration::from_secs(1));
        retry.connected();
        assert_eq!(retry.next_delay(), Duration::from_secs(2));
        for _ in 0..20 {
            retry.next_delay();
        }
        assert_eq!(retry.next_delay(), Duration::from_secs(120));
        retry.connected_at = Some(time::Instant::now() - STABLE_INTERVAL);
        assert_eq!(retry.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn corrupt_frames_are_not_retried_as_network_failures() {
        let malformed: anyhow::Error =
            Lost(std::io::Error::from(std::io::ErrorKind::InvalidData)).into();
        assert!(!Lost::retryable(&malformed));
        assert!(Lost::retryable(&Lost::closed().into()));
    }
}
