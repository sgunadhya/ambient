use std::sync::Arc;
use std::thread;

use ambient_core::{
    CoreError, RawEvent, RawPayload, Result, SourceId, StreamProvider, StreamTransport,
    TransportId, TransportState, TransportStatus,
};
use chrono::Utc;

pub trait HealthKitProvider: Send + Sync {
    fn initial_samples(&self) -> Vec<RawPayload>;
}

pub struct DefaultHealthKitProvider;

impl HealthKitProvider for DefaultHealthKitProvider {
    fn initial_samples(&self) -> Vec<RawPayload> {
        let now = Utc::now();
        vec![
            RawPayload::HealthKitSample {
                metric: ambient_core::HealthMetric::HRV,
                value: 0.0,
                unit: "ms".to_string(),
                recorded_at: now,
            },
            RawPayload::HealthKitSample {
                metric: ambient_core::HealthMetric::SleepQuality,
                value: 0.0,
                unit: "score".to_string(),
                recorded_at: now,
            },
        ]
    }
}

pub struct HealthKitTransport {
    provider: Arc<dyn HealthKitProvider>,
}

impl HealthKitTransport {
    pub fn new(provider: Arc<dyn HealthKitProvider>) -> Self {
        Self { provider }
    }
}

impl StreamTransport for HealthKitTransport {
    fn transport_id(&self) -> TransportId {
        "healthkit".to_string()
    }

    fn start(&self, provider: Arc<dyn StreamProvider>) -> Result<tokio::task::JoinHandle<()>> {
        let provider_ref = Arc::clone(&self.provider);
        thread::Builder::new()
            .name("healthkit-transport".to_string())
            .spawn(move || {
                for payload in provider_ref.initial_samples() {
                    let _ = provider.append_raw(RawEvent {
                        source: SourceId::new("healthkit"),
                        timestamp: Utc::now(),
                        payload,
                    });
                }
            })
            .map_err(|e| {
                CoreError::Internal(format!("failed to spawn healthkit transport: {e}"))
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
        // Simple stub for status since this is a one-shot simulation right now
        TransportStatus {
            transport_id: self.transport_id(),
            state: TransportState::Active,
            peers: Vec::new(),
            last_event_at: None,
            events_ingested: 0,
        }
    }
}
