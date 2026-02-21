use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ambient_core::{
    CoreError, Result, StreamProvider, StreamTransport, TransportId, TransportState, TransportStatus,
};
use chrono::{DateTime, Utc};

use crate::normalizer::{payload_from_bytes, record_to_raw_event, CloudKitPushPayload};
use crate::native::NativeCloudKitFetcher;
use crate::token::{decode, encode, CloudKitTokenState};

pub trait CloudKitChangeFetcher: Send + Sync {
    fn fetch_changes(
        &self,
        push_payload: &[u8],
        previous_token: Option<&str>,
    ) -> Result<CloudKitPushPayload>;
}

#[derive(Default)]
pub struct JsonPayloadFetcher;

impl CloudKitChangeFetcher for JsonPayloadFetcher {
    fn fetch_changes(
        &self,
        push_payload: &[u8],
        _previous_token: Option<&str>,
    ) -> Result<CloudKitPushPayload> {
        payload_from_bytes(push_payload)
    }
}

pub struct CloudKitTransport {
    running: AtomicBool,
    events_ingested: AtomicU64,
    consecutive_failures: AtomicU64,
    container: String,
    zone_name: String,
    provider: Mutex<Option<Arc<dyn StreamProvider>>>,
    last_event_at: Mutex<Option<DateTime<Utc>>>,
    change_token: Mutex<Option<String>>,
    last_error: Mutex<Option<String>>,
    fetcher: Arc<dyn CloudKitChangeFetcher>,
}

impl Default for CloudKitTransport {
    fn default() -> Self {
        Self::with_config(
            "iCloud.dev.ambient.private".to_string(),
            "AmbientZone".to_string(),
            Arc::new(JsonPayloadFetcher),
        )
    }
}

impl CloudKitTransport {
    pub fn new(fetcher: Arc<dyn CloudKitChangeFetcher>) -> Self {
        Self::with_config(
            "iCloud.dev.ambient.private".to_string(),
            "AmbientZone".to_string(),
            fetcher,
        )
    }

    pub fn with_config(
        container: String,
        zone_name: String,
        fetcher: Arc<dyn CloudKitChangeFetcher>,
    ) -> Self {
        Self {
            running: AtomicBool::new(false),
            events_ingested: AtomicU64::new(0),
            consecutive_failures: AtomicU64::new(0),
            container,
            zone_name,
            provider: Mutex::new(None),
            last_event_at: Mutex::new(None),
            change_token: Mutex::new(None),
            last_error: Mutex::new(None),
            fetcher,
        }
    }

    pub fn with_native_bridge(container: String, zone_name: String) -> Self {
        Self::with_config(
            container.clone(),
            zone_name.clone(),
            Arc::new(NativeCloudKitFetcher::new(container, zone_name)),
        )
    }

    fn apply_push(&self, payload: Vec<u8>) -> Result<()> {
        let provider = self
            .provider
            .lock()
            .map_err(|_| CoreError::Internal("cloudkit provider lock poisoned".to_string()))?
            .clone()
            .ok_or_else(|| CoreError::Unsupported("cloudkit transport not started"))?;

        let previous_token = self
            .change_token
            .lock()
            .map_err(|_| CoreError::Internal("cloudkit token lock poisoned".to_string()))?
            .clone();
        let push = self
            .fetcher
            .fetch_changes(&payload, previous_token.as_deref())?;
        let mut appended = 0u64;
        let mut max_ts: Option<DateTime<Utc>> = None;
        for record in push.records {
            let raw = record_to_raw_event(&record)?;
            let event_ts = raw.timestamp;
            provider.append_raw_from(raw, &self.transport_id(), &record.record_id)?;
            appended += 1;
            max_ts = Some(max_ts.map(|cur| cur.max(event_ts)).unwrap_or(event_ts));
        }
        if let Some(token) = push.new_change_token {
            *self
                .change_token
                .lock()
                .map_err(|_| CoreError::Internal("cloudkit token lock poisoned".to_string()))? =
                Some(token);
        }
        if let Some(ts) = max_ts {
            *self
                .last_event_at
                .lock()
                .map_err(|_| CoreError::Internal("cloudkit last_event lock poisoned".to_string()))? =
                Some(ts);
        }
        if appended > 0 {
            self.events_ingested.fetch_add(appended, Ordering::Relaxed);
        }
        Ok(())
    }
}

impl StreamTransport for CloudKitTransport {
    fn transport_id(&self) -> TransportId {
        "cloudkit".to_string()
    }

    fn start(&self, provider: Arc<dyn StreamProvider>) -> Result<tokio::task::JoinHandle<()>> {
        {
            let mut guard = self.provider.lock().map_err(|_| {
                CoreError::Internal("cloudkit provider lock poisoned".to_string())
            })?;
            *guard = Some(provider);
        }
        self.running.store(true, Ordering::Relaxed);

        let handle = tokio::runtime::Handle::try_current()
            .map_err(|e| CoreError::Internal(format!("tokio runtime unavailable: {e}")))?
            .spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            });
        Ok(handle)
    }

    fn stop(&self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        let mut guard = self.provider.lock().map_err(|_| {
            CoreError::Internal("cloudkit provider lock poisoned".to_string())
        })?;
        *guard = None;
        Ok(())
    }

    fn status(&self) -> TransportStatus {
        let active = self.running.load(Ordering::Relaxed);
        let last_event_at = self.last_event_at.lock().ok().and_then(|g| *g);
        let has_provider = self
            .provider
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|_| true))
            .unwrap_or(false);

        let state = if active && has_provider && self.consecutive_failures.load(Ordering::Relaxed) == 0 {
            TransportState::Active
        } else if active && has_provider {
            let reason = self
                .last_error
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .unwrap_or_else(|| "cloudkit push processing failed".to_string());
            TransportState::Degraded { reason }
        } else if active {
            TransportState::Degraded {
                reason: "no stream provider".to_string(),
            }
        } else {
            TransportState::Inactive
        };

        let state = if self.container.trim().is_empty() || self.zone_name.trim().is_empty() {
            TransportState::Degraded {
                reason: "cloudkit container/zone not configured".to_string(),
            }
        } else {
            state
        };

        TransportStatus {
            transport_id: self.transport_id(),
            state,
            peers: Vec::new(),
            last_event_at,
            events_ingested: self.events_ingested.load(Ordering::Relaxed),
        }
    }

    fn save_state(&self) -> Result<Option<Vec<u8>>> {
        let state = CloudKitTokenState::new(
            self.change_token
                .lock()
                .map_err(|_| CoreError::Internal("cloudkit token lock poisoned".to_string()))?
                .clone(),
            self.events_ingested.load(Ordering::Relaxed),
            self.last_event_at
                .lock()
                .map_err(|_| CoreError::Internal("cloudkit last_event lock poisoned".to_string()))?
                .as_ref()
                .copied(),
        );
        Ok(Some(encode(&state)?))
    }

    fn load_state(&self, state: &[u8]) -> Result<()> {
        let decoded = match decode(state) {
            Ok(decoded) => decoded,
            Err(err) => {
                eprintln!("warning: cloudkit state could not be decoded, resetting token: {err}");
                self.events_ingested.store(0, Ordering::Relaxed);
                *self.last_event_at.lock().map_err(|_| {
                    CoreError::Internal("cloudkit last_event lock poisoned".to_string())
                })? = None;
                *self
                    .change_token
                    .lock()
                    .map_err(|_| CoreError::Internal("cloudkit token lock poisoned".to_string()))? =
                    None;
                return Ok(());
            }
        };
        self.events_ingested
            .store(decoded.events_ingested, Ordering::Relaxed);
        *self
            .last_event_at
            .lock()
            .map_err(|_| CoreError::Internal("cloudkit last_event lock poisoned".to_string()))? =
            decoded.last_event_at;
        *self
            .change_token
            .lock()
            .map_err(|_| CoreError::Internal("cloudkit token lock poisoned".to_string()))? =
            decoded.change_token;
        Ok(())
    }

    fn on_push_notification(&self, payload: Vec<u8>) -> Result<()> {
        let result = self.apply_push(payload);
        match &result {
            Ok(_) => {
                self.consecutive_failures.store(0, Ordering::Relaxed);
                if let Ok(mut last_error) = self.last_error.lock() {
                    *last_error = None;
                }
            }
            Err(err) => {
                self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut last_error) = self.last_error.lock() {
                    *last_error = Some(err.to_string());
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ambient_core::{ConsumerId, EventLogEntry, Offset, PulseEvent, RawEvent};
    use tokio::sync::watch;

    use super::*;

    struct TestProvider {
        raws: Mutex<Vec<(String, String, RawEvent)>>,
    }

    impl Default for TestProvider {
        fn default() -> Self {
            Self {
                raws: Mutex::new(Vec::new()),
            }
        }
    }

    impl StreamProvider for TestProvider {
        fn append_raw(&self, _event: RawEvent) -> Result<Offset> {
            Ok(0)
        }

        fn append_pulse(&self, _event: PulseEvent) -> Result<Offset> {
            Ok(0)
        }

        fn append_raw_from(&self, event: RawEvent, transport: &str, record_id: &str) -> Result<Offset> {
            self.raws
                .lock()
                .map_err(|_| CoreError::Internal("test provider lock poisoned".to_string()))?
                .push((transport.to_string(), record_id.to_string(), event));
            Ok(1)
        }

        fn read(
            &self,
            _consumer: &ConsumerId,
            _from: Offset,
            _limit: usize,
        ) -> Result<Vec<(Offset, EventLogEntry)>> {
            Ok(Vec::new())
        }

        fn commit(&self, _consumer: &ConsumerId, _offset: Offset) -> Result<()> {
            Ok(())
        }

        fn last_committed(&self, _consumer: &ConsumerId) -> Result<Option<Offset>> {
            Ok(None)
        }

        fn subscribe(&self, _consumer: &ConsumerId, _tx: watch::Sender<Offset>) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn load_save_state_roundtrip() {
        let transport = CloudKitTransport::default();
        transport.events_ingested.store(42, Ordering::Relaxed);
        *transport.change_token.lock().expect("token lock") = Some("abc".to_string());

        let state = transport
            .save_state()
            .expect("save state")
            .expect("some state");
        let restored = CloudKitTransport::default();
        restored.load_state(&state).expect("load state");

        assert_eq!(restored.events_ingested.load(Ordering::Relaxed), 42);
        assert_eq!(
            restored.change_token.lock().expect("token lock").as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn push_notification_appends_records() {
        let transport = CloudKitTransport::default();
        let provider: Arc<dyn StreamProvider> = Arc::new(TestProvider::default());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = runtime.enter();
        let _ = transport.start(provider.clone()).expect("start");

        let payload = serde_json::json!({
            "new_change_token": "tok-1",
            "records": [{
                "record_id": "r1",
                "record_type": "PhysiologicalSample",
                "modified_at": "2026-02-21T00:00:00Z",
                "fields": {
                    "metric": "HRV",
                    "value": 42.0,
                    "unit": "ms",
                    "recorded_at": "2026-02-21T00:00:00Z"
                }
            }]
        });
        transport
            .on_push_notification(serde_json::to_vec(&payload).expect("encode payload"))
            .expect("push applied");

        assert_eq!(transport.events_ingested.load(Ordering::Relaxed), 1);
        assert_eq!(
            transport.change_token.lock().expect("token lock").as_deref(),
            Some("tok-1")
        );
    }

    #[test]
    fn push_uses_fetcher_with_previous_token() {
        struct Fetcher {
            seen_previous: Mutex<Option<String>>,
        }

        impl CloudKitChangeFetcher for Fetcher {
            fn fetch_changes(
                &self,
                _push_payload: &[u8],
                previous_token: Option<&str>,
            ) -> Result<CloudKitPushPayload> {
                *self
                    .seen_previous
                    .lock()
                    .map_err(|_| CoreError::Internal("fetcher lock poisoned".to_string()))? =
                    previous_token.map(|v| v.to_string());
                Ok(CloudKitPushPayload {
                    new_change_token: Some("tok-2".to_string()),
                    records: Vec::new(),
                })
            }
        }

        let fetcher = Arc::new(Fetcher {
            seen_previous: Mutex::new(None),
        });
        let transport = CloudKitTransport::new(fetcher.clone());
        *transport.change_token.lock().expect("token lock") = Some("tok-1".to_string());
        let provider: Arc<dyn StreamProvider> = Arc::new(TestProvider::default());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = runtime.enter();
        let _ = transport.start(provider).expect("start");

        transport
            .on_push_notification(br#"{"records":[]}"#.to_vec())
            .expect("push");

        assert_eq!(
            fetcher
                .seen_previous
                .lock()
                .expect("fetcher lock")
                .as_deref(),
            Some("tok-1")
        );
        assert_eq!(
            transport.change_token.lock().expect("token lock").as_deref(),
            Some("tok-2")
        );
    }

    #[test]
    fn invalid_push_marks_transport_degraded() {
        let transport = CloudKitTransport::default();
        let provider: Arc<dyn StreamProvider> = Arc::new(TestProvider::default());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = runtime.enter();
        let _ = transport.start(provider).expect("start");

        let result = transport.on_push_notification(b"not-json".to_vec());
        assert!(result.is_err());

        match transport.status().state {
            TransportState::Degraded { reason } => {
                assert!(reason.contains("invalid"));
            }
            other => panic!("expected degraded transport, got {other:?}"),
        }
    }
}
