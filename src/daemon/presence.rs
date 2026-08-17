use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

use tokio::sync::watch;

use crate::{
    domain::{ClientId, SessionId},
    protocol::{ClientPresenceSnapshot, SessionPresence},
};

#[derive(Clone)]
pub(super) struct ClientPresence {
    state: Arc<Mutex<ClientPresenceState>>,
    changes: watch::Sender<ClientPresenceSnapshot>,
}

#[derive(Default)]
struct ClientPresenceState {
    revision: u64,
    clients: HashMap<ClientId, SessionId>,
}

impl Default for ClientPresence {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(ClientPresenceState::default())),
            changes: watch::channel(ClientPresenceSnapshot::default()).0,
        }
    }
}

impl ClientPresence {
    pub(super) fn attach(&self, client: ClientId, session_id: SessionId) -> ClientPresenceGuard {
        self.set(client, session_id);
        ClientPresenceGuard {
            presence: self.clone(),
            client,
        }
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<ClientPresenceSnapshot> {
        self.changes.subscribe()
    }

    pub(super) fn snapshot(&self) -> ClientPresenceSnapshot {
        self.changes.borrow().clone()
    }

    fn set(&self, client: ClientId, session_id: SessionId) {
        let mut state = self.state.lock().expect("client presence lock poisoned");
        if state.clients.insert(client, session_id) == Some(session_id) {
            return;
        }
        state.revision = state
            .revision
            .checked_add(1)
            .expect("client presence revision overflow");
        self.changes.send_replace(snapshot(&state));
    }

    fn remove(&self, client: ClientId) {
        let mut state = self.state.lock().expect("client presence lock poisoned");
        if state.clients.remove(&client).is_none() {
            return;
        }
        state.revision = state
            .revision
            .checked_add(1)
            .expect("client presence revision overflow");
        self.changes.send_replace(snapshot(&state));
    }
}

pub(super) struct ClientPresenceGuard {
    presence: ClientPresence,
    client: ClientId,
}

impl ClientPresenceGuard {
    pub(super) fn select(&mut self, session_id: SessionId) {
        self.presence.set(self.client, session_id);
    }
}

impl Drop for ClientPresenceGuard {
    fn drop(&mut self) {
        self.presence.remove(self.client);
    }
}

fn snapshot(state: &ClientPresenceState) -> ClientPresenceSnapshot {
    let mut sessions = BTreeMap::<SessionId, u32>::new();
    for session_id in state.clients.values() {
        *sessions.entry(*session_id).or_default() += 1;
    }
    ClientPresenceSnapshot {
        revision: state.revision,
        sessions: sessions
            .into_iter()
            .map(|(session_id, clients)| SessionPresence {
                session_id,
                clients,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_sessions_and_follows_guard_lifetimes() {
        let presence = ClientPresence::default();
        let first_session = SessionId::new();
        let second_session = SessionId::new();
        let first_client = ClientId::new();
        let second_client = ClientId::new();

        let mut first = presence.attach(first_client, first_session);
        let second = presence.attach(second_client, first_session);
        assert_eq!(presence.snapshot().client_count(first_session), 2);

        first.select(second_session);
        assert_eq!(presence.snapshot().client_count(first_session), 1);
        assert_eq!(presence.snapshot().client_count(second_session), 1);

        drop(second);
        assert_eq!(presence.snapshot().client_count(first_session), 0);
        drop(first);
        assert!(presence.snapshot().sessions.is_empty());
    }
}
