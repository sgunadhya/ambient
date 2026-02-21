use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ambient_core::{
    CoreError, LoadAware, PulseEvent, PulseSignal, Result, StreamProvider, StreamTransport,
    SystemLoad, TransportId, TransportState, TransportStatus,
};
use chrono::Utc;

use crate::active_app_provider::{load_window_title_allowlist, ActiveAppProvider};

const AX_TIMEOUT_MS: u64 = 500;

pub struct ActiveAppTransport {
    provider: Arc<dyn ActiveAppProvider>,
    load: Arc<AtomicU8>,
    tick: Duration,
    allowlist: Arc<Vec<String>>,
}

impl ActiveAppTransport {
    pub fn new(provider: Arc<dyn ActiveAppProvider>) -> Self {
        Self {
            provider,
            load: Arc::new(AtomicU8::new(0)),
            tick: Duration::from_secs(5),
            allowlist: Arc::new(load_window_title_allowlist()),
        }
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }
}

impl StreamTransport for ActiveAppTransport {
    fn transport_id(&self) -> TransportId {
        "active_app".to_string()
    }

    fn start(&self, provider: Arc<dyn StreamProvider>) -> Result<tokio::task::JoinHandle<()>> {
        let app_provider = Arc::clone(&self.provider);
        let load_ref = Arc::clone(&self.load);
        let tick = self.tick;
        let allowlist = Arc::clone(&self.allowlist);

        thread::Builder::new()
            .name("active-app-transport".to_string())
            .spawn(move || loop {
                thread::sleep(tick);

                let now = Utc::now();
                let Some(bundle_id) = app_provider.active_bundle_id() else {
                    continue;
                };

                let window_title = if load_ref.load(Ordering::Relaxed) == 2 {
                    // Minimal
                    None
                } else if allowlist.iter().any(|allowed| allowed == &bundle_id) {
                    read_window_title_with_timeout(Arc::clone(&app_provider), AX_TIMEOUT_MS)
                } else {
                    None
                };

                let _ = provider.append_pulse(PulseEvent {
                    timestamp: now,
                    signal: PulseSignal::ActiveApp {
                        bundle_id,
                        window_title,
                    },
                });
            })
            .map_err(|e| {
                CoreError::Internal(format!("failed to spawn active app transport: {e}"))
            })?;

        let handle = tokio::runtime::Handle::try_current()
            .map_err(|e| CoreError::Internal(format!("tokio runtime unavailable: {e}")))?
            .spawn(async {});
        Ok(handle)
    }

    fn stop(&self) -> Result<()> {
        Ok(())
    }

    fn status(&self) -> TransportStatus {
        TransportStatus {
            transport_id: self.transport_id(),
            state: TransportState::Active,
            peers: Vec::new(),
            last_event_at: None,
            events_ingested: 0,
        }
    }
}

impl LoadAware for ActiveAppTransport {
    fn on_load_change(&self, load: SystemLoad) {
        let val = match load {
            SystemLoad::Unconstrained => 0,
            SystemLoad::Conservative => 1,
            SystemLoad::Minimal => 2,
        };
        self.load.store(val, Ordering::Relaxed);
    }
}

fn read_window_title_with_timeout(
    provider: Arc<dyn ActiveAppProvider>,
    timeout_ms: u64,
) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(provider.active_window_title());
    });

    rx.recv_timeout(Duration::from_millis(timeout_ms))
        .ok()
        .flatten()
}
