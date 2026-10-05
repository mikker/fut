use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::{
    domain::{ClientId, TerminalSize},
    terminal::{AttachmentConfiguration, TerminalHandle},
};

#[derive(Clone, Default)]
pub struct AttachmentLease(Arc<Mutex<AttachmentState>>);

#[derive(Default)]
struct AttachmentState {
    clients: HashMap<ClientId, AttachedClient>,
    revision: u64,
    // Keep one palette authority while multiple clients share a PTY. The first
    // reporting attachment owns it until detach; then choose a surviving reporter.
    color_owner: Option<ClientId>,
}

struct AttachedClient {
    size: TerminalSize,
    colors: Option<crate::domain::TerminalColors>,
}

impl AttachmentLease {
    pub fn acquire(
        &self,
        client: ClientId,
        size: TerminalSize,
        terminal: Arc<TerminalHandle>,
    ) -> Option<LeaseAcquisition> {
        let mut state = self.0.lock().ok()?;
        if state.clients.contains_key(&client) {
            return None;
        }
        state
            .clients
            .insert(client, AttachedClient { size, colors: None });
        let configuration = configuration(&mut state)?;
        Some(LeaseAcquisition {
            guard: LeaseGuard {
                lease: self.clone(),
                client,
                terminal,
            },
            configuration,
        })
    }
}

pub struct LeaseAcquisition {
    pub guard: LeaseGuard,
    pub configuration: AttachmentConfiguration,
}

pub struct LeaseGuard {
    lease: AttachmentLease,
    client: ClientId,
    terminal: Arc<TerminalHandle>,
}

impl LeaseGuard {
    pub fn set_colors(
        &self,
        colors: crate::domain::TerminalColors,
    ) -> Option<AttachmentConfiguration> {
        let mut state = self.lease.0.lock().ok()?;
        state.clients.get_mut(&self.client)?.colors = Some(colors);
        state.color_owner.get_or_insert(self.client);
        configuration(&mut state)
    }

    pub fn resize(&self, size: TerminalSize) -> Option<AttachmentConfiguration> {
        let mut state = self.lease.0.lock().ok()?;
        state.clients.get_mut(&self.client)?.size = size;
        configuration(&mut state)
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let configuration = self.lease.0.lock().ok().and_then(|mut state| {
            state.clients.remove(&self.client)?;
            if state.color_owner == Some(self.client) {
                state.color_owner = state
                    .clients
                    .iter()
                    .filter_map(|(id, client)| client.colors.map(|_| *id))
                    .min();
            }
            configuration(&mut state)
        });
        if let Some(configuration) = configuration {
            self.terminal.configure_on_attachment_change(configuration);
        }
    }
}

fn configuration(state: &mut AttachmentState) -> Option<AttachmentConfiguration> {
    let size = state
        .clients
        .values()
        .map(|client| client.size)
        .reduce(|smallest, size| TerminalSize {
            columns: smallest.columns.min(size.columns),
            rows: smallest.rows.min(size.rows),
        })?;
    state.revision = state
        .revision
        .checked_add(1)
        .expect("attachment revision overflow");
    Some(AttachmentConfiguration {
        colors: state
            .color_owner
            .and_then(|owner| state.clients.get(&owner).and_then(|client| client.colors)),
        revision: state.revision,
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn color_authority_survives_other_clients_updates_and_transfers_on_detach() {
        let lease = AttachmentLease::default();
        let size = TerminalSize {
            columns: 30,
            rows: 5,
        };
        let terminal = Arc::new(
            crate::terminal::spawn_terminal(crate::terminal::SpawnSpec {
                terminal: crate::terminal::TerminalConfig::default(),
                id: crate::domain::TerminalId::new(),
                program: "/bin/sh".into(),
                argv: vec!["-c".into(), "sleep 60".into()],
                cwd: "/".into(),
                env: HashMap::new(),
                size,
            })
            .unwrap(),
        );
        let first = lease
            .acquire(ClientId::new(), size, Arc::clone(&terminal))
            .unwrap()
            .guard;
        let second = lease
            .acquire(ClientId::new(), size, Arc::clone(&terminal))
            .unwrap()
            .guard;
        let light = crate::domain::TerminalColors {
            background: Some(crate::domain::Rgb {
                red: 255,
                green: 255,
                blue: 255,
            }),
            ..Default::default()
        };
        let dark = crate::domain::TerminalColors {
            background: Some(crate::domain::Rgb {
                red: 0,
                green: 0,
                blue: 0,
            }),
            ..Default::default()
        };
        let first_update = first.set_colors(light).unwrap();
        let second_update = second.set_colors(dark).unwrap();
        assert_eq!(first_update.colors, Some(light));
        assert_eq!(second_update.colors, Some(light));
        assert!(second_update.revision > first_update.revision);
        drop(first);
        assert_eq!(second.resize(size).unwrap().colors, Some(dark));
        drop(second);
        let replacement = lease
            .acquire(ClientId::new(), size, Arc::clone(&terminal))
            .unwrap();
        assert_eq!(
            replacement.configuration.colors, None,
            "unreported colors must not invent a theme"
        );
        drop(replacement);
        terminal.close().await.unwrap();
    }

    #[test]
    fn tracks_each_attachment_size() {
        let first = ClientId::new();
        // Lease guards need a real terminal only when dropped; exercise the
        // pure size selection directly instead.
        let mut state = AttachmentState::default();
        state.clients.insert(
            first,
            AttachedClient {
                size: TerminalSize {
                    columns: 120,
                    rows: 40,
                },
                colors: None,
            },
        );
        state.clients.insert(
            ClientId::new(),
            AttachedClient {
                size: TerminalSize {
                    columns: 80,
                    rows: 50,
                },
                colors: None,
            },
        );
        assert_eq!(
            configuration(&mut state).map(|configuration| configuration.size),
            Some(TerminalSize {
                columns: 80,
                rows: 40
            })
        );
    }
}
