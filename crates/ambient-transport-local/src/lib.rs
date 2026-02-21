use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use ambient_core::{
    CoreError, PulseEvent, PulseSampler, Result, SamplerHandle, SourceAdapter, StreamProvider,
    StreamTransport, TransportId, TransportState, TransportStatus, WatchHandle,
};
use ambient_spotlight::SpotlightAdapter;
use ambient_watcher::{
    ActiveAppSampler, AudioInputSampler, ContextSwitchSampler, DefaultActiveAppProvider,
    ObsidianAdapter, WatchAudioInputSource,
};
use chrono::Utc;

pub struct LocalTransport {
    obsidian_vault: String,
    enable_spotlight: bool,
    enable_context_switch: bool,
    enable_active_app_titles: bool,
    enable_audio_input: bool,
    running: Arc<AtomicBool>,
    events_ingested: Arc<AtomicU64>,
    last_event_at: Arc<Mutex<Option<chrono::DateTime<Utc>>>>,
    worker_threads: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
}

impl LocalTransport {
    pub fn new(
        obsidian_vault: String,
        enable_spotlight: bool,
        enable_context_switch: bool,
        enable_active_app_titles: bool,
        enable_audio_input: bool,
    ) -> Self {
        Self {
            obsidian_vault,
            enable_spotlight,
            enable_context_switch,
            enable_active_app_titles,
            enable_audio_input,
            running: Arc::new(AtomicBool::new(false)),
            events_ingested: Arc::new(AtomicU64::new(0)),
            last_event_at: Arc::new(Mutex::new(None)),
            worker_threads: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn spawn_adapter_reader(
        &self,
        provider: Arc<dyn StreamProvider>,
        rx: mpsc::Receiver<ambient_core::RawEvent>,
        _handles: Vec<WatchHandle>,
    ) {
        let running = Arc::clone(&self.running);
        let events_ingested = Arc::clone(&self.events_ingested);
        let last_event_at = Arc::clone(&self.last_event_at);
        let handle = thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                let Ok(raw) = rx.recv_timeout(std::time::Duration::from_millis(500)) else {
                    continue;
                };
                if provider.append_raw(raw).is_ok() {
                    events_ingested.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut guard) = last_event_at.lock() {
                        *guard = Some(Utc::now());
                    }
                }
            }
        });
        if let Ok(mut workers) = self.worker_threads.lock() {
            workers.push(handle);
        }
    }

    fn spawn_sampler_reader(
        &self,
        provider: Arc<dyn StreamProvider>,
        rx: mpsc::Receiver<PulseEvent>,
        _handles: Vec<SamplerHandle>,
    ) {
        let running = Arc::clone(&self.running);
        let events_ingested = Arc::clone(&self.events_ingested);
        let last_event_at = Arc::clone(&self.last_event_at);
        let handle = thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                let Ok(pulse) = rx.recv_timeout(std::time::Duration::from_millis(500)) else {
                    continue;
                };
                if provider.append_pulse(pulse).is_ok() {
                    events_ingested.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut guard) = last_event_at.lock() {
                        *guard = Some(Utc::now());
                    }
                }
            }
        });
        if let Ok(mut workers) = self.worker_threads.lock() {
            workers.push(handle);
        }
    }
}

impl StreamTransport for LocalTransport {
    fn transport_id(&self) -> TransportId {
        "local".to_string()
    }

    fn start(&self, provider: Arc<dyn StreamProvider>) -> Result<tokio::task::JoinHandle<()>> {
        if self.running.swap(true, Ordering::Relaxed) {
            let handle = tokio::runtime::Handle::try_current()
                .map_err(|e| CoreError::Internal(format!("tokio runtime unavailable: {e}")))?
                .spawn(async {});
            return Ok(handle);
        }

        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();

        let obsidian = ObsidianAdapter::new(self.obsidian_vault.clone());
        handles.push(obsidian.watch(tx.clone())?);

        if self.enable_spotlight {
            let spotlight = SpotlightAdapter;
            handles.push(spotlight.watch(tx)?);
        }

        self.spawn_adapter_reader(provider.clone(), rx, handles);

        let (pulse_tx, pulse_rx) = mpsc::channel();
        let mut sampler_handles = Vec::new();
        let active_provider = Arc::new(DefaultActiveAppProvider);

        if self.enable_context_switch {
            sampler_handles.push(ContextSwitchSampler::new(active_provider.clone()).start(pulse_tx.clone())?);
        }
        if self.enable_active_app_titles {
            sampler_handles.push(ActiveAppSampler::new(active_provider).start(pulse_tx.clone())?);
        }
        if self.enable_audio_input {
            sampler_handles.push(
                AudioInputSampler::new(Arc::new(WatchAudioInputSource::new(false))).start(pulse_tx)?,
            );
        }
        self.spawn_sampler_reader(provider, pulse_rx, sampler_handles);

        let handle = tokio::runtime::Handle::try_current()
            .map_err(|e| CoreError::Internal(format!("tokio runtime unavailable: {e}")))?
            .spawn(async {});
        Ok(handle)
    }

    fn stop(&self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        if let Ok(mut workers) = self.worker_threads.lock() {
            for handle in workers.drain(..) {
                let _ = handle.join();
            }
        }
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
            peers: Vec::new(),
            last_event_at: self.last_event_at.lock().ok().and_then(|g| *g),
            events_ingested: self.events_ingested.load(Ordering::Relaxed),
        }
    }
}
