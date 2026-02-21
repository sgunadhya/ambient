use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ambient_core::{
    CoreError, LoadAware, PulseEvent, PulseSignal, RawEvent, RawPayload, Result, SourceId,
    StreamProvider, StreamTransport, SystemLoad, TransportId, TransportState, TransportStatus,
};
use chrono::Utc;

pub trait CalendarProvider: Send + Sync {
    fn initial_events(&self) -> Vec<RawPayload>;
    fn context(&self) -> CalendarContextSample;
}

#[derive(Debug, Clone)]
pub struct CalendarContextSample {
    pub in_meeting: bool,
    pub in_focus_block: bool,
    pub minutes_until_next_event: Option<u32>,
    pub current_event_duration_minutes: Option<u32>,
}

pub struct DefaultCalendarProvider;

impl CalendarProvider for DefaultCalendarProvider {
    fn initial_events(&self) -> Vec<RawPayload> {
        Vec::new()
    }

    fn context(&self) -> CalendarContextSample {
        CalendarContextSample {
            in_meeting: false,
            in_focus_block: false,
            minutes_until_next_event: None,
            current_event_duration_minutes: None,
        }
    }
}

pub struct CalendarTransport {
    provider: Arc<dyn CalendarProvider>,
}

impl CalendarTransport {
    pub fn new(provider: Arc<dyn CalendarProvider>) -> Self {
        Self { provider }
    }
}

impl StreamTransport for CalendarTransport {
    fn transport_id(&self) -> TransportId {
        "calendar".to_string()
    }

    fn start(&self, provider: Arc<dyn StreamProvider>) -> Result<tokio::task::JoinHandle<()>> {
        let provider_ref = Arc::clone(&self.provider);
        thread::Builder::new()
            .name("calendar-transport".to_string())
            .spawn(move || {
                for payload in provider_ref.initial_events() {
                    let _ = provider.append_raw(RawEvent {
                        source: SourceId::new("calendar"),
                        timestamp: Utc::now(),
                        payload,
                    });
                }
            })
            .map_err(|e| CoreError::Internal(format!("failed to spawn calendar transport: {e}")))?;

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

pub struct CalendarContextTransport {
    provider: Arc<dyn CalendarProvider>,
    load: Arc<AtomicU8>,
    tick: Duration,
}

impl CalendarContextTransport {
    pub fn new(provider: Arc<dyn CalendarProvider>) -> Self {
        Self {
            provider,
            load: Arc::new(AtomicU8::new(0)), // Unconstrained defaults to 0
            tick: Duration::from_secs(60),
        }
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }
}

impl StreamTransport for CalendarContextTransport {
    fn transport_id(&self) -> TransportId {
        "calendar_context".to_string()
    }

    fn start(&self, provider: Arc<dyn StreamProvider>) -> Result<tokio::task::JoinHandle<()>> {
        let provider_ref = Arc::clone(&self.provider);
        let load_ref = Arc::clone(&self.load);
        let tick = self.tick;

        thread::Builder::new()
            .name("calendar-context-transport".to_string())
            .spawn(move || loop {
                thread::sleep(tick);
                if load_ref.load(Ordering::Relaxed) == 2 {
                    // Minimal load
                    continue;
                }
                let sample = provider_ref.context();
                let _ = provider.append_pulse(PulseEvent {
                    timestamp: Utc::now(),
                    signal: PulseSignal::CalendarContext {
                        in_meeting: sample.in_meeting,
                        in_focus_block: sample.in_focus_block,
                        minutes_until_next_event: sample.minutes_until_next_event,
                        current_event_duration_minutes: sample.current_event_duration_minutes,
                    },
                });
            })
            .map_err(|e| {
                CoreError::Internal(format!("failed to spawn calendar context transport: {e}"))
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

impl LoadAware for CalendarContextTransport {
    fn on_load_change(&self, load: SystemLoad) {
        let val = match load {
            SystemLoad::Unconstrained => 0,
            SystemLoad::Conservative => 1,
            SystemLoad::Minimal => 2,
        };
        self.load.store(val, Ordering::Relaxed);
    }
}
