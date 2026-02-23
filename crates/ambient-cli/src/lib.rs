use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ambient_core::{
    CapabilityGate, CapabilityStatus, ConsumerId, EventLogEntry, FeedbackEvent, FeedbackSignal,
    GatedCapability, KnowledgeStore, KnowledgeUnit, LoadAware, Offset, PulseEvent, PulseSignal,
    QueryEngine, QueryRequest, QueryResult, Result, SourceId, StreamProvider, StreamTransport,
    TransportStatus,
};
use ambient_onboard::{HealthCheck, SetupWizard};
use ambient_stream::SqliteStreamProvider;
use ambient_transport_bonjour::BonjourTransport;
use ambient_transport_cloudkit::{CloudKitTransport, JsonPayloadFetcher};
use ambient_transport_google_health::GoogleHealthTransport;
use ambient_transport_local::{
    ActiveAppTransport, AudioInputTransport, CalendarContextTransport, CalendarTransport,
    ContextSwitchTransport, DefaultActiveAppProvider, DefaultCalendarProvider,
    DefaultHealthKitProvider, HealthKitTransport, LoadBroadcaster, ObsidianTransport,
    WatchAudioInputSource,
};
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub struct TransportRegistry {
    transports: Vec<Arc<dyn StreamTransport>>,
    handles: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Default for TransportRegistry {
    fn default() -> Self {
        Self {
            transports: Vec::new(),
            handles: Mutex::new(Vec::new()),
        }
    }
}

impl TransportRegistry {
    pub fn from_config(config: &AmbientConfig, broadcaster: Arc<LoadBroadcaster>) -> Self {
        let mut transports: Vec<Arc<dyn StreamTransport>> = vec![Arc::new(ObsidianTransport::new(
            config.obsidian_vault.clone(),
        ))];
        if config.healthkit {
            transports.push(Arc::new(HealthKitTransport::new(Arc::new(
                DefaultHealthKitProvider,
            ))));
        }
        if config.calendar {
            transports.push(Arc::new(CalendarTransport::new(Arc::new(
                DefaultCalendarProvider,
            ))));
            let calendar_context = Arc::new(CalendarContextTransport::new(Arc::new(
                DefaultCalendarProvider,
            )));
            broadcaster.register(calendar_context.clone() as Arc<dyn LoadAware>);
            transports.push(calendar_context);
        }
        if config.context_switches {
            let context_switch = Arc::new(ContextSwitchTransport::new(Arc::new(
                DefaultActiveAppProvider,
            )));
            broadcaster.register(context_switch.clone() as Arc<dyn LoadAware>);
            transports.push(context_switch);
        }
        if config.active_app_titles {
            let active_app = Arc::new(ActiveAppTransport::new(Arc::new(DefaultActiveAppProvider)));
            broadcaster.register(active_app.clone() as Arc<dyn LoadAware>);
            transports.push(active_app);
        }
        if config.audio_input {
            let audio_input = Arc::new(AudioInputTransport::new(Arc::new(
                WatchAudioInputSource::new(false),
            )));
            broadcaster.register(audio_input.clone() as Arc<dyn LoadAware>);
            transports.push(audio_input);
        }
        if config.bonjour {
            transports.push(Arc::new(BonjourTransport::default()));
        }
        if config.cloudkit {
            if config.cloudkit_native_bridge {
                transports.push(Arc::new(CloudKitTransport::with_native_bridge(
                    config.cloudkit_container.clone(),
                    config.cloudkit_zone_name.clone(),
                    config.cloudkit_native_bridge_cmd.clone(),
                )));
            } else {
                transports.push(Arc::new(CloudKitTransport::with_config(
                    config.cloudkit_container.clone(),
                    config.cloudkit_zone_name.clone(),
                    Arc::new(JsonPayloadFetcher),
                )));
            }
        }
        if config.google_health {
            transports.push(Arc::new(GoogleHealthTransport::default()));
        }
        Self {
            transports,
            handles: Mutex::new(Vec::new()),
        }
    }

    #[cfg(test)]
    fn from_transports(transports: Vec<Arc<dyn StreamTransport>>) -> Self {
        Self {
            transports,
            handles: Mutex::new(Vec::new()),
        }
    }

    pub fn start_all(&self, provider: Arc<dyn StreamProvider>) -> Result<()> {
        let mut handles = self.handles.lock().map_err(|_| {
            ambient_core::CoreError::Internal("transport handles lock poisoned".to_string())
        })?;
        handles.clear();
        for transport in &self.transports {
            let path = transport_state_path(&transport.transport_id())?;
            if path.exists() {
                if let Ok(bytes) = fs::read(&path) {
                    let _ = transport.load_state(&bytes);
                }
            }
            handles.push(transport.start(provider.clone())?);
        }
        Ok(())
    }

    pub fn stop_all(&self) -> Result<()> {
        for transport in &self.transports {
            if let Some(bytes) = transport.save_state()? {
                let path = transport_state_path(&transport.transport_id())?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|e| {
                        ambient_core::CoreError::Internal(format!(
                            "failed creating transport state dir: {e}"
                        ))
                    })?;
                }
                fs::write(&path, bytes).map_err(|e| {
                    ambient_core::CoreError::Internal(format!(
                        "failed writing transport state {}: {e}",
                        path.display()
                    ))
                })?;
            }
            transport.stop()?;
        }
        let mut handles = self.handles.lock().map_err(|_| {
            ambient_core::CoreError::Internal("transport handles lock poisoned".to_string())
        })?;
        for handle in handles.drain(..) {
            handle.abort();
        }
        Ok(())
    }

    pub fn status_all(&self) -> Vec<TransportStatus> {
        self.transports.iter().map(|t| t.status()).collect()
    }

    pub fn on_push_notification(&self, id: &str, payload: Vec<u8>) -> Result<bool> {
        if let Some(transport) = self
            .transports
            .iter()
            .find(|t| t.transport_id().as_str() == id)
        {
            transport.on_push_notification(payload)?;
            return Ok(true);
        }
        Ok(false)
    }
}

pub fn open_default_stream_provider() -> Result<Arc<dyn StreamProvider>> {
    let home = std::env::var("HOME")
        .map_err(|e| ambient_core::CoreError::Internal(format!("failed to read HOME: {e}")))?;
    let path = PathBuf::from(home).join(".ambient").join("stream.db");
    let provider = SqliteStreamProvider::open(&path)?;
    Ok(Arc::new(provider))
}

fn transport_state_path(transport_id: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|e| ambient_core::CoreError::Internal(format!("failed to read HOME: {e}")))?;
    Ok(PathBuf::from(home)
        .join(".ambient")
        .join("transport_state")
        .join(transport_id))
}

pub struct PulseConsumer {
    provider: Arc<dyn StreamProvider>,
    store: Arc<dyn KnowledgeStore>,
    consumer_id: ConsumerId,
    offset: Mutex<Offset>,
    recorder: Option<Arc<ambient_patterns::ImplicitFeedbackRecorder>>,
}

impl PulseConsumer {
    pub fn new(
        provider: Arc<dyn StreamProvider>,
        store: Arc<dyn KnowledgeStore>,
        consumer_id: ConsumerId,
        recorder: Option<Arc<ambient_patterns::ImplicitFeedbackRecorder>>,
    ) -> Result<Self> {
        let offset = provider.last_committed(&consumer_id)?.unwrap_or(0);
        Ok(Self {
            provider,
            store,
            consumer_id,
            offset: Mutex::new(offset),
            recorder,
        })
    }

    pub fn poll_once(&self, limit: usize) -> Result<usize> {
        let mut offset = self.offset.lock().map_err(|_| {
            ambient_core::CoreError::Internal("pulse consumer offset lock poisoned".to_string())
        })?;
        let batch = self.provider.read(&self.consumer_id, *offset, limit)?;
        let mut written = 0usize;

        for (next_offset, entry) in batch {
            if let EventLogEntry::Pulse(event) = entry {
                if self.store.record_pulse(event.clone()).is_ok() {
                    written += 1;
                    if let Some(ref recorder) = self.recorder {
                        match event.signal {
                            PulseSignal::ContextSwitchRate {
                                switches_per_minute,
                            } => {
                                recorder.on_context_switch_spike(switches_per_minute);
                            }
                            PulseSignal::ActiveApp {
                                window_title: Some(title),
                                ..
                            } => {
                                recorder.on_file_opened(&title);
                            }
                            _ => {}
                        }
                    }
                }
            }
            self.provider.commit(&self.consumer_id, next_offset)?;
            *offset = next_offset;
        }

        Ok(written)
    }
}

#[derive(Debug, Clone)]
pub struct AmbientConfig {
    pub obsidian_vault: String,
    pub spotlight: bool,
    pub apple_notes: bool,
    pub healthkit: bool,
    pub calendar: bool,
    pub self_reports: bool,
    pub bonjour: bool,
    pub cloudkit: bool,
    pub google_health: bool,
    pub context_switches: bool,
    pub active_app_titles: bool,
    pub audio_input: bool,
    pub window_title_allowlist: Vec<String>,
    pub calendar_focus_block_patterns: Vec<String>,
    pub checkin_reminder_time: String,
    pub semantic_weight: f32,
    pub feedback_weight: f32,
    pub cloudkit_container: String,
    pub cloudkit_zone_name: String,
    pub cloudkit_native_bridge: bool,
    pub cloudkit_native_bridge_cmd: Option<String>,
    pub http_port: u16,
    pub auth_token: Option<String>,
    pub reasoning_type: String,
    pub reasoning_url: String,
    pub reasoning_api_key: Option<String>,
    pub embedding_model: String,
    pub completion_model: String,

    // Noticer Pipeline Config
    pub noticer_trigger: String,
    pub noticer_selector: String,
    pub noticer_clusterer: String,
    pub noticer_synthesizer: String,
    pub noticer_publisher: String,

    // Rules Pipeline Config
    pub rules_matcher: String,
    pub rules_scorer: String,
    pub rules_threshold: String,
    pub rules_executor: String,
}

impl Default for AmbientConfig {
    fn default() -> Self {
        Self {
            obsidian_vault: "~/Documents/Obsidian".to_string(),
            spotlight: true,
            apple_notes: false,
            healthkit: false,
            calendar: false,
            self_reports: true,
            bonjour: true,
            cloudkit: false,
            google_health: false,
            context_switches: true,
            active_app_titles: false,
            audio_input: true,
            window_title_allowlist: vec![
                "com.apple.Xcode".to_string(),
                "md.obsidian".to_string(),
                "com.todesktop.230313mzl4w4u92".to_string(),
            ],
            calendar_focus_block_patterns: vec![
                "Deep Work".to_string(),
                "Focus".to_string(),
                "No Meetings".to_string(),
                "Blocked".to_string(),
            ],
            checkin_reminder_time: String::new(),
            semantic_weight: 0.7,
            feedback_weight: 0.3,
            cloudkit_container: "iCloud.dev.ambient.private".to_string(),
            cloudkit_zone_name: "AmbientZone".to_string(),
            cloudkit_native_bridge: false,
            cloudkit_native_bridge_cmd: None,
            http_port: 7474,
            auth_token: None,
            reasoning_type: "ollama".to_string(),
            reasoning_url: "http://localhost:11434".to_string(),
            reasoning_api_key: None,
            embedding_model: "nomic-embed-text".to_string(),
            completion_model: "llama3.2".to_string(),
            noticer_trigger: "events_or_24h".to_string(),
            noticer_selector: "mvp_lens".to_string(),
            noticer_clusterer: "mvp_dbscan".to_string(),
            noticer_synthesizer: "llm_json".to_string(),
            noticer_publisher: "store_rule".to_string(),
            rules_matcher: "exact_bitmask".to_string(),
            rules_scorer: "cosine".to_string(),
            rules_threshold: "dynamic".to_string(),
            rules_executor: "noop".to_string(),
        }
    }
}

pub mod factory;
pub use factory::*;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct ConfigFile {
    #[serde(default)]
    sources: SourcesConfig,
    #[serde(default)]
    transports: TransportsConfig,
    #[serde(default)]
    samplers: SamplersConfig,
    #[serde(default)]
    window_title_allowlist: WindowTitleAllowlist,
    #[serde(default)]
    calendar: CalendarConfig,
    #[serde(default)]
    checkin: CheckinConfig,
    #[serde(default)]
    query: QueryConfig,
    #[serde(default)]
    cloudkit: CloudKitConfig,
    #[serde(default)]
    daemon: DaemonConfig,
    #[serde(default)]
    reasoning: ReasoningConfig,
    #[serde(default)]
    noticer: NoticerConfig,
    #[serde(default)]
    rules: RulesConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SourcesConfig {
    obsidian_vault: String,
    spotlight: bool,
    apple_notes: bool,
    healthkit: bool,
    calendar: bool,
    self_reports: bool,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        let d = AmbientConfig::default();
        Self {
            obsidian_vault: d.obsidian_vault,
            spotlight: d.spotlight,
            apple_notes: d.apple_notes,
            healthkit: d.healthkit,
            calendar: d.calendar,
            self_reports: d.self_reports,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SamplersConfig {
    context_switches: bool,
    active_app_titles: bool,
    audio_input: bool,
}

impl Default for SamplersConfig {
    fn default() -> Self {
        let d = AmbientConfig::default();
        Self {
            context_switches: d.context_switches,
            active_app_titles: d.active_app_titles,
            audio_input: d.audio_input,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TransportsConfig {
    bonjour: bool,
    cloudkit: bool,
    google_health: bool,
}

impl Default for TransportsConfig {
    fn default() -> Self {
        let d = AmbientConfig::default();
        Self {
            bonjour: d.bonjour,
            cloudkit: d.cloudkit,
            google_health: d.google_health,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct WindowTitleAllowlist {
    apps: Vec<String>,
}

impl Default for WindowTitleAllowlist {
    fn default() -> Self {
        Self {
            apps: AmbientConfig::default().window_title_allowlist,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CalendarConfig {
    focus_block_patterns: Vec<String>,
}

impl Default for CalendarConfig {
    fn default() -> Self {
        Self {
            focus_block_patterns: AmbientConfig::default().calendar_focus_block_patterns,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct CheckinConfig {
    reminder_time: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct QueryConfig {
    semantic_weight: f32,
    feedback_weight: f32,
}

impl Default for QueryConfig {
    fn default() -> Self {
        let d = AmbientConfig::default();
        Self {
            semantic_weight: d.semantic_weight,
            feedback_weight: d.feedback_weight,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ReasoningConfig {
    #[serde(rename = "type")]
    backend_type: String,
    url: String,
    api_key: Option<String>,
    embedding_model: String,
    completion_model: String,
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        let d = AmbientConfig::default();
        Self {
            backend_type: d.reasoning_type,
            url: d.reasoning_url,
            api_key: d.reasoning_api_key,
            embedding_model: d.embedding_model,
            completion_model: d.completion_model,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct NoticerConfig {
    trigger: String,
    selector: String,
    clusterer: String,
    synthesizer: String,
    publisher: String,
}

impl Default for NoticerConfig {
    fn default() -> Self {
        let d = AmbientConfig::default();
        Self {
            trigger: d.noticer_trigger,
            selector: d.noticer_selector,
            clusterer: d.noticer_clusterer,
            synthesizer: d.noticer_synthesizer,
            publisher: d.noticer_publisher,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RulesConfig {
    pulse_matcher: String,
    intent_scorer: String,
    threshold_policy: String,
    executor: String,
}

impl Default for RulesConfig {
    fn default() -> Self {
        let d = AmbientConfig::default();
        Self {
            pulse_matcher: d.rules_matcher,
            intent_scorer: d.rules_scorer,
            threshold_policy: d.rules_threshold,
            executor: d.rules_executor,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CloudKitConfig {
    container: String,
    zone_name: String,
    native_bridge: bool,
    native_bridge_cmd: Option<String>,
}

impl Default for CloudKitConfig {
    fn default() -> Self {
        let d = AmbientConfig::default();
        Self {
            container: d.cloudkit_container,
            zone_name: d.cloudkit_zone_name,
            native_bridge: d.cloudkit_native_bridge,
            native_bridge_cmd: d.cloudkit_native_bridge_cmd,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DaemonConfig {
    http_port: u16,
    auth_token: Option<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            http_port: AmbientConfig::default().http_port,
            auth_token: None,
        }
    }
}

impl AmbientConfig {
    pub fn ensure_default(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            let parent = path.parent().ok_or_else(|| {
                ambient_core::CoreError::InvalidInput("config path has no parent".to_string())
            })?;
            fs::create_dir_all(parent).map_err(|e| {
                ambient_core::CoreError::Internal(format!("failed to create config dir: {e}"))
            })?;
            let body = default_config_toml();
            fs::write(path, body).map_err(|e| {
                ambient_core::CoreError::Internal(format!("failed to write config: {e}"))
            })?;
        }

        Self::from_path(path)
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path).map_err(|e| {
            ambient_core::CoreError::Internal(format!("failed to read config: {e}"))
        })?;
        let parsed: ConfigFile = toml::from_str(&raw).map_err(|e| {
            ambient_core::CoreError::InvalidInput(format!("invalid config TOML: {e}"))
        })?;

        Ok(Self {
            obsidian_vault: parsed.sources.obsidian_vault,
            spotlight: parsed.sources.spotlight,
            apple_notes: parsed.sources.apple_notes,
            healthkit: parsed.sources.healthkit,
            calendar: parsed.sources.calendar,
            self_reports: parsed.sources.self_reports,
            bonjour: parsed.transports.bonjour,
            cloudkit: parsed.transports.cloudkit,
            google_health: parsed.transports.google_health,
            context_switches: parsed.samplers.context_switches,
            active_app_titles: parsed.samplers.active_app_titles,
            audio_input: parsed.samplers.audio_input,
            window_title_allowlist: parsed.window_title_allowlist.apps,
            calendar_focus_block_patterns: parsed.calendar.focus_block_patterns,
            checkin_reminder_time: parsed.checkin.reminder_time,
            semantic_weight: parsed.query.semantic_weight,
            feedback_weight: parsed.query.feedback_weight,
            cloudkit_container: parsed.cloudkit.container,
            cloudkit_zone_name: parsed.cloudkit.zone_name,
            cloudkit_native_bridge: parsed.cloudkit.native_bridge,
            cloudkit_native_bridge_cmd: parsed.cloudkit.native_bridge_cmd.and_then(|cmd| {
                if cmd.trim().is_empty() {
                    None
                } else {
                    Some(cmd)
                }
            }),
            http_port: parsed.daemon.http_port,
            auth_token: parsed.daemon.auth_token.and_then(|token| {
                if token.trim().is_empty() {
                    None
                } else {
                    Some(token)
                }
            }),
            reasoning_type: parsed.reasoning.backend_type,
            reasoning_url: parsed.reasoning.url,
            reasoning_api_key: parsed.reasoning.api_key,
            embedding_model: parsed.reasoning.embedding_model,
            completion_model: parsed.reasoning.completion_model,
            noticer_trigger: parsed.noticer.trigger,
            noticer_selector: parsed.noticer.selector,
            noticer_clusterer: parsed.noticer.clusterer,
            noticer_synthesizer: parsed.noticer.synthesizer,
            noticer_publisher: parsed.noticer.publisher,
            rules_matcher: parsed.rules.pulse_matcher,
            rules_scorer: parsed.rules.intent_scorer,
            rules_threshold: parsed.rules.threshold_policy,
            rules_executor: parsed.rules.executor,
        })
    }
}

pub fn run_query(
    engine: &dyn QueryEngine,
    text: &str,
    with_pulse: bool,
) -> Result<Vec<QueryResult>> {
    engine.query(QueryRequest {
        text: text.to_string(),
        k: 10,
        include_pulse_context: with_pulse,
        context_window_secs: Some(120),
    })
}

pub fn run_setup(gate: Arc<dyn CapabilityGate>) -> Vec<String> {
    let wizard = SetupWizard::new(gate);
    wizard.run()
}

pub fn run_doctor(gate: Arc<dyn CapabilityGate>) -> Vec<String> {
    let check = HealthCheck::new(gate);
    check.status_lines()
}

pub fn capability_statuses(
    gate: &dyn CapabilityGate,
) -> HashMap<GatedCapability, CapabilityStatus> {
    let capabilities = [
        GatedCapability::SemanticSearch,
        GatedCapability::CognitiveBadges,
        GatedCapability::PatternReports,
        GatedCapability::HealthCorrelations,
        GatedCapability::CalendarContext,
        GatedCapability::MenuBarOverlay,
    ];

    capabilities
        .iter()
        .map(|c| (*c, gate.status(*c)))
        .collect::<HashMap<_, _>>()
}

pub fn submit_checkin(
    store: &dyn KnowledgeStore,
    energy: u8,
    mood: u8,
    note: Option<String>,
) -> Result<KnowledgeUnit> {
    if !(1..=5).contains(&energy) {
        return Err(ambient_core::CoreError::InvalidInput(
            "energy must be in range 1..=5".to_string(),
        ));
    }
    if !(1..=5).contains(&mood) {
        return Err(ambient_core::CoreError::InvalidInput(
            "mood must be in range 1..=5".to_string(),
        ));
    }

    let observed_at = Utc::now();
    let content = if let Some(n) = &note {
        format!("Energy: {energy}/5, Mood: {mood}/5. {n}")
    } else {
        format!("Energy: {energy}/5, Mood: {mood}/5")
    };

    let mut metadata = HashMap::new();
    metadata.insert("energy".to_string(), serde_json::Value::from(energy));
    metadata.insert("mood".to_string(), serde_json::Value::from(mood));

    let hash = blake3::hash(content.as_bytes());
    let mut content_hash = [0u8; 32];
    content_hash.copy_from_slice(hash.as_bytes());

    let unit = KnowledgeUnit {
        id: Uuid::new_v4(),
        source: SourceId::new("self_report"),
        content,
        title: Some(format!("Check-in — {}", observed_at.format("%Y-%m-%d"))),
        metadata,
        observed_at,
        content_hash,
    };

    store.upsert(unit.clone())?;
    store.record_pulse(PulseEvent {
        timestamp: observed_at,
        signal: PulseSignal::EnergyLevel { score: energy },
    })?;
    store.record_pulse(PulseEvent {
        timestamp: observed_at,
        signal: PulseSignal::MoodLevel { score: mood },
    })?;

    Ok(unit)
}

pub fn record_query_feedback(
    store: &dyn KnowledgeStore,
    query_text: &str,
    unit_id: Uuid,
    useful: bool,
    ms_to_action: Option<u32>,
) -> Result<FeedbackEvent> {
    let signal = if useful {
        FeedbackSignal::QueryResultActedOn {
            query_text: query_text.to_string(),
            unit_id,
            action: ambient_core::ResultAction::OpenedSource,
            ms_to_action: ms_to_action.unwrap_or(0),
        }
    } else {
        FeedbackSignal::QueryResultDismissed {
            query_text: query_text.to_string(),
            unit_id,
        }
    };

    let event = FeedbackEvent {
        id: Uuid::new_v4(),
        timestamp: Utc::now(),
        signal,
    };

    store.record_feedback(event.clone())?;
    Ok(event)
}

pub fn record_oracle_feedback(
    store: &dyn KnowledgeStore,
    query_text: &str,
    unit_id: Uuid,
    useful: bool,
    ms_to_action: Option<u32>,
) -> Result<FeedbackEvent> {
    let signal = if useful {
        FeedbackSignal::OracleResultActedOn {
            query_text: query_text.to_string(),
            unit_id,
            action: ambient_core::ResultAction::OpenedSource,
            ms_to_action: ms_to_action.unwrap_or(0),
            active_lens_snapshot: None, // We don't have this in CLI currently, could be added later
        }
    } else {
        FeedbackSignal::QueryResultDismissed {
            query_text: query_text.to_string(),
            unit_id,
        }
    };

    let event = FeedbackEvent {
        id: Uuid::new_v4(),
        timestamp: Utc::now(),
        signal,
    };

    store.record_feedback(event.clone())?;
    Ok(event)
}

fn default_config_toml() -> &'static str {
    r#"[sources]
obsidian_vault = "~/Documents/Obsidian"
spotlight = true
apple_notes = false
healthkit = false
calendar = false
self_reports = true

[transports]
bonjour = true
cloudkit = false
google_health = false

[samplers]
context_switches = true
active_app_titles = false
audio_input = true

[window_title_allowlist]
apps = ["com.apple.Xcode", "md.obsidian", "com.todesktop.230313mzl4w4u92"]

[calendar]
focus_block_patterns = ["Deep Work", "Focus", "No Meetings", "Blocked"]

[checkin]
reminder_time = ""

[query]
semantic_weight = 0.7
feedback_weight = 0.3

[cloudkit]
container = "iCloud.dev.ambient.private"
zone_name = "AmbientZone"
native_bridge = false
native_bridge_cmd = ""

[reasoning]
backend = "local"
ollama_url = "http://localhost:11434"

[daemon]
http_port = 7474
auth_token = ""

[noticer]
trigger = "events_or_24h"
selector = "mvp_lens"
clusterer = "mvp_dbscan"
synthesizer = "llm_json"
publisher = "store_rule"

[rules]
pulse_matcher = "exact_bitmask"
intent_scorer = "cosine"
threshold_policy = "dynamic"
executor = "noop"
"#
}

#[derive(Clone)]
pub struct HttpAppState {
    pub engine: Arc<dyn QueryEngine>,
    pub store: Arc<dyn KnowledgeStore>,
    pub config: Arc<AmbientConfig>,
    pub auth_token: Option<String>,
    pub status_probe: Option<Arc<dyn DaemonStatusProbe>>,
    pub deep_link_focus: Arc<Mutex<Option<Uuid>>>,
    pub transport_registry: Option<Arc<TransportRegistry>>,
    pub feedback_recorder: Option<Arc<ambient_patterns::ImplicitFeedbackRecorder>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QueryHttpRequest {
    pub text: String,
    pub k: usize,
    pub include_pulse_context: bool,
    pub context_window_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AskHttpRequest {
    pub question: String,
    pub context_text: Option<String>,
    pub k: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PredictHttpRequest {
    pub limit: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CheckinRequest {
    pub energy: u8,
    pub mood: u8,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeedbackRequest {
    pub query_text: String,
    pub unit_id: Uuid,
    pub useful: bool,
    pub ms_to_action: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PatternFeedbackRequest {
    pub pattern_id: Uuid,
    pub useful: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OracleFeedbackRequest {
    pub query_text: String,
    pub unit_id: Uuid,
    pub useful: bool,
    pub ms_to_action: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub units: usize,
    pub sources: Vec<String>,
    pub embedding_available: bool,
    pub load: String,
    pub queue_depth: usize,
    pub upserted_units: u64,
    pub active_sources: Vec<String>,
    pub active_samplers: Vec<String>,
    pub transport_statuses: Vec<TransportStatus>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RelatedParams {
    pub depth: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnitParams {
    pub context_window_secs: Option<u64>,
}

pub trait DaemonStatusProbe: Send + Sync {
    fn reasoning_available(&self) -> bool;
    fn load(&self) -> String;
    fn queue_depth(&self) -> usize;
    fn upserted_units(&self) -> u64;
    fn active_sources(&self) -> Vec<String>;
    fn active_samplers(&self) -> Vec<String>;
    fn transport_statuses(&self) -> Vec<TransportStatus> {
        Vec::new()
    }
}

pub fn build_router(state: HttpAppState) -> Router {
    Router::new()
        .route("/query", post(http_query))
        .route("/ask", post(http_ask))
        .route("/unit/:id", get(http_unit))
        .route("/related/:id", get(http_related))
        .route("/health", get(http_health))
        .route("/checkin", post(http_checkin))
        .route("/predict", post(http_predict))
        .route("/feedback", post(http_feedback))
        .route("/oracle-feedback", post(http_oracle_feedback))
        .route("/pattern-feedback", post(http_pattern_feedback))
        .route("/report", get(http_report))
        .route("/export", get(http_export))
        .route("/transport/:id/push", post(http_transport_push))
        .route("/open/unit/:id", post(http_open_unit))
        .route("/open/focus", get(http_get_focus))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn run_http_server(addr: SocketAddr, state: HttpAppState) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| ambient_core::CoreError::Internal(format!("failed to bind server: {e}")))?;

    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| ambient_core::CoreError::Internal(format!("http server error: {e}")))?;
    Ok(())
}

async fn http_query(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    Json(req): Json<QueryHttpRequest>,
) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }

    let mut result = state.engine.query(QueryRequest {
        text: req.text.clone(),
        k: req.k,
        include_pulse_context: req.include_pulse_context,
        context_window_secs: req.context_window_secs,
    });

    if let Ok(ref mut units) = result {
        if let Some(first) = units.first_mut() {
            if let Ok(engine) = factory::build_rule_engine(&state.config) {
                let predictor = ambient_patterns::ActionPredictor::new(state.store.clone(), engine);
                if let Ok(predictions) = predictor.predict(1) {
                    if !predictions.is_empty() {
                        first.predicted_actions = Some(predictions);
                    }
                }
            }
        }

        if let Some(ref recorder) = state.feedback_recorder {
            let ids = units.iter().map(|u| u.unit.id).collect();
            recorder.on_query_result(&req.text, ids, false, None);
        }
    }

    map_result(result)
}

async fn http_predict(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    Json(req): Json<PredictHttpRequest>,
) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }

    if let Ok(engine) = factory::build_rule_engine(&state.config) {
        let predictor = ambient_patterns::ActionPredictor::new(state.store.clone(), engine);
        let result = predictor.predict(req.limit);
        map_result(result)
    } else {
        map_result::<Vec<ambient_core::PredictedAction>>(Ok(Vec::new())) // Default to empty
    }
}

async fn http_ask(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    Json(req): Json<AskHttpRequest>,
) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }

    let query_text = req.context_text.unwrap_or_else(|| req.question.clone());
    let result = state.engine.answer(
        &req.question,
        QueryRequest {
            text: query_text.clone(),
            k: req.k,
            include_pulse_context: false,
            context_window_secs: None,
        },
    );

    if let Ok(ref _answer) = result {
        if let Some(ref _recorder) = state.feedback_recorder {
            // Distill relevant units from the answer if possible,
            // or just use the QueryResult top items.
        }
    }

    map_result(result)
}

async fn http_unit(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<Uuid>,
    Query(params): Query<UnitParams>,
) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }

    let context_window_secs = params.context_window_secs.unwrap_or(3600);
    let result = state
        .store
        .unit_with_context_live(id, context_window_secs)
        .and_then(|opt| match opt {
            Some((unit, pulse, cognitive_state)) => {
                let feedback = state.store.feedback_score(unit.id).unwrap_or(0.5);
                Ok(QueryResult {
                    unit,
                    score: 1.0,
                    pulse_context: Some(pulse),
                    cognitive_state: Some(cognitive_state),
                    historical_feedback_score: feedback,
                    capability_status: None,
                    predicted_actions: None,
                })
            }
            None => Err(ambient_core::CoreError::NotFound(format!(
                "unit {id} not found"
            ))),
        });

    map_result(result)
}

async fn http_related(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<Uuid>,
    Query(params): Query<RelatedParams>,
) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }
    let depth = params.depth.unwrap_or(2);
    map_result(state.store.related(id, depth))
}

async fn http_health(State(state): State<HttpAppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }

    let units = match state.store.search_fulltext("") {
        Ok(found) => found.len(),
        Err(_) => 0,
    };
    let mut sources = state
        .store
        .search_fulltext("")
        .unwrap_or_default()
        .into_iter()
        .map(|u| u.source.0)
        .collect::<Vec<_>>();
    sources.sort();
    sources.dedup();

    let (
        embedding_available,
        load,
        queue_depth,
        upserted_units,
        active_sources,
        active_samplers,
        transport_statuses,
    ) = match &state.status_probe {
        Some(probe) => (
            probe.reasoning_available(),
            probe.load(),
            probe.queue_depth(),
            probe.upserted_units(),
            probe.active_sources(),
            probe.active_samplers(),
            probe.transport_statuses(),
        ),
        None => (
            true,
            "unconstrained".to_string(),
            0,
            0,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ),
    };

    let payload = HealthResponse {
        status: "ok".to_string(),
        units,
        sources,
        embedding_available,
        load,
        queue_depth,
        upserted_units,
        active_sources,
        active_samplers,
        transport_statuses,
    };
    (StatusCode::OK, Json(payload)).into_response()
}

async fn http_checkin(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    Json(req): Json<CheckinRequest>,
) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }
    map_result(submit_checkin(
        state.store.as_ref(),
        req.energy,
        req.mood,
        req.note,
    ))
}

async fn http_feedback(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    Json(req): Json<FeedbackRequest>,
) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }
    map_result(record_query_feedback(
        state.store.as_ref(),
        &req.query_text,
        req.unit_id,
        req.useful,
        req.ms_to_action,
    ))
}

async fn http_oracle_feedback(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    Json(req): Json<OracleFeedbackRequest>,
) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }
    map_result(record_oracle_feedback(
        state.store.as_ref(),
        &req.query_text,
        req.unit_id,
        req.useful,
        req.ms_to_action,
    ))
}

async fn http_pattern_feedback(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    Json(req): Json<PatternFeedbackRequest>,
) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }
    let signal = if req.useful {
        ambient_core::FeedbackSignal::PatternMarkedUseful {
            pattern_id: req.pattern_id,
        }
    } else {
        ambient_core::FeedbackSignal::PatternMarkedNoise {
            pattern_id: req.pattern_id,
        }
    };
    let event = ambient_core::FeedbackEvent {
        id: uuid::Uuid::new_v4(),
        timestamp: chrono::Utc::now(),
        signal,
    };
    map_result(state.store.record_feedback(event))
}

async fn http_report(State(state): State<HttpAppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }

    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "failed to resolve HOME").into_response()
        }
    };

    let path = std::path::PathBuf::from(home)
        .join(".ambient")
        .join("reports")
        .join("weekly.html");

    if let Ok(content) = std::fs::read_to_string(&path) {
        axum::response::Html(content).into_response()
    } else {
        let msg: &'static str = "Report not found or not generated yet";
        (StatusCode::NOT_FOUND, msg).into_response()
    }
}

async fn http_export(State(state): State<HttpAppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }
    match state.store.search_fulltext("") {
        Ok(units) => Json(units).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Export error: {e}"),
        )
            .into_response(),
    }
}

async fn http_open_unit(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<Uuid>,
) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }
    if let Ok(mut guard) = state.deep_link_focus.lock() {
        *guard = Some(id);
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({ "focused_unit": id })),
    )
        .into_response()
}

async fn http_get_focus(State(state): State<HttpAppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }
    let focused = state.deep_link_focus.lock().ok().and_then(|g| *g);
    (
        StatusCode::OK,
        Json(serde_json::json!({ "focused_unit": focused })),
    )
        .into_response()
}

async fn http_transport_push(
    State(state): State<HttpAppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    body: Bytes,
) -> Response {
    if let Err(resp) = authorize(&state.auth_token, &headers) {
        return resp;
    }
    let Some(registry) = &state.transport_registry else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({"error":"transport registry unavailable"})),
        )
            .into_response();
    };
    match registry.on_push_notification(&id, body.to_vec()) {
        Ok(true) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"accepted": true, "transport_id": id})),
        )
            .into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"transport not found", "transport_id": id})),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string(), "transport_id": id})),
        )
            .into_response(),
    }
}

#[allow(clippy::result_large_err)]
fn authorize(token: &Option<String>, headers: &HeaderMap) -> std::result::Result<(), Response> {
    match token {
        Some(expected) => {
            let actual = headers
                .get("X-Ambient-Token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            if actual == expected {
                Ok(())
            } else {
                Err((
                    StatusCode::UNAUTHORIZED,
                    "missing or invalid X-Ambient-Token",
                )
                    .into_response())
            }
        }
        None => Ok(()),
    }
}

fn map_result<T: Serialize>(result: Result<T>) -> Response {
    match result {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(err) => {
            let code = match err {
                ambient_core::CoreError::NotFound(_) => StatusCode::NOT_FOUND,
                ambient_core::CoreError::InvalidInput(_) => StatusCode::BAD_REQUEST,
                ambient_core::CoreError::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
                ambient_core::CoreError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (code, Json(serde_json::json!({ "error": err.to_string() }))).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use ambient_transport_local::LoadBroadcaster;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use ambient_core::mocks::{MockKnowledgeStore, MockQueryEngine};
    use ambient_core::{
        KnowledgeStore, KnowledgeUnit, QueryResult, SourceId, StreamProvider, StreamTransport,
        TransportId, TransportState, TransportStatus,
    };
    use ambient_onboard::InMemoryCapabilityGate;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::{
        build_router, capability_statuses, record_query_feedback, run_doctor, run_setup,
        submit_checkin, AmbientConfig, HttpAppState, TransportRegistry,
    };

    #[test]
    fn checkin_writes_unit_and_pulse() {
        let store = MockKnowledgeStore::default();

        let unit =
            submit_checkin(&store, 4, 3, Some("deep work".to_string())).expect("checkin works");

        assert_eq!(unit.source.0, "self_report");
        assert!(unit.content.contains("Energy: 4/5"));
    }

    #[test]
    fn setup_and_doctor_are_wired() {
        let gate = Arc::new(InMemoryCapabilityGate::default());
        gate.seed_defaults();

        let setup_steps = run_setup(gate.clone());
        assert!(!setup_steps.is_empty());

        let status = run_doctor(gate.clone());
        assert!(!status.is_empty());

        let map = capability_statuses(gate.as_ref());
        assert!(map.contains_key(&ambient_core::GatedCapability::SemanticSearch));
    }

    #[test]
    fn feedback_event_records() {
        let store = MockKnowledgeStore::default();
        let id = uuid::Uuid::new_v4();

        let event = record_query_feedback(&store, "query", id, true, Some(1500)).expect("feedback");
        assert_eq!(event.id.get_version_num(), 4);
    }

    #[tokio::test]
    async fn http_query_route_returns_results() {
        let store = Arc::new(MockKnowledgeStore::default());
        let engine = Arc::new(MockQueryEngine::default());
        let unit = KnowledgeUnit {
            id: Uuid::new_v4(),
            source: SourceId::new("obsidian"),
            content: "hello".to_string(),
            title: Some("hello".to_string()),
            metadata: HashMap::new(),
            observed_at: chrono::Utc::now(),
            content_hash: [1; 32],
        };
        store.upsert(unit.clone()).expect("upsert");
        engine
            .results
            .lock()
            .expect("results lock")
            .push(QueryResult {
                unit,
                score: 1.0,
                pulse_context: None,
                cognitive_state: None,
                historical_feedback_score: 0.5,
                capability_status: None,
                predicted_actions: None,
            });

        let app = build_router(HttpAppState {
            engine,
            store,
            config: std::sync::Arc::new(AmbientConfig::default()),
            auth_token: None,
            status_probe: None,
            deep_link_focus: Arc::new(Mutex::new(None)),
            transport_registry: None,
            feedback_recorder: None,
        });

        let req = Request::builder()
            .method("POST")
            .uri("/query")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"text":"hello","k":10,"include_pulse_context":false,"context_window_secs":120}"#,
            ))
            .expect("request");

        let response = app.oneshot(req).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("bytes");
        let parsed: Vec<QueryResult> = serde_json::from_slice(&body).expect("json");
        assert_eq!(parsed.len(), 1);
    }

    #[tokio::test]
    async fn http_auth_token_is_enforced() {
        let store = Arc::new(MockKnowledgeStore::default());
        let engine = Arc::new(MockQueryEngine::default());
        let app = build_router(HttpAppState {
            engine,
            store,
            config: std::sync::Arc::new(AmbientConfig::default()),
            auth_token: Some("secret".to_string()),
            status_probe: None,
            deep_link_focus: Arc::new(Mutex::new(None)),
            transport_registry: None,
            feedback_recorder: None,
        });

        let unauthorized = Request::builder()
            .method("GET")
            .uri("/health")
            .body(Body::empty())
            .expect("request");
        let unauthorized_resp = app.clone().oneshot(unauthorized).await.expect("response");
        assert_eq!(unauthorized_resp.status(), StatusCode::UNAUTHORIZED);

        let authorized = Request::builder()
            .method("GET")
            .uri("/health")
            .header("X-Ambient-Token", "secret")
            .body(Body::empty())
            .expect("request");
        let authorized_resp = app.oneshot(authorized).await.expect("response");
        assert_eq!(authorized_resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn http_transport_push_route_accepts_payload() {
        let store = Arc::new(MockKnowledgeStore::default());
        let engine = Arc::new(MockQueryEngine::default());
        let app = build_router(HttpAppState {
            engine,
            store,
            config: std::sync::Arc::new(AmbientConfig::default()),
            auth_token: None,
            status_probe: None,
            deep_link_focus: Arc::new(Mutex::new(None)),
            transport_registry: Some(Arc::new(crate::TransportRegistry::default())),
            feedback_recorder: None,
        });

        let req = Request::builder()
            .method("POST")
            .uri("/transport/cloudkit/push")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"records":[]}"#))
            .expect("request");
        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn http_transport_push_route_dispatches_to_target_transport() {
        struct FakeTransport {
            id: String,
            pushes: Arc<AtomicUsize>,
        }

        impl StreamTransport for FakeTransport {
            fn transport_id(&self) -> TransportId {
                self.id.clone()
            }

            fn start(
                &self,
                _provider: Arc<dyn StreamProvider>,
            ) -> ambient_core::Result<tokio::task::JoinHandle<()>> {
                let handle = tokio::runtime::Handle::current().spawn(async {});
                Ok(handle)
            }

            fn stop(&self) -> ambient_core::Result<()> {
                Ok(())
            }

            fn status(&self) -> TransportStatus {
                TransportStatus {
                    transport_id: self.id.clone(),
                    state: TransportState::Active,
                    peers: Vec::new(),
                    last_event_at: None,
                    events_ingested: 0,
                }
            }

            fn on_push_notification(&self, _payload: Vec<u8>) -> ambient_core::Result<()> {
                self.pushes.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }

        let pushes = Arc::new(AtomicUsize::new(0));
        let transport = Arc::new(FakeTransport {
            id: "cloudkit".to_string(),
            pushes: pushes.clone(),
        });
        let registry = Arc::new(crate::TransportRegistry::from_transports(vec![transport]));

        let store = Arc::new(MockKnowledgeStore::default());
        let engine = Arc::new(MockQueryEngine::default());
        let app = build_router(HttpAppState {
            engine,
            store,
            config: std::sync::Arc::new(AmbientConfig::default()),
            auth_token: None,
            status_probe: None,
            deep_link_focus: Arc::new(Mutex::new(None)),
            transport_registry: Some(registry),
            feedback_recorder: None,
        });

        let req = Request::builder()
            .method("POST")
            .uri("/transport/cloudkit/push")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"records":[{"id":"x"}]}"#))
            .expect("request");
        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        assert_eq!(pushes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn http_transport_push_route_returns_404_for_unknown_transport() {
        let store = Arc::new(MockKnowledgeStore::default());
        let engine = Arc::new(MockQueryEngine::default());
        let app = build_router(HttpAppState {
            engine,
            store,
            config: std::sync::Arc::new(AmbientConfig::default()),
            auth_token: None,
            status_probe: None,
            deep_link_focus: Arc::new(Mutex::new(None)),
            transport_registry: Some(Arc::new(crate::TransportRegistry::default())),
            feedback_recorder: None,
        });

        let req = Request::builder()
            .method("POST")
            .uri("/transport/missing/push")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"records":[]}"#))
            .expect("request");
        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn http_transport_push_route_returns_500_on_transport_error() {
        struct FailingTransport {
            id: String,
        }

        impl StreamTransport for FailingTransport {
            fn transport_id(&self) -> TransportId {
                self.id.clone()
            }

            fn start(
                &self,
                _provider: Arc<dyn StreamProvider>,
            ) -> ambient_core::Result<tokio::task::JoinHandle<()>> {
                let handle = tokio::runtime::Handle::current().spawn(async {});
                Ok(handle)
            }

            fn stop(&self) -> ambient_core::Result<()> {
                Ok(())
            }

            fn status(&self) -> TransportStatus {
                TransportStatus {
                    transport_id: self.id.clone(),
                    state: TransportState::Active,
                    peers: Vec::new(),
                    last_event_at: None,
                    events_ingested: 0,
                }
            }

            fn on_push_notification(&self, _payload: Vec<u8>) -> ambient_core::Result<()> {
                Err(ambient_core::CoreError::Internal(
                    "transport failure".to_string(),
                ))
            }
        }

        let registry = Arc::new(crate::TransportRegistry::from_transports(vec![Arc::new(
            FailingTransport {
                id: "cloudkit".to_string(),
            },
        )]));

        let store = Arc::new(MockKnowledgeStore::default());
        let engine = Arc::new(MockQueryEngine::default());
        let app = build_router(HttpAppState {
            engine,
            store,
            config: std::sync::Arc::new(AmbientConfig::default()),
            auth_token: None,
            status_probe: None,
            deep_link_focus: Arc::new(Mutex::new(None)),
            transport_registry: Some(registry),
            feedback_recorder: None,
        });

        let req = Request::builder()
            .method("POST")
            .uri("/transport/cloudkit/push")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"records":[]}"#))
            .expect("request");
        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn config_parses_cloudkit_transport_and_bridge_flags() {
        let path = std::env::temp_dir().join(format!("ambient-config-{}.toml", Uuid::new_v4()));
        let body = r#"
[sources]
obsidian_vault = "~/Documents/Obsidian"
spotlight = true
apple_notes = false
healthkit = false
calendar = false
self_reports = true

[transports]
bonjour = false
cloudkit = true
google_health = false

[samplers]
context_switches = true
active_app_titles = false
audio_input = true

[window_title_allowlist]
apps = ["com.apple.Xcode"]

[calendar]
focus_block_patterns = ["Focus"]

[checkin]
reminder_time = ""

[query]
semantic_weight = 0.7
feedback_weight = 0.3

[cloudkit]
container = "iCloud.test.container"
zone_name = "TestZone"
native_bridge = true
native_bridge_cmd = "/usr/local/bin/ambient-cloudkit-bridge"

[daemon]
http_port = 7474
auth_token = ""
"#;
        fs::write(&path, body).expect("write config");
        let cfg = AmbientConfig::from_path(&path).expect("parse config");
        let _ = fs::remove_file(&path);

        assert!(cfg.cloudkit);
        assert!(cfg.cloudkit_native_bridge);
        assert_eq!(cfg.cloudkit_container, "iCloud.test.container");
        assert_eq!(cfg.cloudkit_zone_name, "TestZone");
        assert_eq!(
            cfg.cloudkit_native_bridge_cmd.as_deref(),
            Some("/usr/local/bin/ambient-cloudkit-bridge")
        );
        assert!(!cfg.bonjour);
    }

    #[test]
    fn transport_registry_uses_transport_flags() {
        let cfg = AmbientConfig {
            bonjour: false,
            cloudkit: true,
            google_health: false,
            ..AmbientConfig::default()
        };
        let broadcaster = Arc::new(LoadBroadcaster::default());
        let registry = TransportRegistry::from_config(&cfg, broadcaster);
        let ids = registry
            .status_all()
            .into_iter()
            .map(|s| s.transport_id)
            .collect::<Vec<_>>();
        assert!(ids.iter().any(|id| id == "obsidian"));
        assert!(ids.iter().any(|id| id == "cloudkit"));
        assert!(!ids.iter().any(|id| id == "bonjour"));
        assert!(!ids.iter().any(|id| id == "google_health"));
    }
}
