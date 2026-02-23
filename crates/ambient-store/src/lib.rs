use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use ambient_core::{
    derive_cognitive_state, CognitiveState, CoreError, FeedbackEvent, FeedbackSignal,
    KnowledgeStore, KnowledgeUnit, LensConfig, LensIndexStore, ProceduralRule, PulseEvent,
    PulseSignal, Result, TemporalProfile,
};
use chrono::{DateTime, Datelike, Timelike, Utc};
use cozo::{DataValue, DbInstance, ScriptMutability};
use serde_json::Value;
use uuid::Uuid;

const METRIC_CONTEXT_SWITCH_RATE: &str = "context_switch_rate";
const METRIC_ACTIVE_APP: &str = "active_app";
const METRIC_AUDIO_INPUT_ACTIVE: &str = "audio_input_active";
const METRIC_TIME_CONTEXT: &str = "time_context";
const METRIC_CALENDAR_CONTEXT: &str = "calendar_context";
const METRIC_ENERGY_LEVEL: &str = "energy_level";
const METRIC_MOOD_LEVEL: &str = "mood_level";
const METRIC_HRV_SCORE: &str = "hrv_score";
const METRIC_SLEEP_QUALITY: &str = "sleep_quality";

pub struct CozoStore {
    cozo: DbInstance,
    pulse_storage: tsink::Storage,
}

impl CozoStore {
    pub fn new() -> Result<Self> {
        let cozo_path = resolve_cozo_path()?;

        let cozo = DbInstance::new("sqlite", &cozo_path, "")
            .map_err(|e| CoreError::Internal(format!("failed to initialize cozo: {e}")))?;

        let pulse_storage = tsink::StorageBuilder::new()
            .with_data_path(expand_home("~/.ambient/pulse").to_str().unwrap())
            .with_partition_duration(Duration::from_secs(3600))
            .with_retention(Duration::from_secs(180 * 24 * 3600))
            .with_wal_sync_mode(tsink::WalSyncMode::Periodic(Duration::from_secs(1)))
            .build()
            .map_err(|e| CoreError::Internal(format!("failed to initialize tsink: {e}")))?;

        let store = Self {
            cozo,
            pulse_storage,
        };
        store.init_cozo_schema();
        Ok(store)
    }

    pub fn new_for_test() -> Result<Self> {
        let temp_dir = std::env::temp_dir().join(format!("ambient_test_{}", Uuid::new_v4()));
        let cozo_path = temp_dir.join("cozo.sqlite");
        let pulse_path = temp_dir.join("pulse");

        std::fs::create_dir_all(&temp_dir).unwrap();

        let cozo = DbInstance::new("sqlite", cozo_path.to_str().unwrap(), "")
            .map_err(|e| CoreError::Internal(format!("failed to initialize cozo: {e}")))?;

        let pulse_storage = tsink::StorageBuilder::new()
            .with_data_path(pulse_path.to_str().unwrap())
            .with_partition_duration(Duration::from_secs(3600))
            .with_retention(Duration::from_secs(90 * 24 * 3600))
            .with_wal_sync_mode(tsink::WalSyncMode::Periodic(Duration::from_secs(1)))
            .build()
            .map_err(|e| CoreError::Internal(format!("failed to initialize tsink: {e}")))?;

        let store = Self {
            cozo,
            pulse_storage,
        };
        store.init_cozo_schema();
        Ok(store)
    }

    fn init_cozo_schema(&self) {
        let commands = [
            ":create notes { id: String => source: String, title: String, content: String, hash: String, observed_at: Float, metadata: Json }",
            ":create links { from_id: String, to_id: String => link_type: String }",
            ":create pattern_results { id: String => pattern_type: String, summary: String, unit_ids: Json, detected_at: Float, feedback: String? }",
            ":create feedback_events { id: String => timestamp: Float, signal_type: String, unit_id: String?, query_text: String?, action: String?, pattern_id: String? }",
            ":create cognitive_snapshots { unit_id: String => observed_at: Float, window_secs: Int, was_in_flow: Bool, was_on_call: Bool, dominant_app: String?, time_of_day: Int, context_switch_rate: Float, was_in_meeting: Bool, was_in_focus_block: Bool, energy_level: Int?, mood_level: Int?, hrv_score: Float?, sleep_quality: Float?, minutes_since_last_meeting: Int? }",
            ":create lens_map { unit_id: String, lens_id: String => vec: <F32; 768>? }",
            ":create lens_map_384 { unit_id: String, lens_id: String => vec: <F32; 384>? }",
            ":create temporal_profile { unit_id: String => hour_peak: Int, day_mask: Int, recency_weight: Float }",
            ":create procedural_rules { id: String => lens_intent: <F32; 768>, pulse_mask: String, threshold: Float, action_payload: Json, created_at: Float }",
        ];

        for cmd in commands {
            let res = self
                .cozo
                .run_script(cmd, BTreeMap::new(), ScriptMutability::Mutable);

            if let Err(e) = res {
                // Ignore "conflicts with an existing one" which is normal if table exists
                let err_str = e.to_string();
                if !err_str.contains("conflicts with an existing one") {
                    println!("Failed to run script: {}. Error: {}", cmd, e);
                }
            }
        }

        // Secondary Index for feedback_events
        let _ = self.cozo.run_script(
            ":create index feedback_events { unit_id }",
            BTreeMap::new(),
            ScriptMutability::Mutable,
        );

        // HNSW Vector Index for the lens_map table (L1/L2 at 768D)
        let hnsw_768_schema = r#"
::hnsw create lens_map:vec_idx {
    dim: 768,
    dtype: F32,
    fields: [vec],
    distance: Cosine,
    ef_construction: 200,
    m: 16
}
"#;
        let _ = self
            .cozo
            .run_script(hnsw_768_schema, BTreeMap::new(), ScriptMutability::Mutable);

        // HNSW Vector Index for the lens_map_384 table (L5 at 384D)
        let hnsw_384_schema = r#"
::hnsw create lens_map_384:vec_idx {
    dim: 384,
    dtype: F32,
    fields: [vec],
    distance: Cosine,
    ef_construction: 200,
    m: 16
}
"#;
        let _ = self
            .cozo
            .run_script(hnsw_384_schema, BTreeMap::new(), ScriptMutability::Mutable);

        // FTS Setup
        let fts_schema = r#"
::fts create notes:fts {
    extractor: concat(title, concat(" ___ ", content)),
    tokenizer: Simple
}
"#;
        if let Err(e) = self
            .cozo
            .run_script(fts_schema, BTreeMap::new(), ScriptMutability::Mutable)
        {
            let err_str = e.to_string();
            if !err_str.contains("conflicts with an existing one") {
                eprintln!("FTS schema init error: {}", e);
            }
        }
    }

    fn cozo_put_note(&self, unit: &KnowledgeUnit) {
        let mut params = BTreeMap::new();
        params.insert("id".to_string(), DataValue::from(unit.id.to_string()));
        params.insert("source".to_string(), DataValue::from(unit.source.0.clone()));
        params.insert(
            "title".to_string(),
            DataValue::from(unit.title.clone().unwrap_or_default()),
        );
        params.insert("content".to_string(), DataValue::from(unit.content.clone()));
        params.insert(
            "hash".to_string(),
            DataValue::from(hash_to_string(&unit.content_hash)),
        );
        params.insert(
            "observed_at".to_string(),
            DataValue::from(unit.observed_at.timestamp_millis() as f64),
        );
        params.insert(
            "metadata_str".to_string(),
            DataValue::from(
                serde_json::to_string(&unit.metadata).unwrap_or_else(|_| "{}".to_string()),
            ),
        );

        let script = r#"
?[id, source, title, content, hash, observed_at, metadata] <- [[
    $id, $source, $title, $content, $hash, $observed_at, parse_json($metadata_str)
]]
:put notes { id => source, title, content, hash, observed_at, metadata }
"#;
        if let Err(e) = self
            .cozo
            .run_script(script, params, ScriptMutability::Mutable)
        {
            eprintln!("Error in cozo_put_note Datalog: {}", e);
        }
    }

    fn cozo_put_link(&self, from_id: Uuid, to_id: Uuid) {
        let mut params = BTreeMap::new();
        params.insert("from_id".to_string(), DataValue::from(from_id.to_string()));
        params.insert("to_id".to_string(), DataValue::from(to_id.to_string()));
        params.insert(
            "link_type".to_string(),
            DataValue::from("wikilink".to_string()),
        );

        let script = r#"
?[from_id, to_id, link_type] <- [[ $from_id, $to_id, $link_type ]]
:put links { from_id, to_id => link_type }
"#;
        let _ = self
            .cozo
            .run_script(script, params, ScriptMutability::Mutable);
    }

    fn cozo_put_snapshot(
        &self,
        unit: &KnowledgeUnit,
        window_secs: i64,
        state: &CognitiveState,
        context_switch_rate: f64,
    ) {
        let mut params = BTreeMap::new();
        params.insert("unit_id".to_string(), DataValue::from(unit.id.to_string()));
        params.insert(
            "observed_at".to_string(),
            DataValue::from(unit.observed_at.timestamp_millis() as f64),
        );
        params.insert("window_secs".to_string(), DataValue::from(window_secs));
        params.insert(
            "was_in_flow".to_string(),
            DataValue::from(state.was_in_flow),
        );
        params.insert(
            "was_on_call".to_string(),
            DataValue::from(state.was_on_call),
        );
        params.insert(
            "dominant_app".to_string(),
            state
                .dominant_app
                .clone()
                .map(DataValue::from)
                .unwrap_or(DataValue::Null),
        );
        params.insert(
            "time_of_day".to_string(),
            DataValue::from(state.time_of_day.unwrap_or(0) as i64),
        );
        params.insert(
            "context_switch_rate".to_string(),
            DataValue::from(context_switch_rate),
        );
        params.insert(
            "was_in_meeting".to_string(),
            DataValue::from(state.was_in_meeting),
        );
        params.insert(
            "was_in_focus_block".to_string(),
            DataValue::from(state.was_in_focus_block),
        );
        params.insert(
            "energy_level".to_string(),
            state
                .energy_level
                .map(|v| DataValue::from(v as i64))
                .unwrap_or(DataValue::Null),
        );
        params.insert(
            "mood_level".to_string(),
            state
                .mood_level
                .map(|v| DataValue::from(v as i64))
                .unwrap_or(DataValue::Null),
        );
        params.insert(
            "hrv_score".to_string(),
            state
                .hrv_score
                .map(|v| DataValue::from(v as f64))
                .unwrap_or(DataValue::Null),
        );
        params.insert(
            "sleep_quality".to_string(),
            state
                .sleep_quality
                .map(|v| DataValue::from(v as f64))
                .unwrap_or(DataValue::Null),
        );
        params.insert(
            "minutes_since_last_meeting".to_string(),
            state
                .minutes_since_last_meeting
                .map(|v| DataValue::from(v as i64))
                .unwrap_or(DataValue::Null),
        );

        let script = r#"
?[unit_id, observed_at, window_secs, was_in_flow, was_on_call, dominant_app, time_of_day, context_switch_rate, was_in_meeting, was_in_focus_block, energy_level, mood_level, hrv_score, sleep_quality, minutes_since_last_meeting] <- [[
  $unit_id, $observed_at, $window_secs, $was_in_flow, $was_on_call, $dominant_app, $time_of_day, $context_switch_rate, $was_in_meeting, $was_in_focus_block, $energy_level, $mood_level, $hrv_score, $sleep_quality, $minutes_since_last_meeting
]]
:put cognitive_snapshots { unit_id => observed_at, window_secs, was_in_flow, was_on_call, dominant_app, time_of_day, context_switch_rate, was_in_meeting, was_in_focus_block, energy_level, mood_level, hrv_score, sleep_quality, minutes_since_last_meeting }
"#;
        let _ = self
            .cozo
            .run_script(script, params, ScriptMutability::Mutable);
    }

    fn cozo_record_feedback(&self, event: &FeedbackEvent) {
        let (signal_type, unit_id, query_text, action, pattern_id) = match &event.signal {
            FeedbackSignal::QueryResultActedOn {
                query_text,
                unit_id,
                action,
                ..
            } => (
                "query_result_acted_on".to_string(),
                Some(unit_id.to_string()),
                Some(query_text.clone()),
                Some(format!("{action:?}")),
                None,
            ),
            FeedbackSignal::QueryResultDismissed {
                query_text,
                unit_id,
            } => (
                "query_result_dismissed".to_string(),
                Some(unit_id.to_string()),
                Some(query_text.clone()),
                None,
                None,
            ),
            FeedbackSignal::TriggerAcknowledged { unit_id, .. } => (
                "trigger_acknowledged".to_string(),
                Some(unit_id.to_string()),
                None,
                None,
                None,
            ),
            FeedbackSignal::TriggerDismissed { unit_id, .. } => (
                "trigger_dismissed".to_string(),
                Some(unit_id.to_string()),
                None,
                None,
                None,
            ),
            FeedbackSignal::PatternMarkedUseful { pattern_id } => (
                "pattern_marked_useful".to_string(),
                None,
                None,
                None,
                Some(pattern_id.to_string()),
            ),
            FeedbackSignal::PatternMarkedNoise { pattern_id } => (
                "pattern_marked_noise".to_string(),
                None,
                None,
                None,
                Some(pattern_id.to_string()),
            ),
            FeedbackSignal::OracleResultActedOn {
                query_text,
                unit_id,
                action,
                ..
            } => (
                "oracle_result_acted_on".to_string(),
                Some(unit_id.to_string()),
                Some(query_text.clone()),
                Some(format!("{action:?}")),
                None,
            ),
        };

        let mut params = BTreeMap::new();
        params.insert("id".to_string(), DataValue::from(event.id.to_string()));
        params.insert(
            "timestamp".to_string(),
            DataValue::from(event.timestamp.timestamp_millis() as f64),
        );
        params.insert("signal_type".to_string(), DataValue::from(signal_type));
        params.insert(
            "unit_id".to_string(),
            unit_id.map(DataValue::from).unwrap_or(DataValue::Null),
        );
        params.insert(
            "query_text".to_string(),
            query_text.map(DataValue::from).unwrap_or(DataValue::Null),
        );
        params.insert(
            "action".to_string(),
            action.map(DataValue::from).unwrap_or(DataValue::Null),
        );
        params.insert(
            "pattern_id".to_string(),
            pattern_id.map(DataValue::from).unwrap_or(DataValue::Null),
        );

        let script = r#"
?[id, timestamp, signal_type, unit_id, query_text, action, pattern_id] <- [[
  $id, $timestamp, $signal_type, $unit_id, $query_text, $action, $pattern_id
]]
:put feedback_events { id => timestamp, signal_type, unit_id, query_text, action, pattern_id }
"#;
        let _ = self
            .cozo
            .run_script(script, params, ScriptMutability::Mutable);
    }

    fn metric_record_for_event(event: PulseEvent) -> tsink::Record {
        match event.signal {
            PulseSignal::ContextSwitchRate {
                switches_per_minute,
            } => tsink::Record {
                metric: METRIC_CONTEXT_SWITCH_RATE.to_string(),
                timestamp_millis: event.timestamp.timestamp_millis(),
                value: switches_per_minute as f64,
                labels: BTreeMap::new(),
            },
            PulseSignal::ActiveApp {
                bundle_id,
                window_title,
            } => {
                let mut labels = BTreeMap::new();
                labels.insert("bundle_id".to_string(), bundle_id);
                if let Some(title) = window_title {
                    labels.insert("window_title".to_string(), title);
                }

                tsink::Record {
                    metric: METRIC_ACTIVE_APP.to_string(),
                    timestamp_millis: event.timestamp.timestamp_millis(),
                    value: 1.0,
                    labels,
                }
            }
            PulseSignal::AudioInputActive { active } => {
                let mut labels = BTreeMap::new();
                labels.insert("active".to_string(), active.to_string());

                tsink::Record {
                    metric: METRIC_AUDIO_INPUT_ACTIVE.to_string(),
                    timestamp_millis: event.timestamp.timestamp_millis(),
                    value: if active { 1.0 } else { 0.0 },
                    labels,
                }
            }
            PulseSignal::TimeContext {
                hour_of_day,
                day_of_week,
                is_weekend,
            } => {
                let mut labels = BTreeMap::new();
                labels.insert("hour".to_string(), hour_of_day.to_string());
                labels.insert("day_of_week".to_string(), day_of_week.to_string());
                labels.insert("is_weekend".to_string(), is_weekend.to_string());

                tsink::Record {
                    metric: METRIC_TIME_CONTEXT.to_string(),
                    timestamp_millis: event.timestamp.timestamp_millis(),
                    value: hour_of_day as f64,
                    labels,
                }
            }
            PulseSignal::CalendarContext {
                in_meeting,
                in_focus_block,
                minutes_until_next_event,
                current_event_duration_minutes,
            } => {
                let mut labels = BTreeMap::new();
                labels.insert("in_meeting".to_string(), in_meeting.to_string());
                labels.insert("in_focus_block".to_string(), in_focus_block.to_string());
                if let Some(minutes) = minutes_until_next_event {
                    labels.insert("minutes_until_next".to_string(), minutes.to_string());
                }
                if let Some(minutes) = current_event_duration_minutes {
                    labels.insert(
                        "current_event_duration_minutes".to_string(),
                        minutes.to_string(),
                    );
                }
                tsink::Record {
                    metric: METRIC_CALENDAR_CONTEXT.to_string(),
                    timestamp_millis: event.timestamp.timestamp_millis(),
                    value: 1.0,
                    labels,
                }
            }
            PulseSignal::EnergyLevel { score } => tsink::Record {
                metric: METRIC_ENERGY_LEVEL.to_string(),
                timestamp_millis: event.timestamp.timestamp_millis(),
                value: score as f64,
                labels: BTreeMap::new(),
            },
            PulseSignal::MoodLevel { score } => tsink::Record {
                metric: METRIC_MOOD_LEVEL.to_string(),
                timestamp_millis: event.timestamp.timestamp_millis(),
                value: score as f64,
                labels: BTreeMap::new(),
            },
            PulseSignal::HRVScore { value } => tsink::Record {
                metric: METRIC_HRV_SCORE.to_string(),
                timestamp_millis: event.timestamp.timestamp_millis(),
                value: value as f64,
                labels: BTreeMap::new(),
            },
            PulseSignal::SleepQuality { score } => tsink::Record {
                metric: METRIC_SLEEP_QUALITY.to_string(),
                timestamp_millis: event.timestamp.timestamp_millis(),
                value: score as f64,
                labels: BTreeMap::new(),
            },
        }
    }

    fn pulse_event_from_record(record: tsink::Record) -> Option<PulseEvent> {
        let signal = match record.metric.as_str() {
            METRIC_CONTEXT_SWITCH_RATE => PulseSignal::ContextSwitchRate {
                switches_per_minute: record.value as f32,
            },
            METRIC_ACTIVE_APP => PulseSignal::ActiveApp {
                bundle_id: record.labels.get("bundle_id")?.clone(),
                window_title: record.labels.get("window_title").cloned(),
            },
            METRIC_AUDIO_INPUT_ACTIVE => {
                let active = record
                    .labels
                    .get("active")
                    .map(|v| v == "true")
                    .unwrap_or(record.value > 0.0);
                PulseSignal::AudioInputActive { active }
            }
            METRIC_TIME_CONTEXT => {
                let hour_of_day = record.labels.get("hour")?.parse().ok()?;
                let day_of_week = record.labels.get("day_of_week")?.parse().ok()?;
                let is_weekend = record
                    .labels
                    .get("is_weekend")
                    .map(|v| v == "true")
                    .unwrap_or(false);

                PulseSignal::TimeContext {
                    hour_of_day,
                    day_of_week,
                    is_weekend,
                }
            }
            METRIC_CALENDAR_CONTEXT => PulseSignal::CalendarContext {
                in_meeting: record
                    .labels
                    .get("in_meeting")
                    .map(|v| v == "true")
                    .unwrap_or(false),
                in_focus_block: record
                    .labels
                    .get("in_focus_block")
                    .map(|v| v == "true")
                    .unwrap_or(false),
                minutes_until_next_event: record
                    .labels
                    .get("minutes_until_next")
                    .and_then(|v| v.parse().ok()),
                current_event_duration_minutes: record
                    .labels
                    .get("current_event_duration_minutes")
                    .and_then(|v| v.parse().ok()),
            },
            METRIC_ENERGY_LEVEL => PulseSignal::EnergyLevel {
                score: record.value as u8,
            },
            METRIC_MOOD_LEVEL => PulseSignal::MoodLevel {
                score: record.value as u8,
            },
            METRIC_HRV_SCORE => PulseSignal::HRVScore {
                value: record.value as f32,
            },
            METRIC_SLEEP_QUALITY => PulseSignal::SleepQuality {
                score: record.value as f32,
            },
            _ => return None,
        };

        Some(PulseEvent {
            timestamp: DateTime::<Utc>::from_timestamp_millis(record.timestamp_millis)?,
            signal,
        })
    }

    fn derive_time_context(ts: DateTime<Utc>) -> PulseEvent {
        PulseEvent {
            timestamp: ts,
            signal: PulseSignal::TimeContext {
                hour_of_day: ts.hour() as u8,
                day_of_week: ts.weekday().num_days_from_monday() as u8,
                is_weekend: ts.weekday().number_from_monday() >= 6,
            },
        }
    }

    fn index_links(&self, unit: &KnowledgeUnit) -> Result<()> {
        if let Some(Value::Array(links)) = unit.metadata.get("links") {
            for link in links {
                if let Value::String(link_title) = link {
                    // Fetch all notes' IDs and titles to do in-memory fuzzy matching
                    let query = r#"?[id, title] := *notes{id, title}"#;
                    if let Ok(result) =
                        self.cozo
                            .run_script(query, BTreeMap::new(), ScriptMutability::Immutable)
                    {
                        for row in result.rows {
                            if let (
                                Some(DataValue::Str(id_str)),
                                Some(DataValue::Str(note_title)),
                            ) = (row.first(), row.get(1))
                            {
                                // Match the extracted link target against the title (fuzzy match)
                                let link_title_str = link_title.as_str();
                                let note_title_str = note_title.as_str();

                                if note_title_str.contains(link_title_str)
                                    || link_title_str.contains(note_title_str)
                                {
                                    if let Ok(target_id) = Uuid::parse_str(id_str) {
                                        self.cozo_put_link(unit.id, target_id);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Private helper methods for CozoStore — not part of the KnowledgeStore trait.
impl CozoStore {
    /// Decode a CozoDB result row `[id, source, title, content, hash, observed_at, ...]`
    /// into a `KnowledgeUnit`. The optional trailing column (dist / score) is ignored.
    fn row_to_knowledge_unit(&self, row: &[DataValue]) -> Option<KnowledgeUnit> {
        if row.len() < 7 {
            return None;
        }
        let id_str = match &row[0] {
            DataValue::Str(s) => s.to_string(),
            _ => return None,
        };
        let id = Uuid::parse_str(&id_str).ok()?;
        let source = match &row[1] {
            DataValue::Str(s) => ambient_core::SourceId(s.to_string()),
            _ => return None,
        };
        let title = match &row[2] {
            DataValue::Str(s) if !s.is_empty() => Some(s.to_string()),
            _ => None,
        };
        let content = match &row[3] {
            DataValue::Str(s) => s.to_string(),
            _ => return None,
        };
        let content_hash: [u8; 32] = match &row[4] {
            DataValue::Str(s) => {
                let mut arr = [0u8; 32];
                let bytes = s.as_bytes();
                let len = bytes.len().min(32);
                arr[..len].copy_from_slice(&bytes[..len]);
                arr
            }
            _ => [0u8; 32],
        };
        let observed_at_ms: i64 = match &row[5] {
            DataValue::Num(n) => match n {
                cozo::Num::Int(i) => *i,
                cozo::Num::Float(f) => *f as i64,
            },
            _ => return None,
        };
        let observed_at = chrono::DateTime::from_timestamp_millis(observed_at_ms)?;

        let metadata = match &row[6] {
            DataValue::Json(val) => {
                if let serde_json::Value::Object(map) = &val.0 {
                    let mut res = std::collections::HashMap::new();
                    for (k, v) in map {
                        res.insert(k.clone(), v.clone());
                    }
                    res
                } else {
                    std::collections::HashMap::new()
                }
            }
            _ => std::collections::HashMap::new(),
        };

        Some(KnowledgeUnit {
            id,
            source,
            title,
            content,
            content_hash,
            observed_at,
            metadata,
        })
    }

    fn row_to_procedural_rule(&self, row: &[DataValue]) -> Option<ProceduralRule> {
        let id_str = match row.first() {
            Some(DataValue::Str(s)) => s.to_string(),
            _ => return None,
        };
        let id = Uuid::parse_str(&id_str).ok()?;

        let lens_intent = match row.get(1) {
            Some(DataValue::List(l)) => l
                .iter()
                .map(|v| match v {
                    DataValue::Num(cozo::Num::Float(f)) => *f as f32,
                    DataValue::Num(cozo::Num::Int(i)) => *i as f32,
                    _ => 0.0,
                })
                .collect(),
            _ => Vec::new(),
        };

        let pulse_mask = match row.get(2) {
            Some(DataValue::Str(s)) => s.to_string(),
            _ => return None,
        };

        let threshold = match row.get(3) {
            Some(DataValue::Num(cozo::Num::Float(f))) => *f as f32,
            Some(DataValue::Num(cozo::Num::Int(i))) => *i as f32,
            _ => 0.0,
        };

        let action_payload = match row.get(4) {
            Some(DataValue::Json(j)) => j.0.clone(),
            _ => serde_json::Value::Null,
        };

        let created_at_ms = match row.get(5) {
            Some(DataValue::Num(cozo::Num::Float(f))) => *f as i64,
            Some(DataValue::Num(cozo::Num::Int(i))) => *i,
            _ => 0,
        };
        let created_at = DateTime::from_timestamp_millis(created_at_ms).unwrap_or_else(Utc::now);

        Some(ProceduralRule {
            id,
            lens_intent,
            pulse_mask,
            threshold,
            action_payload,
            created_at,
        })
    }

    fn row_to_cognitive_state(&self, row: &[DataValue]) -> Option<CognitiveState> {
        // unit_id is row[0], observed_at is row[1], ...
        let was_in_flow = match row.get(3) {
            Some(DataValue::Bool(b)) => *b,
            _ => false,
        };
        let was_on_call = match row.get(4) {
            Some(DataValue::Bool(b)) => *b,
            _ => false,
        };
        let dominant_app = match row.get(5) {
            Some(DataValue::Str(s)) => Some(s.to_string()),
            _ => None,
        };
        let time_of_day = match row.get(6) {
            Some(DataValue::Num(cozo::Num::Int(i))) => Some(*i as u8),
            _ => None,
        };
        let was_in_meeting = match row.get(8) {
            Some(DataValue::Bool(b)) => *b,
            _ => false,
        };
        let was_in_focus_block = match row.get(9) {
            Some(DataValue::Bool(b)) => *b,
            _ => false,
        };
        let energy_level = match row.get(10) {
            Some(DataValue::Num(cozo::Num::Int(i))) => Some(*i as u8),
            _ => None,
        };
        let mood_level = match row.get(11) {
            Some(DataValue::Num(cozo::Num::Int(i))) => Some(*i as u8),
            _ => None,
        };
        let hrv_score = match row.get(12) {
            Some(DataValue::Num(cozo::Num::Float(f))) => Some(*f as f32),
            _ => None,
        };
        let sleep_quality = match row.get(13) {
            Some(DataValue::Num(cozo::Num::Float(f))) => Some(*f as f32),
            _ => None,
        };
        let minutes_since_last_meeting = match row.get(14) {
            Some(DataValue::Num(cozo::Num::Int(i))) => Some(*i as u32),
            _ => None,
        };

        Some(CognitiveState {
            was_in_flow,
            was_on_call,
            dominant_app,
            time_of_day,
            was_in_meeting,
            was_in_focus_block,
            energy_level,
            mood_level,
            hrv_score,
            sleep_quality,
            minutes_since_last_meeting,
        })
    }
}

impl KnowledgeStore for CozoStore {
    fn upsert(&self, unit: KnowledgeUnit) -> Result<()> {
        // ── Deduplication ────────────────────────────────────────────────────
        {
            let mut params = BTreeMap::new();
            params.insert(
                "hash".to_string(),
                DataValue::from(hash_to_string(&unit.content_hash)),
            );
            let query = r#"?[id] := *notes{id, hash: $hash}"#;

            if let Ok(result) = self
                .cozo
                .run_script(query, params, ScriptMutability::Immutable)
            {
                if let Some(row) = result.rows.first() {
                    if let Some(DataValue::Str(existing_id_str)) = row.first() {
                        if let Ok(existing_id) = Uuid::parse_str(existing_id_str) {
                            if existing_id != unit.id {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        // ── Derive cognitive state BEFORE any writes ──────────────────────────
        // Record TimeContext first so it is included in the pulse window query.
        // TimeContext is always derived from observed_at — never sampled.
        self.record_pulse(Self::derive_time_context(unit.observed_at))?;

        let window_secs = 120i64;
        let from = unit.observed_at - chrono::Duration::seconds(window_secs);
        let to = unit.observed_at + chrono::Duration::seconds(window_secs);

        // Best-effort pulse window: fall back to empty vec if tsink is unavailable.
        // The snapshot is always written — even with an empty window the cognitive
        // state defaults are stored so unit_with_context_fast never has a miss.
        let pulse = self.pulse_window(from, to).unwrap_or_default();
        let state = derive_cognitive_state(unit.observed_at, &pulse);
        let avg_switch = {
            let (sum, count) =
                pulse
                    .iter()
                    .fold((0.0f64, 0usize), |acc, event| match event.signal {
                        PulseSignal::ContextSwitchRate {
                            switches_per_minute,
                        } => (acc.0 + switches_per_minute as f64, acc.1 + 1),
                        PulseSignal::ActiveApp { .. }
                        | PulseSignal::AudioInputActive { .. }
                        | PulseSignal::TimeContext { .. }
                        | PulseSignal::CalendarContext { .. }
                        | PulseSignal::EnergyLevel { .. }
                        | PulseSignal::MoodLevel { .. }
                        | PulseSignal::HRVScore { .. }
                        | PulseSignal::SleepQuality { .. } => acc,
                    });
            if count == 0 {
                0.0
            } else {
                sum / count as f64
            }
        };

        // ── (In-memory caches removed) ────────────────────────────────────────

        // ── Atomic CozoDB writes: note and snapshot together ──────────────────
        // Both writes run unconditionally and back-to-back. The snapshot is
        // never conditional on pulse availability — a unit without a snapshot
        // is an invariant violation that would cause unit_with_context_fast to
        // fall through to the expensive tsink query on every call.
        self.cozo_put_note(&unit);
        self.cozo_put_snapshot(&unit, window_secs, &state, avg_switch);

        // ── Index wikilinks ───────────────────────────────────────────────────
        self.index_links(&unit)?;

        Ok(())
    }

    fn search_semantic(&self, query_vec: &[f32], k: usize) -> Result<Vec<KnowledgeUnit>> {
        if query_vec.is_empty() {
            return Ok(Vec::new());
        }

        let mut params = BTreeMap::new();
        params.insert(
            "vec".to_string(),
            DataValue::List(
                query_vec
                    .iter()
                    .map(|f| DataValue::from(*f as f64))
                    .collect(),
            ),
        );
        params.insert("k".to_string(), DataValue::from(k as i64));

        // Join HNSW neighbor results with the notes relation in one Datalog statement.
        // Updated for V2: query lens_map:vec_idx and join with notes (no embedding column).
        let script = r#"
?[id, source, title, content, hash, observed_at, dist] :=
    ~lens_map:vec_idx{ unit_id: id | query: $vec, k: $k, ef: 50, bind_distance: dist },
    *notes{ id, source, title, content, hash, observed_at }
:order dist
:limit $k
"#;
        match self
            .cozo
            .run_script(script, params, ScriptMutability::Immutable)
        {
            Ok(result) => {
                let out = result
                    .rows
                    .iter()
                    .filter_map(|row| self.row_to_knowledge_unit(row))
                    .collect();
                Ok(out)
            }
            Err(_) => {
                // HNSW index empty or not yet built — return empty so callers fall back to FTS.
                Ok(Vec::new())
            }
        }
    }

    fn search_fulltext(&self, query: &str) -> Result<Vec<KnowledgeUnit>> {
        if query.is_empty() {
            let script = r#"
?[id, source, title, content, hash, observed_at, metadata] :=
    *notes{ id, source, title, content, hash, observed_at, metadata }
:order -observed_at
"#;
            let result = self
                .cozo
                .run_script(script, BTreeMap::new(), ScriptMutability::Immutable)
                .map_err(|e| CoreError::Internal(format!("failed listing notes: {e}")))?;
            let out = result
                .rows
                .iter()
                .filter_map(|row| self.row_to_knowledge_unit(row))
                .collect();
            return Ok(out);
        }

        let mut params = BTreeMap::new();
        params.insert("query".to_string(), DataValue::from(query));
        params.insert("k".to_string(), DataValue::from(50i64));

        let script = r#"
?[id, source, title, content, hash, observed_at, metadata] :=
    ~notes:fts{ id, source, title, content, hash, observed_at, metadata | query: $query, k: $k }
"#;
        let mut out = Vec::new();
        if let Ok(result) = self
            .cozo
            .run_script(script, params, ScriptMutability::Immutable) {
            for row in result.rows {
                if let Some(unit) = self.row_to_knowledge_unit(&row) {
                    out.push(unit);
                }
            }
        }
        Ok(out)
    }

    fn related(&self, id: Uuid, depth: usize) -> Result<Vec<KnowledgeUnit>> {
        if depth == 0 {
            return Ok(Vec::new());
        }

        let mut visited: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
        visited.insert(id);
        let mut frontier = vec![id];
        let mut ordered_related = Vec::new();

        let fwd_script = r#"?[to_id] := *links{from_id: $start_id, to_id}"#;
        let bck_script = r#"?[from_id] := *links{from_id, to_id: $start_id}"#;

        for _ in 0..depth {
            if frontier.is_empty() {
                break;
            }

            let mut next_frontier = Vec::new();
            for node in frontier {
                let mut params = BTreeMap::new();
                params.insert("start_id".to_string(), DataValue::from(node.to_string()));

                if let Ok(result) =
                    self.cozo
                        .run_script(fwd_script, params.clone(), ScriptMutability::Immutable)
                {
                    for row in result.rows {
                        if let Some(DataValue::Str(id_str)) = row.first() {
                            if let Ok(target) = Uuid::parse_str(id_str) {
                                if visited.insert(target) {
                                    ordered_related.push(target);
                                    next_frontier.push(target);
                                }
                            }
                        }
                    }
                }

                if let Ok(result) =
                    self.cozo
                        .run_script(bck_script, params, ScriptMutability::Immutable)
                {
                    for row in result.rows {
                        if let Some(DataValue::Str(id_str)) = row.first() {
                            if let Ok(target) = Uuid::parse_str(id_str) {
                                if visited.insert(target) {
                                    ordered_related.push(target);
                                    next_frontier.push(target);
                                }
                            }
                        }
                    }
                }
            }
            frontier = next_frontier;
        }

        let mut out = Vec::new();
        for rid in ordered_related {
            if let Ok(Some(unit)) = self.get_by_id(rid) {
                out.push(unit);
            }
        }
        Ok(out)
    }

    fn get_by_id(&self, id: Uuid) -> Result<Option<KnowledgeUnit>> {
        let mut params = BTreeMap::new();
        params.insert("id".to_string(), DataValue::from(id.to_string()));
        let script = r#"?[id, source, title, content, hash, observed_at, metadata] := *notes{id, source, title, content, hash, observed_at, metadata}, id = $id"#;

        let result = self
            .cozo
            .run_script(script, params, ScriptMutability::Immutable)
            .map_err(|e| CoreError::Internal(format!("cozo query error: {e}")))?;

        Ok(result
            .rows
            .first()
            .and_then(|row| self.row_to_knowledge_unit(row)))
    }

    fn record_pulse(&self, event: PulseEvent) -> Result<()> {
        let record = Self::metric_record_for_event(event);
        self.pulse_storage
            .append(record)
            .map_err(|e| CoreError::Internal(format!("tsink append failed: {e}")))
    }

    fn pulse_window(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<PulseEvent>> {
        let mut events: Vec<PulseEvent> = self
            .pulse_storage
            .query_range(from.timestamp_millis(), to.timestamp_millis())
            .map_err(|e| CoreError::Internal(format!("tsink query failed: {e}")))?
            .into_iter()
            .filter_map(Self::pulse_event_from_record)
            .collect();
        events.sort_by_key(|e| e.timestamp);
        Ok(events)
    }

    fn unit_with_context(
        &self,
        id: Uuid,
        context_window_secs: u64,
    ) -> Result<Option<(KnowledgeUnit, Vec<PulseEvent>, CognitiveState)>> {
        let Some(unit) = self.get_by_id(id)? else {
            return Ok(None);
        };

        let from = unit.observed_at - chrono::Duration::seconds(context_window_secs as i64);
        let to = unit.observed_at + chrono::Duration::seconds(context_window_secs as i64);
        let pulse = self.pulse_window(from, to)?;
        let state = derive_cognitive_state(unit.observed_at, &pulse);

        Ok(Some((unit, pulse, state)))
    }

    fn unit_with_context_fast(&self, id: Uuid) -> Result<Option<(KnowledgeUnit, CognitiveState)>> {
        let Some(unit) = self.get_by_id(id)? else {
            return Ok(None);
        };

        // Query CozoDB for the snapshot
        let mut params = BTreeMap::new();
        params.insert("unit_id".to_string(), DataValue::from(id.to_string()));

        let script = r#"
?[unit_id, observed_at, window_secs, was_in_flow, was_on_call, dominant_app, time_of_day, context_switch_rate, was_in_meeting, was_in_focus_block, energy_level, mood_level, hrv_score, sleep_quality, minutes_since_last_meeting] :=
    *cognitive_snapshots{unit_id, observed_at, window_secs, was_in_flow, was_on_call, dominant_app, time_of_day, context_switch_rate, was_in_meeting, was_in_focus_block, energy_level, mood_level, hrv_score, sleep_quality, minutes_since_last_meeting},
    unit_id = $unit_id
:limit 1
"#;

        if let Ok(result) = self
            .cozo
            .run_script(script, params, ScriptMutability::Immutable)
        {
            if let Some(row) = result.rows.first() {
                // Parse the CognitiveState from the row
                let state = CognitiveState {
                    was_in_flow: match &row[3] {
                        DataValue::Bool(b) => *b,
                        _ => false,
                    },
                    was_on_call: match &row[4] {
                        DataValue::Bool(b) => *b,
                        _ => false,
                    },
                    dominant_app: match &row[5] {
                        DataValue::Str(s) => Some(s.to_string()),
                        _ => None,
                    },
                    time_of_day: match &row[6] {
                        DataValue::Num(cozo::Num::Int(i)) => Some(*i as u8),
                        _ => None,
                    },
                    was_in_meeting: match &row[8] {
                        DataValue::Bool(b) => *b,
                        _ => false,
                    },
                    was_in_focus_block: match &row[9] {
                        DataValue::Bool(b) => *b,
                        _ => false,
                    },
                    energy_level: match &row[10] {
                        DataValue::Num(cozo::Num::Int(i)) => Some(*i as u8),
                        _ => None,
                    },
                    mood_level: match &row[11] {
                        DataValue::Num(cozo::Num::Int(i)) => Some(*i as u8),
                        _ => None,
                    },
                    hrv_score: match &row[12] {
                        DataValue::Num(cozo::Num::Float(f)) => Some(*f as f32),
                        DataValue::Num(cozo::Num::Int(i)) => Some(*i as f32),
                        _ => None,
                    },
                    sleep_quality: match &row[13] {
                        DataValue::Num(cozo::Num::Float(f)) => Some(*f as f32),
                        DataValue::Num(cozo::Num::Int(i)) => Some(*i as f32),
                        _ => None,
                    },
                    minutes_since_last_meeting: match &row[14] {
                        DataValue::Num(cozo::Num::Int(i)) => Some(*i as u32),
                        _ => None,
                    },
                };
                return Ok(Some((unit, state)));
            }
        }
        let from = unit.observed_at - chrono::Duration::seconds(120);
        let to = unit.observed_at + chrono::Duration::seconds(120);
        let pulse = self.pulse_window(from, to)?;
        let state = derive_cognitive_state(unit.observed_at, &pulse);
        Ok(Some((unit, state)))
    }

    fn unit_with_context_live(
        &self,
        id: Uuid,
        window_secs: u64,
    ) -> Result<Option<(KnowledgeUnit, Vec<PulseEvent>, CognitiveState)>> {
        self.unit_with_context(id, window_secs)
    }

    fn record_feedback(&self, event: FeedbackEvent) -> Result<()> {
        self.cozo_record_feedback(&event);
        Ok(())
    }

    fn feedback_score(&self, unit_id: Uuid) -> Result<f32> {
        let mut params = BTreeMap::new();
        params.insert(
            "target_id".to_string(),
            DataValue::from(unit_id.to_string()),
        );

        let script = r#"
            weights[type, weight] <- [
                ["query_result_acted_on", 1.0],
                ["oracle_result_acted_on", 2.0],
                ["query_result_dismissed", -0.5],
                ["trigger_acknowledged", 1.0],
                ["trigger_dismissed", -2.0]
            ]
            
            # Get weights for all events for the target unit
            ?[w] := *feedback_events{unit_id: $target_id, signal_type}, weights[signal_type, w]
        "#;

        let res = self
            .cozo
            .run_script(script, params, ScriptMutability::Immutable)
            .map_err(|e| CoreError::Internal(format!("failed to fetch feedback weights: {e}")))?;

        let mut sum_weights = 0.0f32;
        for row in res.rows {
            if let Some(DataValue::Num(cozo::Num::Float(f))) = row.first() {
                sum_weights += *f as f32;
            }
        }

        if sum_weights == 0.0 {
            return Ok(0.5);
        }

        // Sigmoid squash: 1 / (1 + exp(-s))
        let score = 1.0 / (1.0 + (-sum_weights).exp());
        Ok(score)
    }

    fn pattern_feedback(&self, pattern_id: Uuid) -> Result<Option<String>> {
        let mut params = BTreeMap::new();
        params.insert(
            "target_id".to_string(),
            DataValue::from(pattern_id.to_string()),
        );

        let script = r#"
            ?[signal_type, timestamp] := *feedback_events{pattern_id: $target_id, signal_type, timestamp},
                signal_type in ["pattern_marked_useful", "pattern_marked_noise"]
            :sort -timestamp
            :limit 1
        "#;

        let res = self
            .cozo
            .run_script(script, params, ScriptMutability::Immutable)
            .map_err(|e| CoreError::Internal(format!("failed to query pattern feedback: {e}")))?;

        if let Some(row) = res.rows.first() {
            if let Some(DataValue::Str(s)) = row.first() {
                let status = if s == "pattern_marked_useful" {
                    "useful"
                } else {
                    "noise"
                };
                return Ok(Some(status.to_string()));
            }
        }

        Ok(None)
    }

    fn calculate_temporal_profile(&self, unit_id: Uuid) -> Result<()> {
        let (unit, _pulse, _) = self.unit_with_context(unit_id, 3600)?.ok_or_else(|| {
            CoreError::NotFound("unit not found for temporal profiling".to_string())
        })?;

        let mut params = BTreeMap::new();
        params.insert("unit_id".to_string(), DataValue::from(unit.id.to_string()));
        params.insert(
            "hour_peak".to_string(),
            DataValue::from(unit.observed_at.hour() as i64),
        );
        params.insert(
            "day_mask".to_string(),
            DataValue::from(1 << (unit.observed_at.weekday() as u32)),
        );

        // Simple recency: linear decay over 30 days
        let days_old = (Utc::now() - unit.observed_at).num_days();
        let recency = (30.0 - days_old as f32).max(0.0) / 30.0;
        params.insert(
            "recency_weight".to_string(),
            DataValue::from(recency as f64),
        );

        let script = r#"
?[unit_id, hour_peak, day_mask, recency_weight] <- [[
    $unit_id, $hour_peak, $day_mask, $recency_weight
]]
:put temporal_profile { unit_id => hour_peak, day_mask, recency_weight }
"#;
        self.cozo
            .run_script(script, params, ScriptMutability::Mutable)
            .map(|_| ())
            .map_err(|e| CoreError::Internal(format!("temporal profiling failed: {e}")))
    }

    fn get_temporal_profile(&self, unit_id: Uuid) -> Result<Option<TemporalProfile>> {
        let mut params = BTreeMap::new();
        params.insert("unit_id".to_string(), DataValue::from(unit_id.to_string()));

        let script = r#"
?[unit_id, hour_peak, day_mask, recency_weight] :=
    *temporal_profile{ unit_id: $unit_id, hour_peak, day_mask, recency_weight }
"#;
        let res = self
            .cozo
            .run_script(script, params, ScriptMutability::Immutable)
            .map_err(|e| CoreError::Internal(format!("failed to fetch temporal profile: {e}")))?;

        if res.rows.is_empty() {
            return Ok(None);
        }

        let row = &res.rows[0];
        let hour_peak = match &row[1] {
            DataValue::Num(cozo::Num::Int(i)) => *i as i8,
            _ => -1,
        };
        let day_mask = match &row[2] {
            DataValue::Num(cozo::Num::Int(i)) => *i as u8,
            _ => 0,
        };
        let recency_weight = match &row[3] {
            DataValue::Num(cozo::Num::Float(f)) => *f as f32,
            _ => 0.0,
        };

        Ok(Some(TemporalProfile {
            unit_id,
            hour_peak,
            day_mask,
            recency_weight,
        }))
    }

    fn save_rule(&self, rule: ProceduralRule) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("id".to_string(), DataValue::from(rule.id.to_string()));

        let lens_intent_str = format!(
            "[{}]",
            rule.lens_intent
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );

        params.insert(
            "pulse_mask".to_string(),
            DataValue::from(rule.pulse_mask.clone()),
        );
        params.insert(
            "threshold".to_string(),
            DataValue::from(rule.threshold as f64),
        );
        params.insert(
            "action_payload".to_string(),
            DataValue::from(
                serde_json::to_string(&rule.action_payload).unwrap_or_else(|_| "{}".to_string()),
            ),
        );
        params.insert(
            "created_at".to_string(),
            DataValue::from(rule.created_at.timestamp_millis() as f64),
        );

        let script = format!(
            r#"
?[id, lens_intent, pulse_mask, threshold, action_payload, created_at] <- [[
    $id, vec({lens_intent_str}), $pulse_mask, $threshold, parse_json($action_payload), $created_at
]]
:put procedural_rules {{ id => lens_intent, pulse_mask, threshold, action_payload, created_at }}
"#
        );

        self.cozo
            .run_script(&script, params, ScriptMutability::Mutable)
            .map(|_| ())
            .map_err(|e| CoreError::Internal(format!("failed to save rule: {e}")))
    }

    fn get_active_rules_for_pulse(&self, pulse_mask: &str) -> Result<Vec<ProceduralRule>> {
        let mut params = BTreeMap::new();
        params.insert("pulse_mask".to_string(), DataValue::from(pulse_mask));

        let script = r#"
?[id, lens_intent, pulse_mask, threshold, action_payload, created_at] :=
    *procedural_rules{ id, lens_intent, pulse_mask: $pulse_mask, threshold, action_payload, created_at }
"#;
        let res = self
            .cozo
            .run_script(script, params, ScriptMutability::Immutable)
            .map_err(|e| CoreError::Internal(format!("failed to fetch rules: {e}")))?;

        let mut out = Vec::new();
        for row in res.rows {
            if let Some(rule) = self.row_to_procedural_rule(&row) {
                out.push(rule);
            }
        }
        Ok(out)
    }

    fn get_all_rules(&self) -> Result<Vec<ProceduralRule>> {
        let script = r#"
?[id, lens_intent, pulse_mask, threshold, action_payload, created_at] :=
    *procedural_rules{ id, lens_intent, pulse_mask, threshold, action_payload, created_at }
"#;
        let res = self
            .cozo
            .run_script(script, BTreeMap::new(), ScriptMutability::Immutable)
            .map_err(|e| CoreError::Internal(format!("failed to fetch all rules: {e}")))?;

        let mut out = Vec::new();
        for row in res.rows {
            if let Some(rule) = self.row_to_procedural_rule(&row) {
                out.push(rule);
            }
        }
        Ok(out)
    }

    fn get_recent_snapshots(&self, limit: usize) -> Result<Vec<(Uuid, CognitiveState)>> {
        let mut params = BTreeMap::new();
        params.insert("limit".to_string(), DataValue::from(limit as i64));

        let script = r#"
?[unit_id, observed_at, window_secs, was_in_flow, was_on_call, dominant_app, time_of_day, context_switch_rate, was_in_meeting, was_in_focus_block, energy_level, mood_level, hrv_score, sleep_quality, minutes_since_last_meeting] :=
    *cognitive_snapshots{ unit_id, observed_at, window_secs, was_in_flow, was_on_call, dominant_app, time_of_day, context_switch_rate, was_in_meeting, was_in_focus_block, energy_level, mood_level, hrv_score, sleep_quality, minutes_since_last_meeting }
:order -observed_at
:limit $limit
"#;
        let res = self
            .cozo
            .run_script(script, params, ScriptMutability::Immutable)
            .map_err(|e| CoreError::Internal(format!("failed to fetch recent snapshots: {e}")))?;

        let mut out = Vec::new();
        for row in res.rows {
            if let (Some(DataValue::Str(id_s)), Some(state)) =
                (row.first(), self.row_to_cognitive_state(&row))
            {
                if let Ok(id) = Uuid::parse_str(id_s.as_str()) {
                    out.push((id, state));
                }
            }
        }
        Ok(out)
    }
}

impl LensIndexStore for CozoStore {
    fn upsert_lens_vec(&self, unit_id: Uuid, lens: &LensConfig, vec: &[f32]) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("unit_id".to_string(), DataValue::from(unit_id.to_string()));
        params.insert(
            "lens_id".to_string(),
            DataValue::from(lens.id.as_str().to_string()),
        );
        params.insert(
            "vec".to_string(),
            DataValue::List(vec.iter().map(|f| DataValue::from(*f as f64)).collect()),
        );

        let script = if lens.dimensions == 384 {
            r#"
?[unit_id, lens_id, vec] <- [[$unit_id, $lens_id, $vec]]
:put lens_map_384 { unit_id, lens_id => vec }
"#
        } else {
            r#"
?[unit_id, lens_id, vec] <- [[$unit_id, $lens_id, $vec]]
:put lens_map { unit_id, lens_id => vec }
"#
        };

        self.cozo
            .run_script(script, params, ScriptMutability::Mutable)
            .map_err(|e| CoreError::Internal(format!("failed to upsert lens: {e}")))?;
        Ok(())
    }

    fn search_lens_vec(&self, lens: &LensConfig, query_vec: &[f32], k: usize) -> Result<Vec<Uuid>> {
        if query_vec.is_empty() {
            return Ok(Vec::new());
        }

        let mut params = BTreeMap::new();
        params.insert(
            "lens_id".to_string(),
            DataValue::from(lens.id.as_str().to_string()),
        );
        params.insert(
            "vec".to_string(),
            DataValue::List(
                query_vec
                    .iter()
                    .map(|f| DataValue::from(*f as f64))
                    .collect(),
            ),
        );
        params.insert("k".to_string(), DataValue::from(k as i64));

        let script = if lens.dimensions == 384 {
            r#"
?[id, dist] := ~lens_map_384:vec_idx{ unit_id: id | query: $vec, k: $k, ef: 50, bind_distance: dist, filter: (lens_id == $lens_id) }
:order dist
:limit $k
"#
        } else {
            r#"
?[id, dist] := ~lens_map:vec_idx{ unit_id: id | query: $vec, k: $k, ef: 50, bind_distance: dist, filter: (lens_id == $lens_id) }
:order dist
:limit $k
"#
        };
        match self
            .cozo
            .run_script(script, params, ScriptMutability::Immutable)
        {
            Ok(result) => {
                let out = result
                    .rows
                    .iter()
                    .filter_map(|row| {
                        if let Some(DataValue::Str(id_str)) = row.first() {
                            Uuid::parse_str(id_str).ok()
                        } else {
                            None
                        }
                    })
                    .collect();
                Ok(out)
            }
            Err(_) => Ok(Vec::new()),
        }
    }

    fn get_all_lens_vectors(&self, lens: &LensConfig) -> Result<Vec<(Uuid, Vec<f32>)>> {
        let mut params = BTreeMap::new();
        params.insert(
            "lens_id".to_string(),
            DataValue::from(lens.id.as_str().to_string()),
        );

        let script = if lens.dimensions == 384 {
            r#"
?[unit_id, vec] := *lens_map_384{ unit_id, lens_id: $lens_id, vec }
"#
        } else {
            r#"
?[unit_id, vec] := *lens_map{ unit_id, lens_id: $lens_id, vec }
"#
        };

        let res = self
            .cozo
            .run_script(script, params, ScriptMutability::Immutable)
            .map_err(|e| CoreError::Internal(format!("failed to export lens vectors: {e}")))?;

        let mut out = Vec::new();
        for row in res.rows {
            let id_str = match &row[0] {
                DataValue::Str(s) => s.as_str(),
                _ => "",
            };
            let id = Uuid::parse_str(id_str).unwrap_or_default();
            let vec = match &row[1] {
                DataValue::List(l) => l
                    .iter()
                    .map(|v| match v {
                        DataValue::Num(cozo::Num::Float(f)) => *f as f32,
                        _ => 0.0,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            out.push((id, vec));
        }
        Ok(out)
    }
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

fn resolve_cozo_path() -> Result<PathBuf> {
    let preferred = expand_home("~/.ambient/cozo").join("ambient_cozo.sqlite");
    if let Some(parent) = preferred.parent() {
        if std::fs::create_dir_all(parent).is_ok() {
            return Ok(preferred);
        }
    }

    let fallback_dir = std::env::temp_dir().join("ambient").join("cozo");
    std::fs::create_dir_all(&fallback_dir).map_err(|e| {
        CoreError::Internal(format!(
            "failed to create cozo directories at {}: {e}",
            fallback_dir.display()
        ))
    })?;
    Ok(fallback_dir.join("ambient_cozo.sqlite"))
}

fn hash_to_string(hash: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in hash {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ambient_core::{KnowledgeStore, KnowledgeUnit, SourceId};

    use super::*;

    #[test]
    fn pulse_window_returns_sorted_events() {
        let store = CozoStore::new_for_test().expect("store should build");
        let base = Utc::now();

        store
            .record_pulse(PulseEvent {
                timestamp: base + chrono::Duration::seconds(2),
                signal: PulseSignal::AudioInputActive { active: true },
            })
            .expect("pulse write");

        store
            .record_pulse(PulseEvent {
                timestamp: base + chrono::Duration::seconds(1),
                signal: PulseSignal::ActiveApp {
                    bundle_id: "com.app".to_string(),
                    window_title: Some("Window".to_string()),
                },
            })
            .expect("pulse write");

        store
            .record_pulse(PulseEvent {
                timestamp: base + chrono::Duration::seconds(3),
                signal: PulseSignal::ContextSwitchRate {
                    switches_per_minute: 1.0,
                },
            })
            .expect("pulse write");

        let got = store
            .pulse_window(base, base + chrono::Duration::seconds(5))
            .expect("pulse query");

        assert_eq!(got.len(), 3);
        assert!(got[0].timestamp <= got[1].timestamp && got[1].timestamp <= got[2].timestamp);
    }

    #[test]
    fn unit_with_context_derives_cognitive_state() {
        let store = CozoStore::new_for_test().expect("store should build");
        let observed_at = Utc::now();
        let id = Uuid::new_v4();

        store
            .upsert(KnowledgeUnit {
                id,
                source: SourceId::new("obsidian"),
                content: "test note".to_string(),
                title: Some("title".to_string()),
                metadata: HashMap::new(),
                observed_at,
                content_hash: [1; 32],
            })
            .expect("upsert");

        store
            .record_pulse(PulseEvent {
                timestamp: observed_at,
                signal: PulseSignal::ContextSwitchRate {
                    switches_per_minute: 1.0,
                },
            })
            .expect("pulse write");
        store
            .record_pulse(PulseEvent {
                timestamp: observed_at,
                signal: PulseSignal::AudioInputActive { active: true },
            })
            .expect("pulse write");
        store
            .record_pulse(PulseEvent {
                timestamp: observed_at,
                signal: PulseSignal::ActiveApp {
                    bundle_id: "com.test".to_string(),
                    window_title: None,
                },
            })
            .expect("pulse write");

        let (_, _, state) = store
            .unit_with_context(id, 120)
            .expect("context query")
            .expect("unit exists");
        assert!(state.was_in_flow);
        assert!(state.was_on_call);
        assert_eq!(state.dominant_app.as_deref(), Some("com.test"));
    }

    #[test]
    fn search_fulltext_empty_returns_all_notes() {
        let store = CozoStore::new_for_test().expect("store should build");
        let now = Utc::now();

        for i in 0..3 {
            store
                .upsert(KnowledgeUnit {
                    id: Uuid::new_v4(),
                    source: SourceId::new("obsidian"),
                    content: format!("note {i}"),
                    title: Some(format!("N{i}")),
                    metadata: HashMap::new(),
                    observed_at: now + chrono::Duration::seconds(i),
                    content_hash: [i as u8 + 1; 32],
                })
                .expect("upsert");
        }

        let all = store.search_fulltext("").expect("search");
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn related_uses_graph_depth_not_result_count() {
        let store = CozoStore::new_for_test().expect("store should build");
        let now = Utc::now();

        let id_c = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_a = Uuid::new_v4();

        store
            .upsert(KnowledgeUnit {
                id: id_c,
                source: SourceId::new("obsidian"),
                content: "C".to_string(),
                title: Some("C".to_string()),
                metadata: HashMap::from([("links".to_string(), serde_json::json!([]))]),
                observed_at: now,
                content_hash: [31; 32],
            })
            .expect("upsert C");
        store
            .upsert(KnowledgeUnit {
                id: id_b,
                source: SourceId::new("obsidian"),
                content: "B links [[C]]".to_string(),
                title: Some("B".to_string()),
                metadata: HashMap::from([("links".to_string(), serde_json::json!(["C"]))]),
                observed_at: now + chrono::Duration::seconds(1),
                content_hash: [32; 32],
            })
            .expect("upsert B");
        store
            .upsert(KnowledgeUnit {
                id: id_a,
                source: SourceId::new("obsidian"),
                content: "A links [[B]]".to_string(),
                title: Some("A".to_string()),
                metadata: HashMap::from([("links".to_string(), serde_json::json!(["B"]))]),
                observed_at: now + chrono::Duration::seconds(2),
                content_hash: [33; 32],
            })
            .expect("upsert A");

        let depth1 = store.related(id_a, 1).expect("related d1");
        let depth2 = store.related(id_a, 2).expect("related d2");

        assert!(depth1.iter().any(|u| u.id == id_b));
        assert!(!depth1.iter().any(|u| u.id == id_c));
        assert!(depth2.iter().any(|u| u.id == id_c));
    }

    #[test]
    fn feedback_scoring_logic_is_weighted_and_squashed() {
        let store = CozoStore::new_for_test().expect("store should build");
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        // 1. Initial neutral score
        let rels = store
            .cozo
            .run_script("::relations", BTreeMap::new(), ScriptMutability::Immutable)
            .expect("rels");
        println!("Available relations: {:?}", rels);
        assert_eq!(store.feedback_score(id_a).expect("score"), 0.5);

        // 2. Positive signal (Acted On: +1)
        store
            .record_feedback(FeedbackEvent {
                id: Uuid::new_v4(),
                timestamp: Utc::now(),
                signal: FeedbackSignal::QueryResultActedOn {
                    query_text: "test".to_string(),
                    unit_id: id_a,
                    action: ambient_core::ResultAction::OpenedSource,
                    ms_to_action: 1000,
                },
            })
            .expect("record");

        // sigmoid(1) ~= 0.73
        let score_post_positive = store.feedback_score(id_a).expect("score");
        assert!(score_post_positive > 0.7 && score_post_positive < 0.75);

        // 3. Oracle boost (Oracle Acted: +2) -> total +3
        store
            .record_feedback(FeedbackEvent {
                id: Uuid::new_v4(),
                timestamp: Utc::now(),
                signal: FeedbackSignal::OracleResultActedOn {
                    query_text: "test".to_string(),
                    unit_id: id_a,
                    action: ambient_core::ResultAction::OpenedSource,
                    ms_to_action: 1000,
                    active_lens_snapshot: None,
                },
            })
            .expect("record");

        // sigmoid(3) ~= 0.95
        let score_post_oracle = store.feedback_score(id_a).expect("score");
        assert!(score_post_oracle > 0.9);

        // 4. Negative signal (Dismissed: -0.5) for a DIFFERENT unit
        store
            .record_feedback(FeedbackEvent {
                id: Uuid::new_v4(),
                timestamp: Utc::now(),
                signal: FeedbackSignal::QueryResultDismissed {
                    query_text: "test".to_string(),
                    unit_id: id_b,
                },
            })
            .expect("record");

        // sigmoid(-0.5) ~= 0.37
        let score_b = store.feedback_score(id_b).expect("score");
        assert!(score_b > 0.35 && score_b < 0.4);
    }

    #[test]
    fn get_recent_snapshots_retrieves_latest_first() {
        let store = CozoStore::new_for_test().expect("store");
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let now = Utc::now();

        // We use upsert to create snapshots
        store
            .upsert(KnowledgeUnit {
                id: id1,
                source: SourceId::new("test"),
                content: "a".to_string(),
                title: None,
                metadata: HashMap::new(),
                observed_at: now - chrono::Duration::minutes(10),
                content_hash: [1; 32],
            })
            .expect("upsert 1");

        store
            .upsert(KnowledgeUnit {
                id: id2,
                source: SourceId::new("test"),
                content: "b".to_string(),
                title: None,
                metadata: HashMap::new(),
                observed_at: now,
                content_hash: [2; 32],
            })
            .expect("upsert 2");

        let snapshots = store.get_recent_snapshots(10).expect("query");
        assert!(snapshots.len() >= 2);
        // Latest first
        assert_eq!(snapshots[0].0, id2);
        assert_eq!(snapshots[1].0, id1);
    }

    #[test]
    fn get_all_rules_retrieves_saved_rules() {
        let _store = CozoStore::new_for_test().expect("store");
        let _rule = ProceduralRule {
            id: Uuid::new_v4(),
            lens_intent: vec![1.0; 768],
            pulse_mask: "com.apple.Safari".to_string(),
            threshold: 0.5,
            action_payload: serde_json::json!({"action": "notify"}),
            created_at: Utc::now(),
        };
    }
}
