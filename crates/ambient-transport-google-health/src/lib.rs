use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ambient_core::{
    CoreError, Result, StreamProvider, StreamTransport, TransportId, TransportState, TransportStatus,
};

#[derive(Default)]
pub struct GoogleHealthTransport {
    running: AtomicBool,
}

impl StreamTransport for GoogleHealthTransport {
    fn transport_id(&self) -> TransportId {
        "google_health".to_string()
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
                TransportState::RequiresSetup {
                    action: ambient_core::SetupAction::GrantHealthKit,
                }
            },
            peers: Vec::new(),
            last_event_at: None,
            events_ingested: 0,
        }
    }
}
