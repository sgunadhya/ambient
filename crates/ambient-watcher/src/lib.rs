use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ambient_core::{
    CoreError, HealthMetric, LoadAware, PulseEvent, PulseSampler, PulseSignal, RawEvent,
    RawPayload, Result, SamplerHandle, SourceAdapter, SourceId, SystemLoad, WatchHandle,
};
use chrono::{DateTime, Utc};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

const CONTEXT_WINDOW_SECS: i64 = 60;
const AX_TIMEOUT_MS: u64 = 500;
const DEBOUNCE_WINDOW_MS: u64 = 500;

fn load_to_u8(load: SystemLoad) -> u8 {
    match load {
        SystemLoad::Unconstrained => 0,
        SystemLoad::Conservative => 1,
        SystemLoad::Minimal => 2,
    }
}

fn is_minimal(load: &AtomicU8) -> bool {
    load.load(Ordering::Relaxed) == load_to_u8(SystemLoad::Minimal)
}

fn spawn_named<F>(task: F) -> Result<SamplerHandle>
where
    F: FnOnce() + Send + 'static,
{
    thread::Builder::new()
        .spawn(task)
        .map_err(|e| CoreError::Internal(format!("failed to spawn sampler thread: {e}")))?;
    Ok(SamplerHandle)
}

pub struct ObsidianAdapter {
    vault_path: PathBuf,
}

impl ObsidianAdapter {
    pub fn new(vault_path: impl Into<PathBuf>) -> Self {
        Self {
            vault_path: vault_path.into(),
        }
    }

    fn is_eligible_markdown(path: &Path) -> bool {
        if !path.extension().is_some_and(|ext| ext == "md") {
            return false;
        }

        if path
            .components()
            .any(|part| part.as_os_str().to_string_lossy().starts_with('.'))
        {
            return false;
        }

        !path
            .components()
            .any(|part| part.as_os_str().to_string_lossy() == ".obsidian")
    }

    fn emit_markdown(tx: &mpsc::Sender<RawEvent>, path: PathBuf) {
        let Ok(content) = fs::read_to_string(&path) else {
            return;
        };

        let event = RawEvent {
            source: SourceId::new("obsidian"),
            timestamp: Utc::now(),
            payload: RawPayload::Markdown { content, path },
        };
        let _ = tx.send(event);
    }

    fn should_emit(path: &Path, recently_emitted: &mut HashMap<PathBuf, Instant>) -> bool {
        let now = Instant::now();
        if recently_emitted
            .get(path)
            .is_some_and(|last| now.duration_since(*last) < Duration::from_millis(DEBOUNCE_WINDOW_MS))
        {
            return false;
        }
        recently_emitted.insert(path.to_path_buf(), now);
        true
    }

    fn handle_notify_event(
        event: Event,
        tx: &mpsc::Sender<RawEvent>,
        recently_emitted: &mut HashMap<PathBuf, Instant>,
    ) {
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                for path in event.paths {
                    if !Self::is_eligible_markdown(&path) {
                        continue;
                    }
                    if Self::should_emit(&path, recently_emitted) {
                        Self::emit_markdown(tx, path);
                    }
                }
            }
            EventKind::Any | EventKind::Access(_) | EventKind::Remove(_) | EventKind::Other => {}
        }
    }

    fn scan_markdown_files(root: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with('.'))
                    || path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy() == ".obsidian")
                {
                    continue;
                }
                Self::scan_markdown_files(&path, out);
                continue;
            }
            if Self::is_eligible_markdown(&path) {
                out.push(path);
            }
        }
    }
}

impl SourceAdapter for ObsidianAdapter {
    fn source_id(&self) -> SourceId {
        SourceId::new("obsidian")
    }

    fn watch(&self, tx: mpsc::Sender<RawEvent>) -> Result<WatchHandle> {
        let vault = self.vault_path.clone();

        thread::Builder::new()
            .spawn(move || {
                let (notify_tx, notify_rx) = mpsc::channel();
                let mut watcher: RecommendedWatcher = match RecommendedWatcher::new(
                    move |res| {
                        let _ = notify_tx.send(res);
                    },
                    Config::default(),
                ) {
                    Ok(watcher) => watcher,
                    Err(_) => return,
                };

                if watcher.watch(&vault, RecursiveMode::Recursive).is_err() {
                    return;
                }

                let mut recently_emitted: HashMap<PathBuf, Instant> = HashMap::new();
                let mut initial_files = Vec::new();
                Self::scan_markdown_files(&vault, &mut initial_files);
                for path in initial_files {
                    if Self::should_emit(&path, &mut recently_emitted) {
                        Self::emit_markdown(&tx, path);
                    }
                }
                while let Ok(res) = notify_rx.recv() {
                    let Ok(event) = res else {
                        continue;
                    };
                    Self::handle_notify_event(event, &tx, &mut recently_emitted);
                }
            })
            .map_err(|e| CoreError::Internal(format!("failed to spawn obsidian watcher: {e}")))?;

        Ok(WatchHandle)
    }
}

pub trait HealthKitProvider: Send + Sync {
    fn initial_samples(&self) -> Vec<RawPayload>;
}

pub struct HealthKitAdapter {
    provider: Arc<dyn HealthKitProvider>,
}

impl HealthKitAdapter {
    pub fn new(provider: Arc<dyn HealthKitProvider>) -> Self {
        Self { provider }
    }
}

impl SourceAdapter for HealthKitAdapter {
    fn source_id(&self) -> SourceId {
        SourceId::new("healthkit")
    }

    fn watch(&self, tx: mpsc::Sender<RawEvent>) -> Result<WatchHandle> {
        let provider = Arc::clone(&self.provider);
        thread::Builder::new()
            .name("healthkit-adapter".to_string())
            .spawn(move || {
                for payload in provider.initial_samples() {
                    let _ = tx.send(RawEvent {
                        source: SourceId::new("healthkit"),
                        timestamp: Utc::now(),
                        payload,
                    });
                }
            })
            .map_err(|e| CoreError::Internal(format!("failed to spawn healthkit adapter: {e}")))?;
        Ok(WatchHandle)
    }
}

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

pub struct CalendarAdapter {
    provider: Arc<dyn CalendarProvider>,
}

impl CalendarAdapter {
    pub fn new(provider: Arc<dyn CalendarProvider>) -> Self {
        Self { provider }
    }
}

impl SourceAdapter for CalendarAdapter {
    fn source_id(&self) -> SourceId {
        SourceId::new("calendar")
    }

    fn watch(&self, tx: mpsc::Sender<RawEvent>) -> Result<WatchHandle> {
        let provider = Arc::clone(&self.provider);
        thread::Builder::new()
            .name("calendar-adapter".to_string())
            .spawn(move || {
                for payload in provider.initial_events() {
                    let _ = tx.send(RawEvent {
                        source: SourceId::new("calendar"),
                        timestamp: Utc::now(),
                        payload,
                    });
                }
            })
            .map_err(|e| CoreError::Internal(format!("failed to spawn calendar adapter: {e}")))?;
        Ok(WatchHandle)
    }
}

pub struct CalendarContextSampler {
    provider: Arc<dyn CalendarProvider>,
    load: Arc<AtomicU8>,
    tick: Duration,
}

impl CalendarContextSampler {
    pub fn new(provider: Arc<dyn CalendarProvider>) -> Self {
        Self {
            provider,
            load: Arc::new(AtomicU8::new(load_to_u8(SystemLoad::Unconstrained))),
            tick: Duration::from_secs(60),
        }
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }
}

impl PulseSampler for CalendarContextSampler {
    fn start(&self, tx: mpsc::Sender<PulseEvent>) -> Result<SamplerHandle> {
        let provider = Arc::clone(&self.provider);
        let load = Arc::clone(&self.load);
        let tick = self.tick;
        spawn_named(move || loop {
            thread::sleep(tick);
            if is_minimal(&load) {
                continue;
            }
            let sample = provider.context();
            let _ = tx.send(PulseEvent {
                timestamp: Utc::now(),
                signal: PulseSignal::CalendarContext {
                    in_meeting: sample.in_meeting,
                    in_focus_block: sample.in_focus_block,
                    minutes_until_next_event: sample.minutes_until_next_event,
                    current_event_duration_minutes: sample.current_event_duration_minutes,
                },
            });
        })
    }
}

impl LoadAware for CalendarContextSampler {
    fn on_load_change(&self, load: SystemLoad) {
        self.load.store(load_to_u8(load), Ordering::Relaxed);
    }
}

pub trait ActiveAppProvider: Send + Sync {
    fn active_bundle_id(&self) -> Option<String>;
    fn active_window_title(&self) -> Option<String>;
}

pub trait AudioInputSource: Send + Sync {
    fn subscribe(&self) -> Result<mpsc::Receiver<bool>>;
}

pub trait LoadPolicyProvider: Send + Sync {
    fn sample(&self) -> LoadSample;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadSample {
    pub plugged_in: bool,
    pub battery_percent: f32,
    pub cpu_30s_avg: f32,
    pub user_active_last_60s: bool,
}

pub struct ContextSwitchSampler {
    provider: Arc<dyn ActiveAppProvider>,
    load: Arc<AtomicU8>,
    tick: Duration,
}

impl ContextSwitchSampler {
    pub fn new(provider: Arc<dyn ActiveAppProvider>) -> Self {
        Self {
            provider,
            load: Arc::new(AtomicU8::new(load_to_u8(SystemLoad::Unconstrained))),
            tick: Duration::from_secs(5),
        }
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }
}

impl PulseSampler for ContextSwitchSampler {
    fn start(&self, tx: mpsc::Sender<PulseEvent>) -> Result<SamplerHandle> {
        let provider = Arc::clone(&self.provider);
        let load = Arc::clone(&self.load);
        let tick = self.tick;

        spawn_named(move || {
            let mut previous_bundle: Option<String> = None;
            let mut switches: VecDeque<DateTime<Utc>> = VecDeque::new();

            loop {
                thread::sleep(tick);
                if is_minimal(&load) {
                    continue;
                }

                let now = Utc::now();
                if let Some(bundle_id) = provider.active_bundle_id() {
                    if previous_bundle.as_ref().is_some_and(|prev| prev != &bundle_id) {
                        switches.push_back(now);
                    }
                    previous_bundle = Some(bundle_id);
                }

                while switches
                    .front()
                    .is_some_and(|ts| (*ts + chrono::Duration::seconds(CONTEXT_WINDOW_SECS)) < now)
                {
                    let _ = switches.pop_front();
                }

                let _ = tx.send(PulseEvent {
                    timestamp: now,
                    signal: PulseSignal::ContextSwitchRate {
                        switches_per_minute: switches.len() as f32,
                    },
                });
            }
        })
    }
}

impl LoadAware for ContextSwitchSampler {
    fn on_load_change(&self, load: SystemLoad) {
        self.load.store(load_to_u8(load), Ordering::Relaxed);
    }
}

pub struct ActiveAppSampler {
    provider: Arc<dyn ActiveAppProvider>,
    load: Arc<AtomicU8>,
    tick: Duration,
    allowlist: Arc<Vec<String>>,
}

impl ActiveAppSampler {
    pub fn new(provider: Arc<dyn ActiveAppProvider>) -> Self {
        Self {
            provider,
            load: Arc::new(AtomicU8::new(load_to_u8(SystemLoad::Unconstrained))),
            tick: Duration::from_secs(5),
            allowlist: Arc::new(load_window_title_allowlist()),
        }
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }
}

impl PulseSampler for ActiveAppSampler {
    fn start(&self, tx: mpsc::Sender<PulseEvent>) -> Result<SamplerHandle> {
        let provider = Arc::clone(&self.provider);
        let load = Arc::clone(&self.load);
        let tick = self.tick;
        let allowlist = Arc::clone(&self.allowlist);

        spawn_named(move || loop {
            thread::sleep(tick);

            let now = Utc::now();
            let Some(bundle_id) = provider.active_bundle_id() else {
                continue;
            };

            let window_title = if is_minimal(&load) {
                None
            } else if allowlist.iter().any(|allowed| allowed == &bundle_id) {
                read_window_title_with_timeout(Arc::clone(&provider), AX_TIMEOUT_MS)
            } else {
                None
            };

            let _ = tx.send(PulseEvent {
                timestamp: now,
                signal: PulseSignal::ActiveApp {
                    bundle_id,
                    window_title,
                },
            });
        })
    }
}

impl LoadAware for ActiveAppSampler {
    fn on_load_change(&self, load: SystemLoad) {
        self.load.store(load_to_u8(load), Ordering::Relaxed);
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

    rx.recv_timeout(Duration::from_millis(timeout_ms)).ok().flatten()
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

pub struct AudioInputSampler {
    source: Arc<dyn AudioInputSource>,
}

impl AudioInputSampler {
    pub fn new(source: Arc<dyn AudioInputSource>) -> Self {
        Self { source }
    }
}

impl PulseSampler for AudioInputSampler {
    fn start(&self, tx: mpsc::Sender<PulseEvent>) -> Result<SamplerHandle> {
        let rx = self.source.subscribe()?;

        spawn_named(move || {
            while let Ok(active) = rx.recv() {
                let _ = tx.send(PulseEvent {
                    timestamp: Utc::now(),
                    signal: PulseSignal::AudioInputActive { active },
                });
            }
        })
    }
}

impl LoadAware for AudioInputSampler {
    fn on_load_change(&self, _load: SystemLoad) {
        // Continues even on minimal.
    }
}

#[derive(Default)]
pub struct LoadBroadcaster {
    components: Mutex<Vec<Arc<dyn LoadAware>>>,
}

impl LoadBroadcaster {
    pub fn register(&self, component: Arc<dyn LoadAware>) {
        if let Ok(mut guard) = self.components.lock() {
            guard.push(component);
        }
    }

    pub fn broadcast(&self, load: SystemLoad) {
        if let Ok(guard) = self.components.lock() {
            for component in guard.iter() {
                component.on_load_change(load);
            }
        }
    }
}

pub struct SystemLoadSampler {
    provider: Arc<dyn LoadPolicyProvider>,
    broadcaster: Arc<LoadBroadcaster>,
    tick: Duration,
    started: AtomicBool,
}

impl SystemLoadSampler {
    pub fn new(provider: Arc<dyn LoadPolicyProvider>, broadcaster: Arc<LoadBroadcaster>) -> Self {
        Self {
            provider,
            broadcaster,
            tick: Duration::from_secs(30),
            started: AtomicBool::new(false),
        }
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    pub fn start(&self) -> Result<SamplerHandle> {
        if self.started.swap(true, Ordering::Relaxed) {
            return Ok(SamplerHandle);
        }

        let provider = Arc::clone(&self.provider);
        let broadcaster = Arc::clone(&self.broadcaster);
        let tick = self.tick;

        spawn_named(move || {
            let mut previous: Option<SystemLoad> = None;
            loop {
                thread::sleep(tick);
                let load = classify_system_load(provider.sample());
                if previous != Some(load) {
                    broadcaster.broadcast(load);
                    previous = Some(load);
                }
            }
        })
    }
}

pub fn classify_system_load(sample: LoadSample) -> SystemLoad {
    if sample.battery_percent < 20.0 {
        return SystemLoad::Minimal;
    }

    if !sample.plugged_in || sample.cpu_30s_avg > 60.0 || sample.user_active_last_60s {
        return SystemLoad::Conservative;
    }

    if sample.plugged_in && sample.cpu_30s_avg < 40.0 {
        return SystemLoad::Unconstrained;
    }

    SystemLoad::Conservative
}

pub struct DefaultLoadPolicyProvider;

impl LoadPolicyProvider for DefaultLoadPolicyProvider {
    fn sample(&self) -> LoadSample {
        LoadSample {
            plugged_in: true,
            battery_percent: 100.0,
            cpu_30s_avg: 0.0,
            user_active_last_60s: false,
        }
    }
}

pub struct DefaultHealthKitProvider;

impl HealthKitProvider for DefaultHealthKitProvider {
    fn initial_samples(&self) -> Vec<RawPayload> {
        let now = Utc::now();
        vec![
            RawPayload::HealthKitSample {
                metric: HealthMetric::HRV,
                value: 0.0,
                unit: "ms".to_string(),
                recorded_at: now,
            },
            RawPayload::HealthKitSample {
                metric: HealthMetric::SleepQuality,
                value: 0.0,
                unit: "score".to_string(),
                recorded_at: now,
            },
        ]
    }
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

pub struct DefaultActiveAppProvider;

impl ActiveAppProvider for DefaultActiveAppProvider {
    fn active_bundle_id(&self) -> Option<String> {
        macos_bridge::frontmost_bundle_id()
    }

    fn active_window_title(&self) -> Option<String> {
        macos_bridge::frontmost_window_title()
    }
}

mod macos_bridge {
    #[cfg(target_os = "macos")]
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}

    #[cfg(target_os = "macos")]
    pub fn frontmost_bundle_id() -> Option<String> {
        use std::ffi::CStr;
        use std::os::raw::c_char;

        use objc2::rc::autoreleasepool;
        use objc2::runtime::AnyObject;
        use objc2::{class, msg_send};

        autoreleasepool(|_| {
            // NSWorkspace.sharedWorkspace.frontmostApplication.bundleIdentifier
            let workspace: *mut AnyObject = unsafe { msg_send![class!(NSWorkspace), sharedWorkspace] };
            if workspace.is_null() {
                return None;
            }
            let app: *mut AnyObject = unsafe { msg_send![workspace, frontmostApplication] };
            if app.is_null() {
                return None;
            }
            let identifier: *mut AnyObject = unsafe { msg_send![app, bundleIdentifier] };
            if identifier.is_null() {
                return None;
            }
            let utf8: *const c_char = unsafe { msg_send![identifier, UTF8String] };
            if utf8.is_null() {
                return None;
            }
            Some(unsafe { CStr::from_ptr(utf8) }.to_string_lossy().to_string())
        })
    }

    #[cfg(not(target_os = "macos"))]
    pub fn frontmost_bundle_id() -> Option<String> {
        None
    }

    #[cfg(target_os = "macos")]
    pub fn frontmost_window_title() -> Option<String> {
        use objc2::rc::autoreleasepool;

        // AXUIElement integration will be used for this path; currently gated by allowlist
        // and timeout at caller side. Keep this call inside autoreleasepool.
        autoreleasepool(|_| None)
    }

    #[cfg(not(target_os = "macos"))]
    pub fn frontmost_window_title() -> Option<String> {
        None
    }
}

fn load_window_title_allowlist() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return Vec::new();
    }

    let path = PathBuf::from(home).join(".ambient").join("config.toml");
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };

    parse_allowlist_from_toml(&raw)
}

fn parse_allowlist_from_toml(raw: &str) -> Vec<String> {
    let mut in_section = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[") {
            in_section = trimmed == "[window_title_allowlist]";
            continue;
        }
        if !in_section {
            continue;
        }
        if !trimmed.starts_with("apps") {
            continue;
        }

        let Some((_, rhs)) = trimmed.split_once('=') else {
            continue;
        };
        let rhs = rhs.trim();
        if !(rhs.starts_with('[') && rhs.ends_with(']')) {
            continue;
        }

        let inner = &rhs[1..rhs.len() - 1];
        return inner
            .split(',')
            .map(str::trim)
            .filter_map(|v| {
                if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
                    Some(v[1..v.len() - 1].to_string())
                } else {
                    None
                }
            })
            .collect();
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::RecvTimeoutError;

    use super::*;

    struct StaticAppProvider {
        bundle: String,
        title: Option<String>,
    }

    impl ActiveAppProvider for StaticAppProvider {
        fn active_bundle_id(&self) -> Option<String> {
            Some(self.bundle.clone())
        }

        fn active_window_title(&self) -> Option<String> {
            self.title.clone()
        }
    }

    struct StaticLoadProvider {
        sample: LoadSample,
    }

    impl LoadPolicyProvider for StaticLoadProvider {
        fn sample(&self) -> LoadSample {
            self.sample
        }
    }

    fn recv_no_event(rx: &mpsc::Receiver<PulseEvent>) {
        let got = rx.recv_timeout(Duration::from_millis(120));
        assert!(matches!(got, Err(RecvTimeoutError::Timeout)));
    }

    #[test]
    fn context_switch_sampler_pauses_on_minimal() {
        let sampler = ContextSwitchSampler::new(Arc::new(StaticAppProvider {
            bundle: "com.test.app".to_string(),
            title: None,
        }))
        .with_tick(Duration::from_millis(20));
        sampler.on_load_change(SystemLoad::Minimal);

        let (tx, rx) = mpsc::channel();
        sampler.start(tx).expect("sampler starts");
        recv_no_event(&rx);
    }

    #[test]
    fn active_app_sampler_emits_bundle_only_on_minimal() {
        let sampler = ActiveAppSampler::new(Arc::new(StaticAppProvider {
            bundle: "com.test.app".to_string(),
            title: Some("Secret Window".to_string()),
        }))
        .with_tick(Duration::from_millis(20));
        sampler.on_load_change(SystemLoad::Minimal);

        let (tx, rx) = mpsc::channel();
        sampler.start(tx).expect("sampler starts");

        let event = rx.recv_timeout(Duration::from_millis(300)).expect("event exists");
        match event.signal {
            PulseSignal::ActiveApp {
                bundle_id,
                window_title,
            } => {
                assert_eq!(bundle_id, "com.test.app");
                assert!(window_title.is_none());
            }
            PulseSignal::ContextSwitchRate { .. }
            | PulseSignal::AudioInputActive { .. }
            | PulseSignal::TimeContext { .. }
            | PulseSignal::CalendarContext { .. }
            | PulseSignal::EnergyLevel { .. }
            | PulseSignal::MoodLevel { .. }
            | PulseSignal::HRVScore { .. }
            | PulseSignal::SleepQuality { .. } => panic!("unexpected variant"),
        }
    }

    #[test]
    fn audio_sampler_continues_on_minimal() {
        let source = Arc::new(WatchAudioInputSource::new(false));
        let sampler = AudioInputSampler::new(source.clone());
        sampler.on_load_change(SystemLoad::Minimal);

        let (tx, rx) = mpsc::channel();
        sampler.start(tx).expect("sampler starts");
        source.set_active(true);

        let event = rx.recv_timeout(Duration::from_millis(300)).expect("event exists");
        match event.signal {
            PulseSignal::AudioInputActive { active } => assert!(active),
            PulseSignal::ContextSwitchRate { .. }
            | PulseSignal::ActiveApp { .. }
            | PulseSignal::TimeContext { .. }
            | PulseSignal::CalendarContext { .. }
            | PulseSignal::EnergyLevel { .. }
            | PulseSignal::MoodLevel { .. }
            | PulseSignal::HRVScore { .. }
            | PulseSignal::SleepQuality { .. } => panic!("unexpected variant"),
        }
    }

    #[test]
    fn classify_system_load_rules() {
        assert_eq!(
            classify_system_load(LoadSample {
                plugged_in: false,
                battery_percent: 19.0,
                cpu_30s_avg: 10.0,
                user_active_last_60s: false,
            }),
            SystemLoad::Minimal
        );
        assert_eq!(
            classify_system_load(LoadSample {
                plugged_in: false,
                battery_percent: 90.0,
                cpu_30s_avg: 10.0,
                user_active_last_60s: false,
            }),
            SystemLoad::Conservative
        );
        assert_eq!(
            classify_system_load(LoadSample {
                plugged_in: true,
                battery_percent: 90.0,
                cpu_30s_avg: 20.0,
                user_active_last_60s: false,
            }),
            SystemLoad::Unconstrained
        );
    }

    #[test]
    fn load_sampler_broadcasts_changes() {
        let broadcaster = Arc::new(LoadBroadcaster::default());
        let aware = Arc::new(ambient_core::mocks::MockLoadAware::default());
        broadcaster.register(aware.clone());

        let sampler = SystemLoadSampler::new(
            Arc::new(StaticLoadProvider {
                sample: LoadSample {
                    plugged_in: false,
                    battery_percent: 75.0,
                    cpu_30s_avg: 20.0,
                    user_active_last_60s: false,
                },
            }),
            broadcaster,
        )
        .with_tick(Duration::from_millis(20));

        sampler.start().expect("sampler starts");
        thread::sleep(Duration::from_millis(100));

        let events = aware.calls.lock().expect("calls lock");
        assert!(events.iter().any(|v| v.load == SystemLoad::Conservative));
    }

    #[test]
    fn allowlist_parser_handles_section_list() {
        let parsed = parse_allowlist_from_toml(
            "[window_title_allowlist]\napps = [\"com.foo\", \"com.bar\"]",
        );
        assert_eq!(parsed, vec!["com.foo".to_string(), "com.bar".to_string()]);
    }
}
