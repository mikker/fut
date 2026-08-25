//! Compact daemon-owned terminal bell state with independent per-client cursors.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::{ClientId, TerminalId};

pub const MAX_ALERT_CLIENTS: usize = 128;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AlertCursor {
    pub bell_count: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalAlertState {
    pub bell_count: u64,
    pub last_bell_at_ms: u64,
}

impl TerminalAlertState {
    pub fn cursor(self) -> AlertCursor {
        AlertCursor {
            bell_count: self.bell_count,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalAlertSnapshot {
    pub terminal_id: TerminalId,
    pub state: TerminalAlertState,
    pub seen: AlertCursor,
}

impl TerminalAlertSnapshot {
    pub fn unseen(self) -> bool {
        self.state.bell_count > self.seen.bell_count
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClientAlertSnapshot {
    pub revision: u64,
    pub terminals: Vec<TerminalAlertSnapshot>,
}

#[derive(Clone, Debug, Default)]
struct ClientState {
    attached: bool,
    last_used: u64,
    cursors: BTreeMap<TerminalId, AlertCursor>,
}

#[derive(Clone, Debug, Default)]
pub struct AlertStore {
    revision: u64,
    terminals: BTreeMap<TerminalId, TerminalAlertState>,
    clients: BTreeMap<ClientId, ClientState>,
}

impl AlertStore {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn register_terminal(&mut self, terminal_id: TerminalId) {
        self.terminals.entry(terminal_id).or_default();
    }

    pub fn remove_terminal(&mut self, terminal_id: TerminalId) {
        if self.terminals.remove(&terminal_id).is_none() {
            return;
        }
        for client in self.clients.values_mut() {
            client.cursors.remove(&terminal_id);
        }
        self.bump();
    }

    pub fn attach(&mut self, client_id: ClientId) {
        let is_new = !self.clients.contains_key(&client_id);
        let current = self
            .terminals
            .iter()
            .map(|(id, state)| (*id, state.cursor()))
            .collect::<Vec<_>>();
        let client = self.clients.entry(client_id).or_default();
        client.attached = true;
        client.last_used = self.revision;
        if is_new {
            client.cursors.extend(current);
        }
        self.prune_clients();
    }

    pub fn detach(&mut self, client_id: ClientId) {
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.attached = false;
            client.last_used = self.revision;
        }
    }

    pub fn record_bells(&mut self, terminal_id: TerminalId, bell_count: u64, now_ms: u64) -> bool {
        let Some(state) = self.terminals.get_mut(&terminal_id) else {
            return false;
        };
        if bell_count <= state.bell_count {
            return false;
        }
        state.bell_count = bell_count;
        state.last_bell_at_ms = now_ms;
        self.bump();
        true
    }

    pub fn acknowledge(
        &mut self,
        client_id: ClientId,
        terminal_id: TerminalId,
        observed: AlertCursor,
    ) -> Result<bool, &'static str> {
        let Some(state) = self.terminals.get(&terminal_id) else {
            return Ok(false);
        };
        let client = self
            .clients
            .get_mut(&client_id)
            .ok_or("unknown alert client")?;
        let cursor = client.cursors.entry(terminal_id).or_default();
        let before = *cursor;
        cursor.bell_count = cursor
            .bell_count
            .max(observed.bell_count.min(state.bell_count));
        let changed = *cursor != before;
        if changed {
            self.bump();
        }
        Ok(changed)
    }

    pub fn snapshot(&self, client_id: ClientId) -> ClientAlertSnapshot {
        let cursors = self.clients.get(&client_id).map(|client| &client.cursors);
        ClientAlertSnapshot {
            revision: self.revision,
            terminals: self
                .terminals
                .iter()
                .map(|(terminal_id, state)| TerminalAlertSnapshot {
                    terminal_id: *terminal_id,
                    state: *state,
                    seen: cursors
                        .and_then(|cursors| cursors.get(terminal_id))
                        .copied()
                        .unwrap_or_default(),
                })
                .collect(),
        }
    }

    fn bump(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }

    fn prune_clients(&mut self) {
        while self.clients.len() > MAX_ALERT_CLIENTS {
            let Some(id) = self
                .clients
                .iter()
                .filter(|(_, client)| !client.attached)
                .min_by_key(|(_, client)| client.last_used)
                .map(|(id, _)| *id)
            else {
                break;
            };
            self.clients.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(store: &AlertStore, client: ClientId, terminal: TerminalId) -> TerminalAlertSnapshot {
        store
            .snapshot(client)
            .terminals
            .into_iter()
            .find(|alert| alert.terminal_id == terminal)
            .unwrap()
    }

    #[test]
    fn clients_have_independent_seen_state_across_detach() {
        let terminal = TerminalId::new();
        let first = ClientId::new();
        let second = ClientId::new();
        let mut store = AlertStore::default();
        store.register_terminal(terminal);
        store.attach(first);
        store.attach(second);
        store.record_bells(terminal, 1, 10);
        let observed = alert(&store, first, terminal).state.cursor();
        store.acknowledge(first, terminal, observed).unwrap();
        assert!(!alert(&store, first, terminal).unseen());
        assert!(alert(&store, second, terminal).unseen());

        store.detach(second);
        store.record_bells(terminal, 2, 20);
        store.attach(second);
        assert!(alert(&store, second, terminal).unseen());
    }

    #[test]
    fn stale_acknowledgement_cannot_clear_a_repeated_bell() {
        let terminal = TerminalId::new();
        let client = ClientId::new();
        let mut store = AlertStore::default();
        store.register_terminal(terminal);
        store.attach(client);
        store.record_bells(terminal, 1, 10);
        let stale = alert(&store, client, terminal).state.cursor();
        store.record_bells(terminal, 2, 11);
        store.acknowledge(client, terminal, stale).unwrap();
        assert!(alert(&store, client, terminal).unseen());
    }

    #[test]
    fn repeated_bells_keep_one_bounded_latest_value() {
        let terminal = TerminalId::new();
        let mut store = AlertStore::default();
        store.register_terminal(terminal);
        for count in 1..=10_000 {
            store.record_bells(terminal, count, count);
        }
        let json = serde_json::to_value(store.snapshot(ClientId::new())).unwrap();
        assert_eq!(json["terminals"].as_array().unwrap().len(), 1);
        assert_eq!(json["terminals"][0]["state"]["bell_count"], 10_000);
        assert!(json["terminals"][0]["state"].get("events").is_none());
    }
}
