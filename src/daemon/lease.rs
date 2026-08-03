use std::sync::{Arc, Mutex};

use crate::domain::ClientId;

#[derive(Clone, Default)]
pub struct AttachmentLease(Arc<Mutex<Option<ClientId>>>);

impl AttachmentLease {
    pub fn acquire(&self, client: ClientId) -> Option<LeaseGuard> {
        let mut holder = self.0.lock().ok()?;
        if holder.is_some() {
            return None;
        }
        *holder = Some(client);
        Some(LeaseGuard {
            lease: self.clone(),
            client,
        })
    }

    #[cfg(test)]
    fn holder(&self) -> Option<ClientId> {
        *self.0.lock().unwrap()
    }
}

pub struct LeaseGuard {
    lease: AttachmentLease,
    client: ClientId,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if let Ok(mut holder) = self.lease.0.lock()
            && *holder == Some(self.client)
        {
            *holder = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_holder_and_drop_releases() {
        let lease = AttachmentLease::default();
        let first = ClientId::new();
        let guard = lease.acquire(first).unwrap();
        assert_eq!(lease.holder(), Some(first));
        assert!(lease.acquire(ClientId::new()).is_none());
        drop(guard);
        assert!(lease.acquire(ClientId::new()).is_some());
    }
}
