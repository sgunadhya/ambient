use std::collections::HashMap;
use std::path::PathBuf;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
pub use uuid::Uuid;

pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalProfile {
    pub unit_id: Uuid,
    pub hour_peak: i8, // -1 to 23
    pub day_mask: u8,  // bitmask for Mon-Sun
    pub recency_weight: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProceduralRule {
    pub id: Uuid,
    pub lens_intent: Vec<f32>, // Centroid in L1 space
    pub pulse_mask: String,    // Datalog bitmask or pattern
    pub threshold: f32,        // Cosine similarity threshold
    pub action_payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("unsupported operation: {0}")]
    Unsupported(&'static str),
    #[error("entity not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("internal error: {0}")]
    Internal(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceId(pub String);

impl SourceId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RawPayload {
    Markdown {
        content: String,
        path: PathBuf,
    },
    AppleNote {
        protobuf_blob: Bytes,
        note_id: String,
        modified_at: DateTime<Utc>,
    },
    SpotlightItem {
        bundle_id: String,
        display_name: String,
        content_type: String,
        text_content: String,
        last_modified: DateTime<Utc>,
        file_url: Option<String>,
    },
    PlainText {
        content: String,
        path: PathBuf,
    },
    HealthKitSample {
        metric: HealthMetric,
        value: f64,
        unit: String,
        recorded_at: DateTime<Utc>,
    },
    CalendarEvent {
        title: String,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        duration_minutes: u32,
        attendee_count: u32,
        is_focus_block: bool,
        calendar_name: String,
    },
    SelfReport {
        energy: u8,
        mood: u8,
        note: Option<String>,
        reported_at: DateTime<Utc>,
    },
    BehavioralSummary {
        period_start: DateTime<Utc>,
        period_end: DateTime<Utc>,
        avg_context_switch_rate: f32,
        peak_context_switch_rate: f32,
        audio_active_seconds: u32,
        dominant_app: Option<String>,
        app_switch_count: u32,
        was_in_meeting: bool,
        was_in_focus_block: bool,
        device_origin: String,
    },
    KnowledgeUnitSync {
        unit: Box<KnowledgeUnit>,
        device_origin: String,
    },
    FeedbackEventSync {
        event: Box<FeedbackEvent>,
        device_origin: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthMetric {
    HRV,
    RestingHeartRate,
    SleepDuration,
    SleepQuality,
    StepCount,
    ActiveCalories,
    RespiratoryRate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawEvent {
    pub source: SourceId,
    pub timestamp: DateTime<Utc>,
    pub payload: RawPayload,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeUnit {
    pub id: Uuid,
    pub source: SourceId,
    pub content: String,
    pub title: Option<String>,
    pub metadata: HashMap<String, Value>,
    pub observed_at: DateTime<Utc>,
    pub content_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PulseSignal {
    ContextSwitchRate {
        switches_per_minute: f32,
    },
    ActiveApp {
        bundle_id: String,
        window_title: Option<String>,
    },
    AudioInputActive {
        active: bool,
    },
    TimeContext {
        hour_of_day: u8,
        day_of_week: u8,
        is_weekend: bool,
    },
    CalendarContext {
        in_meeting: bool,
        in_focus_block: bool,
        minutes_until_next_event: Option<u32>,
        current_event_duration_minutes: Option<u32>,
    },
    EnergyLevel {
        score: u8,
    },
    MoodLevel {
        score: u8,
    },
    HRVScore {
        value: f32,
    },
    SleepQuality {
        score: f32,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PulseEvent {
    pub timestamp: DateTime<Utc>,
    pub signal: PulseSignal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryRequest {
    pub text: String,
    pub k: usize,
    pub include_pulse_context: bool,
    pub context_window_secs: Option<u64>,
}

impl Default for QueryRequest {
    fn default() -> Self {
        Self {
            text: String::new(),
            k: 10,
            include_pulse_context: false,
            context_window_secs: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredictedAction {
    pub rule_id: Uuid,
    pub action_payload: serde_json::Value,
    pub confidence: f32,
    pub trigger_reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    pub unit: KnowledgeUnit,
    pub score: f32,
    pub pulse_context: Option<Vec<PulseEvent>>,
    pub cognitive_state: Option<CognitiveState>,
    pub historical_feedback_score: f32,
    pub capability_status: Option<CapabilityStatus>,
    pub predicted_actions: Option<Vec<PredictedAction>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CognitiveState {
    pub was_in_flow: bool,
    pub was_on_call: bool,
    pub dominant_app: Option<String>,
    pub time_of_day: Option<u8>,
    pub was_in_meeting: bool,
    pub was_in_focus_block: bool,
    pub energy_level: Option<u8>,
    pub mood_level: Option<u8>,
    pub hrv_score: Option<f32>,
    pub sleep_quality: Option<f32>,
    pub minutes_since_last_meeting: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackEvent {
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub signal: FeedbackSignal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FeedbackSignal {
    QueryResultActedOn {
        query_text: String,
        unit_id: Uuid,
        action: ResultAction,
        ms_to_action: u32,
    },
    QueryResultDismissed {
        query_text: String,
        unit_id: Uuid,
    },
    TriggerAcknowledged {
        trigger_id: String,
        unit_id: Uuid,
    },
    TriggerDismissed {
        trigger_id: String,
        unit_id: Uuid,
    },
    PatternMarkedUseful {
        pattern_id: Uuid,
    },
    PatternMarkedNoise {
        pattern_id: Uuid,
    },
    /// Emitted when the user acts on an Oracle (`/ask`) result.
    /// This is the highest-quality learning signal — the Noticer
    /// weights these events 3x in cluster discovery.
    OracleResultActedOn {
        query_text: String,
        unit_id: Uuid,
        action: ResultAction,
        ms_to_action: u32,
        /// L1 lens vector active at the moment of the Oracle call.
        /// Stored so the Noticer can associate the success with the
        /// semantic context, not just the query string.
        active_lens_snapshot: Option<Vec<f32>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultAction {
    OpenedSource,
    OpenedDetail,
    CopiedContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemLoad {
    Unconstrained,
    Conservative,
    Minimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tier", rename_all = "snake_case")]
pub enum License {
    Free,
    Pro { expires_at: DateTime<Utc> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningBackend {
    Local {
        ollama_base_url: String,
    },
    OpenAI {
        base_url: String,
        api_key: Option<String>,
    },
    Remote {
        provider: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatedFeature {
    MenuBarOverlay,
    PatternReport,
    CrossDeviceSync,
    HostedInference,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LicenseError {
    #[error("feature {feature:?} requires a Pro license")]
    ProRequired { feature: GatedFeature },
    #[error("license expired at {expires_at}")]
    Expired { expires_at: DateTime<Utc> },
}

pub type Offset = u64;
pub type ConsumerId = String;
pub type TransportId = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventLogEntry {
    Raw(RawEvent),
    Pulse(PulseEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportStatus {
    pub transport_id: TransportId,
    pub state: TransportState,
    pub peers: Vec<PeerStatus>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub events_ingested: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TransportState {
    Active,
    Degraded { reason: String },
    Inactive,
    RequiresSetup { action: SetupAction },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerStatus {
    pub peer_id: String,
    pub device_name: String,
    pub last_seen: DateTime<Utc>,
    pub events_received: u64,
    pub reachable: bool,
}

pub trait Normalizer: Send + Sync {
    fn can_handle(&self, payload: &RawPayload) -> bool;
    fn normalize(&self, event: RawEvent) -> Result<KnowledgeUnit>;
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexBackend {
    Hnsw,
    Flat,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LensId {
    L1Semantic,
    L2Technical,
    L3Fulltext,
    L4Temporal,
    L5Social,
}

impl LensId {
    pub fn as_str(&self) -> &'static str {
        match self {
            LensId::L1Semantic => "l1_semantic",
            LensId::L2Technical => "l2_technical",
            LensId::L3Fulltext => "l3_fulltext",
            LensId::L4Temporal => "l4_temporal",
            LensId::L5Social => "l5_social",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LensConfig {
    pub id: LensId,
    pub dimensions: usize,
    pub model_name: String,
    pub index_backend: IndexBackend,
}

impl LensConfig {
    pub fn semantic() -> Self {
        Self {
            id: LensId::L1Semantic,
            dimensions: 768,
            model_name: "nomic-embed-text".to_string(),
            index_backend: IndexBackend::Hnsw,
        }
    }

    pub fn technical() -> Self {
        Self {
            id: LensId::L2Technical,
            dimensions: 768,
            model_name: "jina-embeddings-v2-base-code".to_string(),
            index_backend: IndexBackend::Hnsw,
        }
    }

    pub fn social() -> Self {
        Self {
            id: LensId::L5Social,
            dimensions: 384,
            model_name: "gte-small".to_string(),
            index_backend: IndexBackend::Hnsw,
        }
    }
}

pub trait LensIndexStore: Send + Sync {
    fn upsert_lens_vec(&self, unit_id: Uuid, lens: &LensConfig, vec: &[f32]) -> Result<()>;
    fn search_lens_vec(&self, lens: &LensConfig, query_vec: &[f32], k: usize) -> Result<Vec<Uuid>>;
    fn get_all_lens_vectors(&self, lens: &LensConfig) -> Result<Vec<(Uuid, Vec<f32>)>>;
}

pub trait LensRetriever: Send + Sync {
    fn config(&self) -> Option<&LensConfig>;
    fn retrieve(&self, request: &QueryRequest) -> Result<Vec<Uuid>>;
}

pub trait KnowledgeStore: Send + Sync {
    fn upsert(&self, unit: KnowledgeUnit) -> Result<()>;
    fn search_semantic(&self, query_vec: &[f32], k: usize) -> Result<Vec<KnowledgeUnit>>;
    fn search_fulltext(&self, query: &str) -> Result<Vec<KnowledgeUnit>>;
    fn related(&self, id: Uuid, depth: usize) -> Result<Vec<KnowledgeUnit>>;
    fn get_by_id(&self, id: Uuid) -> Result<Option<KnowledgeUnit>>;

    fn record_pulse(&self, event: PulseEvent) -> Result<()>;
    fn pulse_window(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<PulseEvent>>;

    fn unit_with_context(
        &self,
        id: Uuid,
        context_window_secs: u64,
    ) -> Result<Option<(KnowledgeUnit, Vec<PulseEvent>, CognitiveState)>>;

    fn unit_with_context_fast(&self, id: Uuid) -> Result<Option<(KnowledgeUnit, CognitiveState)>> {
        self.unit_with_context(id, 120)
            .map(|opt| opt.map(|(unit, _, state)| (unit, state)))
    }

    fn unit_with_context_live(
        &self,
        id: Uuid,
        window_secs: u64,
    ) -> Result<Option<(KnowledgeUnit, Vec<PulseEvent>, CognitiveState)>> {
        self.unit_with_context(id, window_secs)
    }

    fn record_feedback(&self, event: FeedbackEvent) -> Result<()>;
    fn feedback_score(&self, unit_id: Uuid) -> Result<f32>;
    fn pattern_feedback(&self, pattern_id: Uuid) -> Result<Option<String>>;
    fn save_rule(&self, rule: ProceduralRule) -> Result<()>;
    fn get_active_rules_for_pulse(&self, pulse_mask: &str) -> Result<Vec<ProceduralRule>>;
    fn get_all_rules(&self) -> Result<Vec<ProceduralRule>>;

    /// Retrieve the most recent N cognitive snapshots.
    fn get_recent_snapshots(&self, limit: usize) -> Result<Vec<(Uuid, CognitiveState)>>;

    /// Re-calculate and update the temporal profiles for units based on pulse history.
    fn calculate_temporal_profile(&self, unit_id: Uuid) -> Result<()>;
    fn get_temporal_profile(&self, unit_id: Uuid) -> Result<Option<TemporalProfile>>;
}

pub trait ReasoningEngine: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn embed_with_model(&self, text: &str, model: &str) -> Result<Vec<f32>>;
    fn answer(&self, question: &str, context: &[KnowledgeUnit]) -> Result<String>;
}

pub fn derive_cognitive_state(observed_at: DateTime<Utc>, pulse: &[PulseEvent]) -> CognitiveState {
    let mut context_sum = 0.0f32;
    let mut context_count = 0usize;
    let mut was_on_call = false;
    let mut app_counts: HashMap<String, usize> = HashMap::new();
    let mut was_in_meeting = false;
    let mut was_in_focus_block = false;
    let mut energy_level = None;
    let mut mood_level = None;
    let mut hrv_score = None;
    let mut sleep_quality = None;
    let mut minutes_until_next = None;

    for event in pulse {
        match &event.signal {
            PulseSignal::ContextSwitchRate {
                switches_per_minute,
            } => {
                context_sum += switches_per_minute;
                context_count += 1;
            }
            PulseSignal::ActiveApp { bundle_id, .. } => {
                *app_counts.entry(bundle_id.clone()).or_insert(0) += 1;
            }
            PulseSignal::AudioInputActive { active } => {
                if *active {
                    was_on_call = true;
                }
            }
            PulseSignal::CalendarContext {
                in_meeting,
                in_focus_block,
                minutes_until_next_event,
                ..
            } => {
                was_in_meeting |= *in_meeting;
                was_in_focus_block |= *in_focus_block;
                minutes_until_next = *minutes_until_next_event;
            }
            PulseSignal::EnergyLevel { score } => energy_level = Some(*score),
            PulseSignal::MoodLevel { score } => mood_level = Some(*score),
            PulseSignal::HRVScore { value } => hrv_score = Some(*value),
            PulseSignal::SleepQuality { score } => sleep_quality = Some(*score),
            PulseSignal::TimeContext { .. } => {}
        }
    }

    let avg_switch_rate = if context_count == 0 {
        f32::INFINITY
    } else {
        context_sum / context_count as f32
    };

    let dominant_app = app_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(bundle, _)| bundle);

    CognitiveState {
        was_in_flow: avg_switch_rate < 2.0,
        was_on_call,
        dominant_app,
        time_of_day: Some(observed_at.hour() as u8),
        was_in_meeting,
        was_in_focus_block,
        energy_level,
        mood_level,
        hrv_score,
        sleep_quality,
        minutes_since_last_meeting: minutes_until_next,
    }
}

pub trait QueryEngine: Send + Sync {
    fn query(&self, req: QueryRequest) -> Result<Vec<QueryResult>>;
    fn answer(&self, question: &str, req: QueryRequest) -> Result<Option<String>>;
}

pub trait StreamProvider: Send + Sync {
    fn append_raw(&self, event: RawEvent) -> Result<Offset>;
    fn append_pulse(&self, event: PulseEvent) -> Result<Offset>;
    fn append_raw_from(&self, event: RawEvent, transport: &str, record_id: &str) -> Result<Offset>;
    fn read(
        &self,
        consumer: &ConsumerId,
        from: Offset,
        limit: usize,
    ) -> Result<Vec<(Offset, EventLogEntry)>>;
    fn commit(&self, consumer: &ConsumerId, offset: Offset) -> Result<()>;
    fn last_committed(&self, consumer: &ConsumerId) -> Result<Option<Offset>>;
    fn subscribe(
        &self,
        consumer: &ConsumerId,
        tx: tokio::sync::watch::Sender<Offset>,
    ) -> Result<()>;
}

pub trait StreamTransport: Send + Sync {
    fn transport_id(&self) -> TransportId;
    fn start(&self, provider: Arc<dyn StreamProvider>) -> Result<tokio::task::JoinHandle<()>>;
    fn stop(&self) -> Result<()>;
    fn status(&self) -> TransportStatus;
    fn save_state(&self) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn load_state(&self, _state: &[u8]) -> Result<()> {
        Ok(())
    }
    fn on_push_notification(&self, _payload: Vec<u8>) -> Result<()> {
        Ok(())
    }
}

pub trait SpotlightExporter: Send + Sync {
    fn export(&self, unit: &KnowledgeUnit) -> Result<()>;
    fn delete(&self, id: Uuid) -> Result<()>;
}

pub trait TriggerCondition: Send + Sync {
    fn evaluate(&self, unit: &KnowledgeUnit, store: &dyn KnowledgeStore) -> Result<bool>;
}

pub trait TriggerAction: Send + Sync {
    fn fire(&self, unit: &KnowledgeUnit) -> Result<()>;
}

pub trait LoadAware: Send + Sync {
    fn on_load_change(&self, load: SystemLoad);
}

pub trait LicenseGate: Send + Sync {
    fn is_pro(&self) -> bool;
    fn check(&self, feature: GatedFeature) -> std::result::Result<(), LicenseError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CapabilityStatus {
    Ready,
    Pending {
        reason: String,
        estimated_eta: Option<Duration>,
    },
    RequiresSetup {
        action: SetupAction,
    },
    RequiresPermission {
        permission: MacOSPermission,
    },
    RequiresDependency {
        dependency: ExternalDependency,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupAction {
    ConfigureVaultPath,
    InstallOllama,
    GrantAccessibility,
    GrantMicrophone,
    GrantHealthKit,
    GrantCalendar,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacOSPermission {
    Accessibility,
    Microphone,
    HealthKit,
    Calendar,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExternalDependency {
    Ollama { install_command: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatedCapability {
    SemanticSearch,
    CognitiveBadges,
    PatternReports,
    HealthCorrelations,
    CalendarContext,
    MenuBarOverlay,
}

pub trait CapabilityGate: Send + Sync {
    fn status(&self, capability: GatedCapability) -> CapabilityStatus;
    fn mark_ready(&self, capability: GatedCapability);
}

pub struct KnowledgeConsumer {
    provider: Arc<dyn StreamProvider>,
    consumer_id: ConsumerId,
    offset: Mutex<Offset>,
}

impl KnowledgeConsumer {
    pub fn new(provider: Arc<dyn StreamProvider>, consumer_id: ConsumerId) -> Result<Self> {
        let offset = provider.last_committed(&consumer_id)?.unwrap_or(0);
        Ok(Self {
            provider,
            consumer_id,
            offset: Mutex::new(offset),
        })
    }

    pub fn provider(&self) -> Arc<dyn StreamProvider> {
        self.provider.clone()
    }

    pub fn poll<F>(&self, limit: usize, mut f: F) -> Result<()>
    where
        F: FnMut(Offset, EventLogEntry) -> Result<()>,
    {
        let mut offset_guard = self.offset.lock().map_err(|_| {
            CoreError::Internal("knowledge consumer offset lock poisoned".to_string())
        })?;
        let batch = self
            .provider
            .read(&self.consumer_id, *offset_guard, limit)?;

        for (next_offset, entry) in batch {
            f(next_offset, entry)?;
            self.provider.commit(&self.consumer_id, next_offset)?;
            *offset_guard = next_offset;
        }

        Ok(())
    }
}

#[cfg(any(test, feature = "test-mocks"))]
pub mod mocks {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use chrono::{DateTime, Timelike, Utc};
    use uuid::Uuid;

    use crate::{
        CapabilityGate, CapabilityStatus, CognitiveState, ConsumerId, CoreError, EventLogEntry,
        ExternalDependency, FeedbackEvent, GatedCapability, GatedFeature, KnowledgeStore,
        LicenseError, LicenseGate, LoadAware, Normalizer, Offset, PeerStatus, PulseEvent,
        QueryEngine, QueryRequest, QueryResult, RawEvent, RawPayload, ReasoningEngine, Result,
        SpotlightExporter, StreamProvider, StreamTransport, SystemLoad, TransportState,
        TransportStatus, TriggerAction, TriggerCondition,
    };

    #[derive(Debug, Clone)]
    pub struct NormalizerCall {
        pub method: String,
    }

    pub struct MockNormalizer {
        pub payload: RawPayload,
        pub output: crate::KnowledgeUnit,
        pub calls: Mutex<Vec<NormalizerCall>>,
    }

    impl Normalizer for MockNormalizer {
        fn can_handle(&self, payload: &RawPayload) -> bool {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(NormalizerCall {
                    method: "can_handle".to_string(),
                });
            }
            payload == &self.payload
        }

        fn normalize(&self, _event: RawEvent) -> Result<crate::KnowledgeUnit> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(NormalizerCall {
                    method: "normalize".to_string(),
                });
            }
            Ok(self.output.clone())
        }
    }

    #[derive(Debug, Clone)]
    pub struct KnowledgeStoreCall {
        pub method: String,
    }

    #[derive(Default)]
    pub struct MockKnowledgeStore {
        pub units: Mutex<HashMap<Uuid, crate::KnowledgeUnit>>,
        pub pulses: Mutex<Vec<PulseEvent>>,
        pub calls: Mutex<Vec<KnowledgeStoreCall>>,
    }

    impl KnowledgeStore for MockKnowledgeStore {
        fn upsert(&self, unit: crate::KnowledgeUnit) -> Result<()> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(KnowledgeStoreCall {
                    method: "upsert".to_string(),
                });
            }
            self.units
                .lock()
                .map_err(|_| CoreError::Internal("units lock poisoned".to_string()))?
                .insert(unit.id, unit);
            Ok(())
        }

        fn search_semantic(
            &self,
            _query_vec: &[f32],
            k: usize,
        ) -> Result<Vec<crate::KnowledgeUnit>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(KnowledgeStoreCall {
                    method: "search_semantic".to_string(),
                });
            }
            let values: Vec<_> = self
                .units
                .lock()
                .map_err(|_| CoreError::Internal("units lock poisoned".to_string()))?
                .values()
                .take(k)
                .cloned()
                .collect();
            Ok(values)
        }

        fn search_fulltext(&self, _query: &str) -> Result<Vec<crate::KnowledgeUnit>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(KnowledgeStoreCall {
                    method: "search_fulltext".to_string(),
                });
            }
            let values: Vec<_> = self
                .units
                .lock()
                .map_err(|_| CoreError::Internal("units lock poisoned".to_string()))?
                .values()
                .cloned()
                .collect();
            Ok(values)
        }

        fn related(&self, _id: Uuid, _depth: usize) -> Result<Vec<crate::KnowledgeUnit>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(KnowledgeStoreCall {
                    method: "related".to_string(),
                });
            }
            self.search_fulltext("")
        }

        fn get_by_id(&self, id: Uuid) -> Result<Option<crate::KnowledgeUnit>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(KnowledgeStoreCall {
                    method: "get_by_id".to_string(),
                });
            }
            Ok(self
                .units
                .lock()
                .map_err(|_| CoreError::Internal("units lock poisoned".to_string()))?
                .get(&id)
                .cloned())
        }

        fn record_pulse(&self, event: PulseEvent) -> Result<()> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(KnowledgeStoreCall {
                    method: "record_pulse".to_string(),
                });
            }
            self.pulses
                .lock()
                .map_err(|_| CoreError::Internal("pulses lock poisoned".to_string()))?
                .push(event);
            Ok(())
        }

        fn pulse_window(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<PulseEvent>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(KnowledgeStoreCall {
                    method: "pulse_window".to_string(),
                });
            }
            let values: Vec<_> = self
                .pulses
                .lock()
                .map_err(|_| CoreError::Internal("pulses lock poisoned".to_string()))?
                .iter()
                .filter(|p| p.timestamp >= from && p.timestamp <= to)
                .cloned()
                .collect();
            Ok(values)
        }

        fn unit_with_context(
            &self,
            id: Uuid,
            context_window_secs: u64,
        ) -> Result<Option<(crate::KnowledgeUnit, Vec<PulseEvent>, CognitiveState)>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(KnowledgeStoreCall {
                    method: "unit_with_context".to_string(),
                });
            }
            if let Some(unit) = self.get_by_id(id)? {
                let from = unit.observed_at - chrono::Duration::seconds(context_window_secs as i64);
                let to = unit.observed_at + chrono::Duration::seconds(context_window_secs as i64);
                let pulse = self.pulse_window(from, to)?;
                let state = CognitiveState {
                    was_in_flow: false,
                    was_on_call: false,
                    dominant_app: None,
                    time_of_day: Some(unit.observed_at.hour() as u8),
                    was_in_meeting: false,
                    was_in_focus_block: false,
                    energy_level: None,
                    mood_level: None,
                    hrv_score: None,
                    sleep_quality: None,
                    minutes_since_last_meeting: None,
                };
                return Ok(Some((unit, pulse, state)));
            }
            Ok(None)
        }

        fn record_feedback(&self, _event: FeedbackEvent) -> Result<()> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(KnowledgeStoreCall {
                    method: "record_feedback".to_string(),
                });
            }
            Ok(())
        }

        fn feedback_score(&self, _unit_id: Uuid) -> Result<f32> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(KnowledgeStoreCall {
                    method: "feedback_score".to_string(),
                });
            }
            Ok(0.5)
        }

        fn pattern_feedback(&self, _pattern_id: Uuid) -> Result<Option<String>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(KnowledgeStoreCall {
                    method: "pattern_feedback".to_string(),
                });
            }
            Ok(None)
        }

        fn calculate_temporal_profile(&self, _unit_id: Uuid) -> Result<()> {
            Ok(())
        }

        fn get_temporal_profile(&self, _unit_id: Uuid) -> Result<Option<crate::TemporalProfile>> {
            Ok(None)
        }

        fn save_rule(&self, _rule: crate::ProceduralRule) -> Result<()> {
            Ok(())
        }

        fn get_active_rules_for_pulse(
            &self,
            _pulse_mask: &str,
        ) -> Result<Vec<crate::ProceduralRule>> {
            Ok(Vec::new())
        }

        fn get_all_rules(&self) -> Result<Vec<crate::ProceduralRule>> {
            Ok(Vec::new())
        }

        fn get_recent_snapshots(&self, _limit: usize) -> Result<Vec<(Uuid, CognitiveState)>> {
            Ok(Vec::new())
        }
    }

    #[derive(Debug, Clone)]
    pub struct ReasoningEngineCall {
        pub method: String,
    }

    #[derive(Default)]
    pub struct MockReasoningEngine {
        pub calls: Mutex<Vec<ReasoningEngineCall>>,
    }

    impl ReasoningEngine for MockReasoningEngine {
        fn embed(&self, text: &str) -> Result<Vec<f32>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(ReasoningEngineCall {
                    method: "embed".to_string(),
                });
            }
            Ok(vec![text.len() as f32])
        }

        fn embed_with_model(&self, text: &str, _model: &str) -> Result<Vec<f32>> {
            self.embed(text)
        }

        fn answer(&self, question: &str, _context: &[crate::KnowledgeUnit]) -> Result<String> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(ReasoningEngineCall {
                    method: "answer".to_string(),
                });
            }
            Ok(format!("mock answer: {question}"))
        }
    }

    #[derive(Debug, Clone)]
    pub struct QueryEngineCall {
        pub method: String,
    }

    #[derive(Default)]
    pub struct MockQueryEngine {
        pub results: Mutex<Vec<QueryResult>>,
        pub calls: Mutex<Vec<QueryEngineCall>>,
    }

    impl QueryEngine for MockQueryEngine {
        fn query(&self, _req: QueryRequest) -> Result<Vec<QueryResult>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(QueryEngineCall {
                    method: "query".to_string(),
                });
            }
            Ok(self
                .results
                .lock()
                .map_err(|_| CoreError::Internal("results lock poisoned".to_string()))?
                .clone())
        }

        fn answer(&self, _question: &str, _req: QueryRequest) -> Result<Option<String>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(QueryEngineCall {
                    method: "answer".to_string(),
                });
            }
            Ok(None)
        }
    }

    #[derive(Debug, Clone)]
    pub struct StreamProviderCall {
        pub method: String,
    }

    #[derive(Default)]
    pub struct MockStreamProvider {
        pub entries: Mutex<Vec<(Offset, EventLogEntry)>>,
        pub committed: Mutex<HashMap<ConsumerId, Offset>>,
        pub record_ids: Mutex<HashMap<String, Offset>>,
        pub subscribers: Mutex<HashMap<ConsumerId, tokio::sync::watch::Sender<Offset>>>,
        pub calls: Mutex<Vec<StreamProviderCall>>,
    }

    impl StreamProvider for MockStreamProvider {
        fn append_raw(&self, event: RawEvent) -> Result<Offset> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(StreamProviderCall {
                    method: "append_raw".to_string(),
                });
            }
            let mut entries = self
                .entries
                .lock()
                .map_err(|_| CoreError::Internal("entries lock poisoned".to_string()))?;
            let next = (entries.len() as Offset) + 1;
            entries.push((next, EventLogEntry::Raw(event)));
            drop(entries);
            self.notify_subscribers(next);
            Ok(next)
        }

        fn append_pulse(&self, event: PulseEvent) -> Result<Offset> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(StreamProviderCall {
                    method: "append_pulse".to_string(),
                });
            }
            let mut entries = self
                .entries
                .lock()
                .map_err(|_| CoreError::Internal("entries lock poisoned".to_string()))?;
            let next = (entries.len() as Offset) + 1;
            entries.push((next, EventLogEntry::Pulse(event)));
            drop(entries);
            self.notify_subscribers(next);
            Ok(next)
        }

        fn append_raw_from(
            &self,
            event: RawEvent,
            _transport: &str,
            record_id: &str,
        ) -> Result<Offset> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(StreamProviderCall {
                    method: "append_raw_from".to_string(),
                });
            }
            if let Some(existing) = self
                .record_ids
                .lock()
                .map_err(|_| CoreError::Internal("record ids lock poisoned".to_string()))?
                .get(record_id)
                .copied()
            {
                return Ok(existing);
            }
            let offset = self.append_raw(event)?;
            self.record_ids
                .lock()
                .map_err(|_| CoreError::Internal("record ids lock poisoned".to_string()))?
                .insert(record_id.to_string(), offset);
            Ok(offset)
        }

        fn read(
            &self,
            _consumer: &ConsumerId,
            from: Offset,
            limit: usize,
        ) -> Result<Vec<(Offset, EventLogEntry)>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(StreamProviderCall {
                    method: "read".to_string(),
                });
            }
            Ok(self
                .entries
                .lock()
                .map_err(|_| CoreError::Internal("entries lock poisoned".to_string()))?
                .iter()
                .filter(|(offset, _)| *offset > from)
                .take(limit)
                .cloned()
                .collect())
        }

        fn commit(&self, consumer: &ConsumerId, offset: Offset) -> Result<()> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(StreamProviderCall {
                    method: "commit".to_string(),
                });
            }
            self.committed
                .lock()
                .map_err(|_| CoreError::Internal("committed lock poisoned".to_string()))?
                .insert(consumer.clone(), offset);
            Ok(())
        }

        fn last_committed(&self, consumer: &ConsumerId) -> Result<Option<Offset>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(StreamProviderCall {
                    method: "last_committed".to_string(),
                });
            }
            Ok(self
                .committed
                .lock()
                .map_err(|_| CoreError::Internal("committed lock poisoned".to_string()))?
                .get(consumer)
                .copied())
        }

        fn subscribe(
            &self,
            consumer: &ConsumerId,
            tx: tokio::sync::watch::Sender<Offset>,
        ) -> Result<()> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(StreamProviderCall {
                    method: "subscribe".to_string(),
                });
            }
            self.subscribers
                .lock()
                .map_err(|_| CoreError::Internal("subscribers lock poisoned".to_string()))?
                .insert(consumer.clone(), tx);
            Ok(())
        }
    }

    impl MockStreamProvider {
        fn notify_subscribers(&self, offset: Offset) {
            let subscribers = match self.subscribers.lock() {
                Ok(guard) => guard.values().cloned().collect::<Vec<_>>(),
                Err(_) => return,
            };
            for tx in subscribers {
                let _ = tx.send(offset);
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct StreamTransportCall {
        pub method: String,
    }

    pub struct MockStreamTransport {
        pub id: String,
        pub calls: Mutex<Vec<StreamTransportCall>>,
    }

    impl Default for MockStreamTransport {
        fn default() -> Self {
            Self {
                id: "mock-transport".to_string(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl StreamTransport for MockStreamTransport {
        fn transport_id(&self) -> String {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(StreamTransportCall {
                    method: "transport_id".to_string(),
                });
            }
            self.id.clone()
        }

        fn start(
            &self,
            _provider: std::sync::Arc<dyn StreamProvider>,
        ) -> Result<tokio::task::JoinHandle<()>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(StreamTransportCall {
                    method: "start".to_string(),
                });
            }
            let handle = tokio::runtime::Handle::try_current()
                .map_err(|e| CoreError::Internal(format!("tokio runtime unavailable: {e}")))?
                .spawn(async {});
            Ok(handle)
        }

        fn stop(&self) -> Result<()> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(StreamTransportCall {
                    method: "stop".to_string(),
                });
            }
            Ok(())
        }

        fn status(&self) -> TransportStatus {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(StreamTransportCall {
                    method: "status".to_string(),
                });
            }
            TransportStatus {
                transport_id: self.id.clone(),
                state: TransportState::Active,
                peers: vec![PeerStatus {
                    peer_id: "peer".to_string(),
                    device_name: "Mock Device".to_string(),
                    last_seen: Utc::now(),
                    events_received: 0,
                    reachable: true,
                }],
                last_event_at: None,
                events_ingested: 0,
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct SpotlightExporterCall {
        pub method: String,
    }

    #[derive(Default)]
    pub struct MockSpotlightExporter {
        pub calls: Mutex<Vec<SpotlightExporterCall>>,
    }

    impl SpotlightExporter for MockSpotlightExporter {
        fn export(&self, _unit: &crate::KnowledgeUnit) -> Result<()> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(SpotlightExporterCall {
                    method: "export".to_string(),
                });
            }
            Ok(())
        }

        fn delete(&self, _id: Uuid) -> Result<()> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(SpotlightExporterCall {
                    method: "delete".to_string(),
                });
            }
            Ok(())
        }
    }

    #[derive(Debug, Clone)]
    pub struct TriggerConditionCall {
        pub method: String,
    }

    pub struct MockTriggerCondition {
        pub value: bool,
        pub calls: Mutex<Vec<TriggerConditionCall>>,
    }

    impl TriggerCondition for MockTriggerCondition {
        fn evaluate(
            &self,
            _unit: &crate::KnowledgeUnit,
            _store: &dyn KnowledgeStore,
        ) -> Result<bool> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(TriggerConditionCall {
                    method: "evaluate".to_string(),
                });
            }
            Ok(self.value)
        }
    }

    #[derive(Debug, Clone)]
    pub struct TriggerActionCall {
        pub method: String,
    }

    #[derive(Default)]
    pub struct MockTriggerAction {
        pub calls: Mutex<Vec<TriggerActionCall>>,
    }

    impl TriggerAction for MockTriggerAction {
        fn fire(&self, _unit: &crate::KnowledgeUnit) -> Result<()> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(TriggerActionCall {
                    method: "fire".to_string(),
                });
            }
            Ok(())
        }
    }

    #[derive(Debug, Clone)]
    pub struct LoadAwareCall {
        pub load: SystemLoad,
    }

    #[derive(Default)]
    pub struct MockLoadAware {
        pub calls: Mutex<Vec<LoadAwareCall>>,
    }

    impl LoadAware for MockLoadAware {
        fn on_load_change(&self, load: SystemLoad) {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(LoadAwareCall { load });
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct LicenseGateCall {
        pub method: String,
    }

    pub struct MockLicenseGate {
        pub is_pro_user: bool,
        pub expired_at: Option<DateTime<Utc>>,
        pub calls: Mutex<Vec<LicenseGateCall>>,
    }

    #[derive(Default)]
    pub struct MockCapabilityGate {
        pub statuses: Mutex<HashMap<GatedCapability, CapabilityStatus>>,
    }

    impl CapabilityGate for MockCapabilityGate {
        fn status(&self, capability: GatedCapability) -> CapabilityStatus {
            self.statuses
                .lock()
                .ok()
                .and_then(|m| m.get(&capability).cloned())
                .unwrap_or(CapabilityStatus::RequiresDependency {
                    dependency: ExternalDependency::Ollama {
                        install_command: "brew install ollama".to_string(),
                    },
                })
        }

        fn mark_ready(&self, capability: GatedCapability) {
            if let Ok(mut statuses) = self.statuses.lock() {
                statuses.insert(capability, CapabilityStatus::Ready);
            }
        }
    }

    impl LicenseGate for MockLicenseGate {
        fn is_pro(&self) -> bool {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(LicenseGateCall {
                    method: "is_pro".to_string(),
                });
            }
            self.is_pro_user
        }

        fn check(&self, feature: GatedFeature) -> std::result::Result<(), LicenseError> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(LicenseGateCall {
                    method: "check".to_string(),
                });
            }
            if !self.is_pro_user {
                return Err(LicenseError::ProRequired { feature });
            }
            if let Some(expires_at) = self.expired_at {
                if expires_at < Utc::now() {
                    return Err(LicenseError::Expired { expires_at });
                }
            }
            Ok(())
        }
    }

    #[test]
    fn mock_store_roundtrip() {
        let store = MockKnowledgeStore::default();
        let id = Uuid::new_v4();

        let unit = crate::KnowledgeUnit {
            id,
            source: crate::SourceId::new("obsidian"),
            content: "hello".to_string(),
            title: Some("note".to_string()),
            metadata: HashMap::new(),
            observed_at: Utc::now(),
            content_hash: [7; 32],
        };

        store.upsert(unit).expect("upsert should succeed");
        let got = store.get_by_id(id).expect("lookup should succeed");
        assert!(got.is_some());

        let _ = CapabilityStatus::RequiresSetup {
            action: crate::SetupAction::ConfigureVaultPath,
        };
        let _ = CapabilityStatus::RequiresPermission {
            permission: crate::MacOSPermission::Calendar,
        };
    }
}

// ==========================================
// Phase 9: Rule Engine & Noticer Policy Architecture
// ==========================================

// --- Rule Engine Traits ---

#[derive(Debug, Clone)]
pub struct PulseSnapshot {
    pub current: CognitiveState,
    pub history: Vec<CognitiveState>,
}

#[derive(Debug, Clone)]
pub struct EvalContext {
    pub rule: ProceduralRule,
    pub snapshot: PulseSnapshot,
}

pub trait PulseMatcher: Send + Sync {
    fn candidates(
        &self,
        store: &dyn KnowledgeStore,
        pulse: &PulseSnapshot,
    ) -> Result<Vec<ProceduralRule>>;
}

pub trait IntentScorer: Send + Sync {
    fn score(&self, current_lens: &[f32], rule: &ProceduralRule) -> f32;
}

pub trait ThresholdPolicy: Send + Sync {
    fn threshold(&self, rule: &ProceduralRule, ctx: &EvalContext) -> f32;
}

pub trait RuleExecutor: Send + Sync {
    fn execute(&self, rule: &ProceduralRule) -> Result<()>;
}

// --- Noticer Pipeline Traits ---

#[derive(Debug, Clone)]
pub struct NoticerState {
    pub last_run: Option<DateTime<Utc>>,
    pub new_events_count: usize,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub unit_id: Uuid,
    pub vector: Vec<f32>,
    pub cognitive_state: Option<CognitiveState>,
}

#[derive(Debug, Clone)]
pub struct Cluster {
    pub centroid: Vec<f32>,
    pub candidate_ids: Vec<Uuid>,
}

pub trait DiscoveryTrigger: Send + Sync {
    fn should_run(&self, state: &NoticerState) -> bool;
}

pub trait CandidateSelector: Send + Sync {
    fn select(
        &self,
        store: &dyn KnowledgeStore,
        index: &dyn LensIndexStore,
    ) -> Result<Vec<Candidate>>;
}

pub trait Clusterer: Send + Sync {
    fn cluster(&self, vecs: &[Vec<f32>]) -> Result<Vec<Cluster>>;
}

pub trait RuleSynthesizer: Send + Sync {
    fn synthesize(&self, cluster: &Cluster, units: &[KnowledgeUnit]) -> Result<ProceduralRule>;
}

pub trait RulePublisher: Send + Sync {
    fn publish(&self, rule: ProceduralRule, store: &dyn KnowledgeStore) -> Result<()>;
}
