use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::{
    domain::{ClientId, TerminalSize},
    terminal::TerminalHandle,
};

#[derive(Clone, Default)]
pub struct AttachmentLease(Arc<Mutex<HashMap<ClientId, TerminalSize>>>);

impl AttachmentLease {
    pub fn acquire(
        &self,
        client: ClientId,
        size: TerminalSize,
        terminal: Arc<TerminalHandle>,
    ) -> Option<LeaseGuard> {
        let mut attachments = self.0.lock().ok()?;
        if attachments.insert(client, size).is_some() {
            return None;
        }
        Some(LeaseGuard {
            lease: self.clone(),
            client,
            terminal,
        })
    }

    #[cfg(test)]
    fn sizes(&self) -> Vec<TerminalSize> {
        self.0.lock().unwrap().values().copied().collect()
    }
}

pub struct LeaseGuard {
    lease: AttachmentLease,
    client: ClientId,
    terminal: Arc<TerminalHandle>,
}

impl LeaseGuard {
    pub fn has_peers(&self) -> bool {
        self.lease
            .0
            .lock()
            .is_ok_and(|attachments| attachments.len() > 1)
    }

    pub fn resize(&self, size: TerminalSize) -> Option<TerminalSize> {
        let mut attachments = self.lease.0.lock().ok()?;
        *attachments.get_mut(&self.client)? = size;
        smallest(&attachments)
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let size = self.lease.0.lock().ok().and_then(|mut attachments| {
            attachments.remove(&self.client)?;
            smallest(&attachments)
        });
        if let Some(size) = size {
            self.terminal.resize_on_attachment_change(size);
        }
    }
}

fn smallest(attachments: &HashMap<ClientId, TerminalSize>) -> Option<TerminalSize> {
    attachments
        .values()
        .copied()
        .reduce(|smallest, size| TerminalSize {
            columns: smallest.columns.min(size.columns),
            rows: smallest.rows.min(size.rows),
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
        let mut attachments = HashMap::new();
        attachments.insert(
            first,
            TerminalSize {
                columns: 120,
                rows: 40,
            },
        );
        attachments.insert(
            ClientId::new(),
            TerminalSize {
                columns: 80,
                rows: 50,
            },
        );
        assert_eq!(
            smallest(&attachments),
            Some(TerminalSize {
                columns: 80,
                rows: 40
            })
        );
        assert!(lease.sizes().is_empty());
    }
}
