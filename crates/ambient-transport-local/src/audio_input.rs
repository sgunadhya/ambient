use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;

use ambient_core::{
    CoreError, LoadAware, PulseEvent, PulseSignal, Result, StreamProvider, StreamTransport,
    SystemLoad, TransportId, TransportState, TransportStatus,
};
use chrono::Utc;

pub trait AudioInputSource: Send + Sync {
    fn subscribe(&self) -> Result<mpsc::Receiver<bool>>;
}

pub struct WatchAudioInputSource {
    tx: mpsc::Sender<bool>,
    rx: Mutex<Option<mpsc::Receiver<bool>>>,
}

impl WatchAudioInputSource {
    pub fn new(_initial_active: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    pub fn set_active(&self, active: bool) {
        let _ = self.tx.send(active);
    }
}

impl AudioInputSource for WatchAudioInputSource {
    fn subscribe(&self) -> Result<mpsc::Receiver<bool>> {
        self.rx
            .lock()
            .map_err(|_| CoreError::Internal("audio source lock poisoned".to_string()))?
            .take()
            .ok_or_else(|| CoreError::InvalidInput("audio source already subscribed".to_string()))
    }
}

pub struct AudioInputTransport {
    source: Arc<dyn AudioInputSource>,
}

impl AudioInputTransport {
    pub fn new(source: Arc<dyn AudioInputSource>) -> Self {
        Self { source }
    }
}

impl StreamTransport for AudioInputTransport {
    fn transport_id(&self) -> TransportId {
        "audio_input".to_string()
    }

    fn start(&self, provider: Arc<dyn StreamProvider>) -> Result<tokio::task::JoinHandle<()>> {
        let rx = self.source.subscribe()?;

        thread::Builder::new()
            .name("audio-input-transport".to_string())
            .spawn(move || {
                while let Ok(active) = rx.recv() {
                    let _ = provider.append_pulse(PulseEvent {
                        timestamp: Utc::now(),
                        signal: PulseSignal::AudioInputActive { active },
                    });
                }
            })
            .map_err(|e| {
                CoreError::Internal(format!("failed to spawn audio input transport: {e}"))
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

impl LoadAware for AudioInputTransport {
    fn on_load_change(&self, _load: SystemLoad) {
        // Audio continues even on minimal
    }
}
