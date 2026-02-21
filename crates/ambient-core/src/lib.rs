use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, CoreError>;

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
    pub embedding: Option<Vec<f32>>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    pub unit: KnowledgeUnit,
    pub score: f32,
    pub pulse_context: Option<Vec<PulseEvent>>,
    pub cognitive_state: Option<CognitiveState>,
    pub historical_feedback_score: f32,
    pub capability_status: Option<CapabilityStatus>,
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
    Local { ollama_base_url: String },
    Remote { provider: String },
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

#[derive(Debug, Clone, Default)]
pub struct WatchHandle;

#[derive(Debug, Clone, Default)]
pub struct SamplerHandle;

pub trait SourceAdapter: Send + Sync {
    fn source_id(&self) -> SourceId;
    fn watch(&self, tx: Sender<RawEvent>) -> Result<WatchHandle>;
}

pub trait PulseSampler: Send + Sync {
    fn start(&self, tx: Sender<PulseEvent>) -> Result<SamplerHandle>;
}

pub trait Normalizer: Send + Sync {
    fn can_handle(&self, payload: &RawPayload) -> bool;
    fn normalize(&self, event: RawEvent) -> Result<KnowledgeUnit>;
}

pub trait KnowledgeStore: Send + Sync {
    fn upsert(&self, unit: KnowledgeUnit) -> Result<()>;
    fn search_semantic(&self, query: &str, k: usize) -> Result<Vec<KnowledgeUnit>>;
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

    fn record_feedback(&self, event: FeedbackEvent) -> Result<()>;
    fn feedback_score(&self, unit_id: Uuid) -> Result<f32>;
    fn pattern_feedback(&self, pattern_id: Uuid) -> Result<Option<String>>;
}

pub trait ReasoningEngine: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn answer(&self, question: &str, context: &[KnowledgeUnit]) -> Result<String>;
}

pub trait QueryEngine: Send + Sync {
    fn query(&self, req: QueryRequest) -> Result<Vec<QueryResult>>;
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

#[cfg(any(test, feature = "test-mocks"))]
pub mod mocks {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use chrono::{DateTime, Timelike, Utc};
    use uuid::Uuid;

    use crate::{
        CapabilityGate, CapabilityStatus, CognitiveState, CoreError, ExternalDependency, FeedbackEvent,
        GatedCapability, GatedFeature, KnowledgeStore, LicenseError, LicenseGate, LoadAware,
        Normalizer, PulseEvent, PulseSampler, QueryEngine, QueryRequest, QueryResult, RawEvent,
        RawPayload, ReasoningEngine, Result, SamplerHandle, SourceAdapter, SourceId,
        SpotlightExporter, SystemLoad, TriggerAction, TriggerCondition, WatchHandle,
    };

    #[derive(Debug, Clone)]
    pub struct SourceAdapterCall {
        pub method: String,
    }

    pub struct MockSourceAdapter {
        pub source: SourceId,
        pub calls: Mutex<Vec<SourceAdapterCall>>,
    }

    impl Default for MockSourceAdapter {
        fn default() -> Self {
            Self {
                source: SourceId::new("mock"),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl SourceAdapter for MockSourceAdapter {
        fn source_id(&self) -> SourceId {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(SourceAdapterCall {
                    method: "source_id".to_string(),
                });
            }
            self.source.clone()
        }

        fn watch(&self, _tx: std::sync::mpsc::Sender<RawEvent>) -> Result<WatchHandle> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(SourceAdapterCall {
                    method: "watch".to_string(),
                });
            }
            Ok(WatchHandle)
        }
    }

    #[derive(Debug, Clone)]
    pub struct PulseSamplerCall {
        pub method: String,
    }

    #[derive(Default)]
    pub struct MockPulseSampler {
        pub calls: Mutex<Vec<PulseSamplerCall>>,
    }

    impl PulseSampler for MockPulseSampler {
        fn start(&self, _tx: std::sync::mpsc::Sender<PulseEvent>) -> Result<SamplerHandle> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(PulseSamplerCall {
                    method: "start".to_string(),
                });
            }
            Ok(SamplerHandle)
        }
    }

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

        fn search_semantic(&self, _query: &str, k: usize) -> Result<Vec<crate::KnowledgeUnit>> {
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
        fn evaluate(&self, _unit: &crate::KnowledgeUnit, _store: &dyn KnowledgeStore) -> Result<bool> {
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
            source: SourceId::new("obsidian"),
            content: "hello".to_string(),
            title: Some("note".to_string()),
            metadata: HashMap::new(),
            embedding: Some(vec![0.1, 0.2]),
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
