use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::{
    domain::{ClientId, TerminalSize},
    terminal::{AttachmentGeometry, TerminalHandle},
};

#[derive(Clone, Default)]
pub struct AttachmentLease(Arc<Mutex<AttachmentState>>);

#[derive(Default)]
struct AttachmentState {
    sizes: HashMap<ClientId, TerminalSize>,
    revision: u64,
}

impl AttachmentLease {
    pub fn acquire(
        &self,
        client: ClientId,
        size: TerminalSize,
        terminal: Arc<TerminalHandle>,
    ) -> Option<LeaseAcquisition> {
        let mut state = self.0.lock().ok()?;
        if state.sizes.contains_key(&client) {
            return None;
        }
        state.sizes.insert(client, size);
        let geometry = geometry(&mut state)?;
        Some(LeaseAcquisition {
            guard: LeaseGuard {
                lease: self.clone(),
                client,
                terminal,
            },
            geometry,
        })
    }

    #[cfg(test)]
    fn sizes(&self) -> Vec<TerminalSize> {
        self.0.lock().unwrap().sizes.values().copied().collect()
    }
}

pub struct LeaseAcquisition {
    pub guard: LeaseGuard,
    pub geometry: AttachmentGeometry,
}

pub struct LeaseGuard {
    lease: AttachmentLease,
    client: ClientId,
    terminal: Arc<TerminalHandle>,
}

impl LeaseGuard {
    pub fn resize(&self, size: TerminalSize) -> Option<AttachmentGeometry> {
        let mut state = self.lease.0.lock().ok()?;
        *state.sizes.get_mut(&self.client)? = size;
        geometry(&mut state)
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let geometry = self.lease.0.lock().ok().and_then(|mut state| {
            state.sizes.remove(&self.client)?;
            geometry(&mut state)
        });
        if let Some(geometry) = geometry {
            self.terminal.resize_on_attachment_change(geometry);
        }
    }
}

fn geometry(state: &mut AttachmentState) -> Option<AttachmentGeometry> {
    let size = state
        .sizes
        .values()
        .copied()
        .reduce(|smallest, size| TerminalSize {
            columns: smallest.columns.min(size.columns),
            rows: smallest.rows.min(size.rows),
        })?;
    state.revision = state
        .revision
        .checked_add(1)
        .expect("attachment revision overflow");
    Some(AttachmentGeometry {
        revision: state.revision,
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_each_attachment_size() {
        let lease = AttachmentLease::default();
        let first = ClientId::new();
        // Lease guards need a real terminal only when dropped; exercise the
        // pure size selection directly instead.
        let mut state = AttachmentState::default();
        state.sizes.insert(
            first,
            TerminalSize {
                columns: 120,
                rows: 40,
            },
        );
        state.sizes.insert(
            ClientId::new(),
            TerminalSize {
                columns: 80,
                rows: 50,
            },
        );
        assert_eq!(
            geometry(&mut state).map(|geometry| geometry.size),
            Some(TerminalSize {
                columns: 80,
                rows: 40
            })
        );
        assert!(lease.sizes().is_empty());
    }
}
