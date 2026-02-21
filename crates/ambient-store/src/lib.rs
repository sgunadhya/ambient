use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use ambient_core::{
    derive_cognitive_state, CognitiveState, CoreError, FeedbackEvent, FeedbackSignal, KnowledgeStore,
    KnowledgeUnit, PulseEvent, PulseSignal, Result,
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

pub struct LadybugStore {
    cozo: DbInstance,
    units: Mutex<HashMap<Uuid, KnowledgeUnit>>,
    hash_index: Mutex<HashMap<[u8; 32], Uuid>>,
    links: Mutex<HashMap<Uuid, HashSet<Uuid>>>,
    snapshots: Mutex<HashMap<Uuid, CognitiveState>>,
    feedback: Mutex<Vec<FeedbackEvent>>,
    pulse_storage: tsink::Storage,
}

impl LadybugStore {
    pub fn new() -> Result<Self> {
        let cozo_path = resolve_cozo_path()?;

        let cozo = DbInstance::new("sqlite", &cozo_path, "")
            .map_err(|e| CoreError::Internal(format!("failed to initialize cozo: {e}")))?;

        let pulse_storage = tsink::StorageBuilder::new()
            .with_data_path("~/.ambient/pulse")
            .with_partition_duration(Duration::from_secs(3600))
            .with_retention(Duration::from_secs(90 * 24 * 3600))
            .with_wal_sync_mode(tsink::WalSyncMode::Periodic(Duration::from_secs(1)))
            .build()
            .map_err(|e| CoreError::Internal(format!("failed to initialize tsink: {e}")))?;

        let store = Self {
            cozo,
            units: Mutex::new(HashMap::new()),
            hash_index: Mutex::new(HashMap::new()),
            links: Mutex::new(HashMap::new()),
            snapshots: Mutex::new(HashMap::new()),
            feedback: Mutex::new(Vec::new()),
            pulse_storage,
        };
        store.init_cozo_schema();
        Ok(store)
    }

    fn init_cozo_schema(&self) {
        let schema = r#"
:create notes {
    id: String =>
    source: String,
    title: String,
    content: String,
    hash: String,
    observed_at: Float
}
:create links {
    from_id: String,
    to_id: String =>
    link_type: String
}
:create pattern_results {
    id: String =>
    pattern_type: String,
    summary: String,
    unit_ids: [String],
    detected_at: Float,
    feedback: String?
}
:create feedback_events {
    id: String =>
    timestamp: Float,
    signal_type: String,
    unit_id: String?,
    query_text: String?,
    action: String?,
    pattern_id: String?
}
:create cognitive_snapshots {
    unit_id: String =>
    observed_at: Float,
    window_secs: Int,
    was_in_flow: Bool,
    was_on_call: Bool,
    dominant_app: String?,
    time_of_day: Int,
    context_switch_rate: Float,
    was_in_meeting: Bool,
    was_in_focus_block: Bool,
    energy_level: Int?,
    mood_level: Int?,
    hrv_score: Float?,
    sleep_quality: Float?,
    minutes_since_last_meeting: Int?
}
"#;

        let _ = self
            .cozo
            .run_script(schema, BTreeMap::new(), ScriptMutability::Mutable);
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

        let script = r#"
?[id, source, title, content, hash, observed_at] <- [[
    $id, $source, $title, $content, $hash, $observed_at
]]
:put notes { id => source, title, content, hash, observed_at }
"#;
        let _ = self
            .cozo
            .run_script(script, params, ScriptMutability::Mutable);
    }

    fn cozo_put_link(&self, from_id: Uuid, to_id: Uuid) {
        let mut params = BTreeMap::new();
        params.insert("from_id".to_string(), DataValue::from(from_id.to_string()));
        params.insert("to_id".to_string(), DataValue::from(to_id.to_string()));
        params.insert("link_type".to_string(), DataValue::from("wikilink".to_string()));

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
        params.insert("was_in_flow".to_string(), DataValue::from(state.was_in_flow));
        params.insert("was_on_call".to_string(), DataValue::from(state.was_on_call));
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
        params.insert("context_switch_rate".to_string(), DataValue::from(context_switch_rate));
        params.insert("was_in_meeting".to_string(), DataValue::from(state.was_in_meeting));
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
            FeedbackSignal::QueryResultDismissed { query_text, unit_id } => (
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
                    labels.insert("current_event_duration_minutes".to_string(), minutes.to_string());
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
        let mut title_to_id = HashMap::new();
        {
            let guard = self
                .units
                .lock()
                .map_err(|_| CoreError::Internal("units lock poisoned".to_string()))?;
            for existing in guard.values() {
                if let Some(title) = &existing.title {
                    title_to_id.insert(title.clone(), existing.id);
                }
            }
        }

        let mut edges = HashSet::new();
        if let Some(Value::Array(links)) = unit.metadata.get("links") {
            for link in links {
                if let Value::String(link_title) = link {
                    if let Some(target) = title_to_id.get(link_title) {
                        edges.insert(*target);
                        self.cozo_put_link(unit.id, *target);
                    }
                }
            }
        }

        self.links
            .lock()
            .map_err(|_| CoreError::Internal("links lock poisoned".to_string()))?
            .insert(unit.id, edges);

        Ok(())
    }
}

impl KnowledgeStore for LadybugStore {
    fn upsert(&self, unit: KnowledgeUnit) -> Result<()> {
        {
            let mut hashes = self
                .hash_index
                .lock()
                .map_err(|_| CoreError::Internal("hash lock poisoned".to_string()))?;
            if let Some(existing_id) = hashes.get(&unit.content_hash).copied() {
                if existing_id != unit.id {
                    return Ok(());
                }
            }
            hashes.insert(unit.content_hash, unit.id);
        }

        self.units
            .lock()
            .map_err(|_| CoreError::Internal("units lock poisoned".to_string()))?
            .insert(unit.id, unit.clone());

        self.cozo_put_note(&unit);
        self.index_links(&unit)?;

        // TimeContext is derived at ingestion time from observed_at.
        self.record_pulse(Self::derive_time_context(unit.observed_at))?;
        let window_secs = 120i64;
        let from = unit.observed_at - chrono::Duration::seconds(window_secs);
        let to = unit.observed_at + chrono::Duration::seconds(window_secs);
        if let Ok(pulse) = self.pulse_window(from, to) {
            let state = derive_cognitive_state(unit.observed_at, &pulse);
            let (sum, count) = pulse.iter().fold((0.0f64, 0usize), |acc, event| {
                match event.signal {
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
                }
            });
            let avg_switch = if count == 0 { 0.0 } else { sum / count as f64 };
            self.cozo_put_snapshot(&unit, window_secs, &state, avg_switch);
            let _ = self
                .snapshots
                .lock()
                .map(|mut snapshots| snapshots.insert(unit.id, state));
        }
        Ok(())
    }

    fn search_semantic(&self, query: &str, k: usize) -> Result<Vec<KnowledgeUnit>> {
        let units: Vec<KnowledgeUnit> = self
            .units
            .lock()
            .map_err(|_| CoreError::Internal("units lock poisoned".to_string()))?
            .values()
            .cloned()
            .collect();

        if units.iter().any(|u| u.embedding.is_some()) {
            let mut ranked = units;
            ranked.sort_by_key(|u| std::cmp::Reverse(u.embedding.as_ref().map_or(0usize, |v| v.len())));
            return Ok(ranked.into_iter().take(k).collect());
        }

        let mut fallback: Vec<KnowledgeUnit> = self.search_fulltext(query)?.into_iter().take(k).collect();
        if fallback.is_empty() {
            fallback = self
                .units
                .lock()
                .map_err(|_| CoreError::Internal("units lock poisoned".to_string()))?
                .values()
                .take(k)
                .cloned()
                .collect();
        }
        Ok(fallback)
    }

    fn search_fulltext(&self, query: &str) -> Result<Vec<KnowledgeUnit>> {
        Ok(self
            .units
            .lock()
            .map_err(|_| CoreError::Internal("units lock poisoned".to_string()))?
            .values()
            .filter(|unit| {
                unit.content.contains(query)
                    || unit
                        .title
                        .as_deref()
                        .is_some_and(|title| title.contains(query))
            })
            .cloned()
            .collect())
    }

    fn related(&self, id: Uuid, depth: usize) -> Result<Vec<KnowledgeUnit>> {
        if depth == 0 {
            return Ok(Vec::new());
        }

        let links = self
            .links
            .lock()
            .map_err(|_| CoreError::Internal("links lock poisoned".to_string()))?
            .clone();
        let units = self
            .units
            .lock()
            .map_err(|_| CoreError::Internal("units lock poisoned".to_string()))?
            .clone();

        let mut frontier = vec![id];
        let mut visited = HashSet::new();
        let mut out = Vec::new();

        for _ in 0..depth {
            let mut next = Vec::new();
            for node in frontier {
                if !visited.insert(node) {
                    continue;
                }
                if let Some(neighbors) = links.get(&node) {
                    for neighbor in neighbors {
                        if let Some(unit) = units.get(neighbor) {
                            out.push(unit.clone());
                        }
                        next.push(*neighbor);
                    }
                }
            }
            frontier = next;
        }

        Ok(out)
    }

    fn get_by_id(&self, id: Uuid) -> Result<Option<KnowledgeUnit>> {
        Ok(self
            .units
            .lock()
            .map_err(|_| CoreError::Internal("units lock poisoned".to_string()))?
            .get(&id)
            .cloned())
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
        if let Some(state) = self
            .snapshots
            .lock()
            .map_err(|_| CoreError::Internal("snapshots lock poisoned".to_string()))?
            .get(&id)
            .cloned()
        {
            return Ok(Some((unit, state)));
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
        self.feedback
            .lock()
            .map_err(|_| CoreError::Internal("feedback lock poisoned".to_string()))?
            .push(event.clone());
        self.cozo_record_feedback(&event);
        Ok(())
    }

    fn feedback_score(&self, unit_id: Uuid) -> Result<f32> {
        let guard = self
            .feedback
            .lock()
            .map_err(|_| CoreError::Internal("feedback lock poisoned".to_string()))?;
        let mut acted = 0f32;
        let mut dismissed = 0f32;

        for event in guard.iter() {
            match &event.signal {
                FeedbackSignal::QueryResultActedOn { unit_id: id, .. } if *id == unit_id => {
                    acted += 1.0
                }
                FeedbackSignal::QueryResultDismissed { unit_id: id, .. } if *id == unit_id => {
                    dismissed += 1.0
                }
                _ => {}
            }
        }

        let denom = acted + dismissed;
        if denom == 0.0 {
            return Ok(0.5);
        }
        Ok(acted / denom)
    }

    fn pattern_feedback(&self, pattern_id: Uuid) -> Result<Option<String>> {
        let guard = self
            .feedback
            .lock()
            .map_err(|_| CoreError::Internal("feedback lock poisoned".to_string()))?;

        for event in guard.iter().rev() {
            match &event.signal {
                FeedbackSignal::PatternMarkedUseful { pattern_id: id } if *id == pattern_id => {
                    return Ok(Some("useful".to_string()))
                }
                FeedbackSignal::PatternMarkedNoise { pattern_id: id } if *id == pattern_id => {
                    return Ok(Some("noise".to_string()))
                }
                _ => {}
            }
        }

        Ok(None)
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
        let store = LadybugStore::new().expect("store should build");
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
        let store = LadybugStore::new().expect("store should build");
        let observed_at = Utc::now();
        let id = Uuid::new_v4();

        store
            .upsert(KnowledgeUnit {
                id,
                source: SourceId::new("obsidian"),
                content: "test note".to_string(),
                title: Some("title".to_string()),
                metadata: HashMap::new(),
                embedding: None,
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
}
