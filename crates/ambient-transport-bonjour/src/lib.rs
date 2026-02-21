use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use ambient_core::{
    CoreError, PeerStatus, Result, StreamProvider, StreamTransport, TransportId, TransportState,
    TransportStatus,
};
use chrono::Utc;

#[derive(Default)]
pub struct BonjourTransport {
    running: AtomicBool,
    events_ingested: AtomicU64,
}

impl StreamTransport for BonjourTransport {
    fn transport_id(&self) -> TransportId {
        "bonjour".to_string()
    }

    fn start(&self, _provider: Arc<dyn StreamProvider>) -> Result<tokio::task::JoinHandle<()>> {
        self.running.store(true, Ordering::Relaxed);
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|e| CoreError::Internal(format!("tokio runtime unavailable: {e}")))?
            .spawn(async {});
        Ok(handle)
    }

    fn stop(&self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn status(&self) -> TransportStatus {
        TransportStatus {
            transport_id: self.transport_id(),
            state: if self.running.load(Ordering::Relaxed) {
                TransportState::Active
            } else {
                TransportState::Inactive
            },
            peers: vec![PeerStatus {
                peer_id: "local-peer".to_string(),
                device_name: "Loopback".to_string(),
                last_seen: Utc::now(),
                events_received: self.events_ingested.load(Ordering::Relaxed),
                reachable: self.running.load(Ordering::Relaxed),
            }],
            last_event_at: Some(Utc::now()),
            events_ingested: self.events_ingested.load(Ordering::Relaxed),
        }
    }

    fn save_state(&self) -> Result<Option<Vec<u8>>> {
        Ok(Some(
            self.events_ingested
                .load(Ordering::Relaxed)
                .to_le_bytes()
                .to_vec(),
        ))
    }

    fn load_state(&self, state: &[u8]) -> Result<()> {
        if state.len() == 8 {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(state);
            self.events_ingested
                .store(u64::from_le_bytes(bytes), Ordering::Relaxed);
        }
        Ok(())
    }

    fn on_push_notification(&self, _payload: Vec<u8>) -> Result<()> {
        self.events_ingested.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
