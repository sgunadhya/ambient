use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ambient_core::{
    CoreError, LoadAware, PulseEvent, PulseSignal, Result, StreamProvider, StreamTransport,
    SystemLoad, TransportId, TransportState, TransportStatus,
};
use chrono::{DateTime, Utc};

use crate::active_app_provider::ActiveAppProvider;

const CONTEXT_WINDOW_SECS: i64 = 60;

pub struct ContextSwitchTransport {
    provider: Arc<dyn ActiveAppProvider>,
    load: Arc<AtomicU8>,
    tick: Duration,
}

impl ContextSwitchTransport {
    pub fn new(provider: Arc<dyn ActiveAppProvider>) -> Self {
        Self {
            provider,
            load: Arc::new(AtomicU8::new(0)),
            tick: Duration::from_secs(5),
        }
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }
}

impl StreamTransport for ContextSwitchTransport {
    fn transport_id(&self) -> TransportId {
        "context_switch".to_string()
    }

    fn start(&self, provider: Arc<dyn StreamProvider>) -> Result<tokio::task::JoinHandle<()>> {
        let app_provider = Arc::clone(&self.provider);
        let load_ref = Arc::clone(&self.load);
        let tick = self.tick;

        thread::Builder::new()
            .name("context-switch-transport".to_string())
            .spawn(move || {
                let mut previous_bundle: Option<String> = None;
                let mut switches: VecDeque<DateTime<Utc>> = VecDeque::new();

                loop {
                    thread::sleep(tick);
                    if load_ref.load(Ordering::Relaxed) == 2 {
                        // Minimal
                        continue;
                    }

                    let now = Utc::now();
                    if let Some(bundle_id) = app_provider.active_bundle_id() {
                        if previous_bundle
                            .as_ref()
                            .is_some_and(|prev| prev != &bundle_id)
                        {
                            switches.push_back(now);
                        }
                        previous_bundle = Some(bundle_id);
                    }

                    while switches.front().is_some_and(|ts| {
                        (*ts + chrono::Duration::seconds(CONTEXT_WINDOW_SECS)) < now
                    }) {
                        let _ = switches.pop_front();
                    }

                    let _ = provider.append_pulse(PulseEvent {
                        timestamp: now,
                        signal: PulseSignal::ContextSwitchRate {
                            switches_per_minute: switches.len() as f32,
                        },
                    });
                }
            })
            .map_err(|e| {
                CoreError::Internal(format!("failed to spawn context switch transport: {e}"))
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

impl LoadAware for ContextSwitchTransport {
    fn on_load_change(&self, load: SystemLoad) {
        let val = match load {
            SystemLoad::Unconstrained => 0,
            SystemLoad::Conservative => 1,
            SystemLoad::Minimal => 2,
        };
        self.load.store(val, Ordering::Relaxed);
    }
}
