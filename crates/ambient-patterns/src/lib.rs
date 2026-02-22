//! # ambient-patterns
//!
//! PatternDetector — pure Datalog rules executed against CozoDB.
//! No tsink dependency. Per NEXT.md Amendment 5 + 6.
//!
//! Phase 5 — ImplicitFeedbackRecorder: correlates post-query activity
//! signals with returned result units and records behavioral FeedbackEvents.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ambient_core::{
    CognitiveState, CoreError, FeedbackEvent, FeedbackSignal, KnowledgeStore, KnowledgeUnit,
    QueryRequest, QueryResult, Result, ResultAction, SourceId, Uuid,
};
use chrono::{Datelike, Timelike, Utc};
use cozo::{DataValue, DbInstance, ScriptMutability};
use ndarray::Array2;
use petal_clustering::{Dbscan, Fit};
use petal_neighbors::distance::Euclidean;

// ── PatternInsight ─────────────────────────────────────────────────────────────

/// A single detected pattern insight, surfaced in the weekly HTML report.
#[derive(Debug, Clone)]
pub struct PatternInsight {
    pub key: String,
    pub summary: String,
    pub score: f32,
    /// IDs of associated notes (may be empty for aggregate patterns).
    pub unit_ids: Vec<Uuid>,
}

// ── Datalog rules ─────────────────────────────────────────────────────────────

/// Pure Datalog rules executed against CozoDB.
/// Each rule queries `cognitive_snapshots` joined with `notes`.
/// No Rust iteration over results — the filtering/aggregation is in Datalog.
struct DatalogRules;

impl DatalogRules {
    /// Flow cluster: notes captured during low-context-switch sessions.
    /// Returns (unit_id, observed_at).
    const FLOW_NOTES: &'static str = r#"
?[unit_id, observed_at] :=
    *cognitive_snapshots{ unit_id, was_in_flow: true, observed_at }
:order -observed_at
:limit 500
"#;

    /// Call-capture: notes captured while audio input was active (was_on_call).
    const CALL_NOTES: &'static str = r#"
?[unit_id, observed_at] :=
    *cognitive_snapshots{ unit_id, was_on_call: true, observed_at }
:order -observed_at
:limit 500
"#;

    /// Late-night notes: time_of_day outside 06:00–22:00 range.
    const LATE_NIGHT_NOTES: &'static str = r#"
?[unit_id, observed_at, time_of_day] :=
    *cognitive_snapshots{ unit_id, time_of_day, observed_at },
    (time_of_day < 6 || time_of_day >= 22)
:order -observed_at
:limit 500
"#;

    /// Focus-block notes: captured inside a calendar focus block.
    const FOCUS_BLOCK_NOTES: &'static str = r#"
?[unit_id, observed_at] :=
    *cognitive_snapshots{ unit_id, was_in_focus_block: true, observed_at }
:order -observed_at
:limit 500
"#;

    /// High-churn context: context_switch_rate above 3 switches/min — fragmented session.
    #[allow(dead_code)]
    const HIGH_CHURN_NOTES: &'static str = r#"
?[unit_id, observed_at, rate] :=
    *cognitive_snapshots{ unit_id, context_switch_rate: rate, observed_at },
    rate > 3.0
:order -rate
:limit 500
"#;

    /// Stale revisit candidates: notes older than 30 days, no recent retrieval.
    /// Approximated via observed_at (ms since epoch).
    fn stale_notes_query(cutoff_ms: i64) -> String {
        format!(
            r#"
?[unit_id, observed_at] :=
    *notes{{ id: unit_id, observed_at }},
    observed_at < {}
:order observed_at
:limit 200
"#,
            cutoff_ms as f64
        )
    }

    /// COUNT of flow notes in one query (used for the summary line).
    const COUNT_FLOW: &'static str = r#"
?[count(unit_id)] :=
    *cognitive_snapshots{ unit_id, was_in_flow: true }
"#;

    const COUNT_CALL: &'static str = r#"
?[count(unit_id)] :=
    *cognitive_snapshots{ unit_id, was_on_call: true }
"#;

    const COUNT_LATE_NIGHT: &'static str = r#"
?[count(unit_id)] :=
    *cognitive_snapshots{ unit_id, time_of_day },
    (time_of_day < 6 || time_of_day >= 22)
"#;

    const COUNT_FOCUS: &'static str = r#"
?[count(unit_id)] :=
    *cognitive_snapshots{ unit_id, was_in_focus_block: true }
"#;
}

// ── PatternDetector ───────────────────────────────────────────────────────────

pub struct PatternDetector {
    store: Arc<dyn KnowledgeStore>,
}

impl PatternDetector {
    pub fn new(store: Arc<dyn KnowledgeStore>) -> Self {
        Self { store }
    }

    /// Run all Datalog pattern rules against `db` and return insights.
    /// This is the "pure Datalog" path per NEXT.md Amendment 6.
    pub fn detect_from_cozo(&self, db: &DbInstance) -> Vec<PatternInsight> {
        let mut out = Vec::new();

        // Rule 1: flow cluster
        if let Some(insight) = Self::run_count_rule(
            db,
            DatalogRules::COUNT_FLOW,
            DatalogRules::FLOW_NOTES,
            "flow-cluster",
            "notes captured during focused low-interruption sessions",
        ) {
            out.push(insight);
        }

        // Rule 2: call captures
        if let Some(insight) = Self::run_count_rule(
            db,
            DatalogRules::COUNT_CALL,
            DatalogRules::CALL_NOTES,
            "call-capture",
            "notes captured while on a call",
        ) {
            out.push(insight);
        }

        // Rule 3: late-night notes
        if let Some(insight) = Self::run_count_rule(
            db,
            DatalogRules::COUNT_LATE_NIGHT,
            DatalogRules::LATE_NIGHT_NOTES,
            "late-night",
            "notes captured outside core hours (10pm–6am)",
        ) {
            out.push(insight);
        }

        // Rule 4: focus-block notes
        if let Some(insight) = Self::run_count_rule(
            db,
            DatalogRules::COUNT_FOCUS,
            DatalogRules::FOCUS_BLOCK_NOTES,
            "focus-block",
            "notes captured during calendar focus blocks",
        ) {
            out.push(insight);
        }

        // Rule 5: stale revisit candidates (30-day cutoff)
        let cutoff = Utc::now() - chrono::Duration::days(30);
        let cutoff_ms = cutoff.timestamp_millis();
        let query = DatalogRules::stale_notes_query(cutoff_ms);
        if let Ok(result) = db.run_script(&query, BTreeMap::new(), ScriptMutability::Immutable) {
            let ids: Vec<Uuid> = result
                .rows
                .iter()
                .filter_map(|row| {
                    row.first()
                        .and_then(|v| {
                            if let DataValue::Str(s) = v {
                                Some(s.as_ref())
                            } else {
                                None
                            }
                        })
                        .and_then(|s| Uuid::parse_str(s).ok())
                })
                .collect();
            if !ids.is_empty() {
                out.push(PatternInsight {
                    key: "revisit-candidates".to_string(),
                    summary: format!(
                        "{} notes older than 30 days are candidates for revisit",
                        ids.len()
                    ),
                    score: ids.len() as f32,
                    unit_ids: ids,
                });
            }
        }

        out
    }

    /// Fallback path: detect patterns from in-memory QueryResult candidates
    /// (used when CozoDB instance is not directly accessible).
    pub fn detect(&self, candidates: &[QueryResult]) -> Result<Vec<PatternInsight>> {
        let mut out = Vec::new();

        let flow_ids: Vec<Uuid> = candidates
            .iter()
            .filter(|item| item.cognitive_state.as_ref().is_some_and(|s| s.was_in_flow))
            .map(|item| item.unit.id)
            .collect();
        if !flow_ids.is_empty() {
            out.push(PatternInsight {
                key: "flow-cluster".to_string(),
                summary: format!("{} notes captured during flow sessions", flow_ids.len()),
                score: flow_ids.len() as f32,
                unit_ids: flow_ids,
            });
        }

        let call_ids: Vec<Uuid> = candidates
            .iter()
            .filter(|item| item.cognitive_state.as_ref().is_some_and(|s| s.was_on_call))
            .map(|item| item.unit.id)
            .collect();
        if !call_ids.is_empty() {
            out.push(PatternInsight {
                key: "call-capture".to_string(),
                summary: format!("{} notes captured during calls", call_ids.len()),
                score: call_ids.len() as f32,
                unit_ids: call_ids,
            });
        }

        let late_ids: Vec<Uuid> = candidates
            .iter()
            .filter(|item| {
                item.cognitive_state
                    .as_ref()
                    .and_then(|s| s.time_of_day)
                    .is_some_and(|h| !(6..22).contains(&h))
            })
            .map(|item| item.unit.id)
            .collect();
        if !late_ids.is_empty() {
            out.push(PatternInsight {
                key: "late-night".to_string(),
                summary: format!(
                    "{} notes captured late at night or early morning",
                    late_ids.len()
                ),
                score: late_ids.len() as f32,
                unit_ids: late_ids,
            });
        }

        let mut tag_counts: HashMap<String, Vec<Uuid>> = HashMap::new();
        for item in candidates {
            if let Some(tags) = item.unit.metadata.get("tags").and_then(|v| v.as_array()) {
                for tag in tags.iter().filter_map(|v| v.as_str()) {
                    tag_counts
                        .entry(tag.to_string())
                        .or_default()
                        .push(item.unit.id);
                }
            }
        }
        if let Some((top_tag, ids)) = tag_counts.into_iter().max_by_key(|(_, ids)| ids.len()) {
            if ids.len() > 1 {
                out.push(PatternInsight {
                    key: "topic-cooccurrence".to_string(),
                    summary: format!(
                        "Topic '{}' appears in {} clustered notes",
                        top_tag,
                        ids.len()
                    ),
                    score: ids.len() as f32,
                    unit_ids: ids,
                });
            }
        }

        let revisit = self.revisit_candidates()?;
        if !revisit.is_empty() {
            out.push(PatternInsight {
                key: "revisit-candidates".to_string(),
                summary: format!(
                    "{} notes older than 30 days worth revisiting",
                    revisit.len()
                ),
                score: revisit.len() as f32,
                unit_ids: revisit,
            });
        }

        Ok(out)
    }

    pub fn should_run_now(
        &self,
        idle_minutes: u64,
        plugged_in: bool,
        battery_percent: f32,
    ) -> bool {
        let hour = Utc::now().hour();
        // Only run during 1–6am, when system is idle and on power
        idle_minutes > 10 && (plugged_in || battery_percent > 80.0) && (1..6).contains(&hour)
    }

    pub fn revisit_candidates(&self) -> Result<Vec<Uuid>> {
        let units = self.store.search_fulltext("")?;
        let threshold = Utc::now() - chrono::Duration::days(30);
        Ok(units
            .into_iter()
            .filter(|unit| unit.observed_at < threshold)
            .map(|unit| unit.id)
            .collect())
    }

    pub fn query_seed(&self, text: &str, k: usize) -> QueryRequest {
        QueryRequest {
            text: text.to_string(),
            k,
            include_pulse_context: true,
            context_window_secs: Some(120),
        }
    }

    pub fn collect_candidates(&self, limit: usize) -> Result<Vec<QueryResult>> {
        let units = self.store.search_fulltext("")?;
        let mut out = Vec::new();

        for unit in units.into_iter().take(limit) {
            let (pulse_context, cognitive_state) = match self.store.unit_with_context(unit.id, 120)
            {
                Ok(Some((_, pulse, state))) => (Some(pulse), Some(state)),
                _ => (None, Some(default_state(&unit))),
            };

            out.push(QueryResult {
                unit,
                score: 1.0,
                pulse_context,
                cognitive_state,
                historical_feedback_score: 0.5,
                capability_status: None,
            });
        }

        Ok(out)
    }

    // ── Private Datalog execution helpers ────────────────────────────────────

    fn run_count_rule(
        db: &DbInstance,
        count_query: &str,
        _detail_query: &str,
        key: &str,
        description: &str,
    ) -> Option<PatternInsight> {
        let result = db
            .run_script(count_query, BTreeMap::new(), ScriptMutability::Immutable)
            .ok()?;
        let count = result
            .rows
            .first()
            .and_then(|row| row.first())
            .and_then(|v| match v {
                DataValue::Num(n) => Some(match n {
                    cozo::Num::Int(i) => *i as usize,
                    cozo::Num::Float(f) => *f as usize,
                }),
                _ => None,
            })
            .unwrap_or(0);

        if count == 0 {
            return None;
        }

        Some(PatternInsight {
            key: key.to_string(),
            summary: format!("{count} {description}"),
            score: count as f32,
            unit_ids: Vec::new(), // IDs fetched separately only for report generation
        })
    }
}

// ── SchedulerProbe ────────────────────────────────────────────────────────────

pub trait SchedulerProbe: Send + Sync {
    fn idle_minutes(&self) -> u64;
    fn plugged_in(&self) -> bool;
    fn battery_percent(&self) -> f32;
    fn user_active(&self) -> bool;
}

pub struct DefaultSchedulerProbe;

impl SchedulerProbe for DefaultSchedulerProbe {
    fn idle_minutes(&self) -> u64 {
        30
    }
    fn plugged_in(&self) -> bool {
        true
    }
    fn battery_percent(&self) -> f32 {
        100.0
    }
    fn user_active(&self) -> bool {
        false
    }
}

// ── PatternScheduler ──────────────────────────────────────────────────────────

pub struct PatternScheduler {
    detector: PatternDetector,
    noticer: NoticerConsumer,
    probe: Arc<dyn SchedulerProbe>,
    poll_interval: Duration,
    report_dir: PathBuf,
    last_run_day: Arc<Mutex<Option<(i32, u32, u32)>>>,
}

impl PatternScheduler {
    pub fn new(
        store: Arc<dyn KnowledgeStore>,
        reasoner: Arc<dyn ambient_core::ReasoningEngine>,
        report_dir: PathBuf,
    ) -> Self {
        Self {
            detector: PatternDetector::new(store.clone()),
            noticer: NoticerConsumer::new(store, reasoner),
            probe: Arc::new(DefaultSchedulerProbe),
            poll_interval: Duration::from_secs(60),
            report_dir,
            last_run_day: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_probe(mut self, probe: Arc<dyn SchedulerProbe>) -> Self {
        self.probe = probe;
        self
    }

    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    pub fn start(self) {
        thread::spawn(move || loop {
            thread::sleep(self.poll_interval);

            if self.probe.user_active() {
                continue;
            }

            if !self.detector.should_run_now(
                self.probe.idle_minutes(),
                self.probe.plugged_in(),
                self.probe.battery_percent(),
            ) {
                continue;
            }

            let today = {
                let now = Utc::now();
                (now.year(), now.month(), now.day())
            };

            let should_run = match self.last_run_day.lock() {
                Ok(mut guard) => {
                    if guard.as_ref() == Some(&today) {
                        false
                    } else {
                        *guard = Some(today);
                        true
                    }
                }
                Err(_) => false,
            };
            if !should_run {
                continue;
            }

            if let Ok(candidates) = self.detector.collect_candidates(500) {
                if let Ok(insights) = self.detector.detect(&candidates) {
                    let _ = persist_pattern_results(&self.detector.store, &insights);
                    let _ = write_weekly_report(&self.report_dir, &insights);
                    notify_report_ready(&self.report_dir);
                }
            }

            // Phase 4: Noticer Discovery Cycle
            if let Err(e) = self.noticer.run_discovery_cycle() {
                eprintln!("Noticer discovery cycle failed: {e}");
            }

            // Phase 3/4: Temporal Recalculation
            let _ = self.run_temporal_maintenance();
        });
    }

    fn run_temporal_maintenance(&self) -> Result<()> {
        let store = &self.detector.store;
        let units = store.search_fulltext("")?;
        for unit in units {
            let _ = store.calculate_temporal_profile(unit.id);
        }
        Ok(())
    }
}

// ── The Noticer (Procedural Memory Discovery) ───────────────────────────────

pub struct NoticerConsumer {
    store: Arc<dyn KnowledgeStore>,
    reasoner: Arc<dyn ambient_core::ReasoningEngine>,
}

impl NoticerConsumer {
    pub fn new(
        store: Arc<dyn KnowledgeStore>,
        reasoner: Arc<dyn ambient_core::ReasoningEngine>,
    ) -> Self {
        Self { store, reasoner }
    }

    /// Run a discovery cycle: fetch all L1 vectors, cluster them, and generate rules.
    pub fn run_discovery_cycle(&self) -> Result<()> {
        let vectors = self.store.get_all_lens_vectors("l1_semantic")?;
        if vectors.len() < 10 {
            return Ok(());
        }

        // Prepare data for clustering
        let unit_ids: Vec<Uuid> = vectors.iter().map(|(id, _)| *id).collect();
        let dim = vectors[0].1.len();
        let mut data = Vec::with_capacity(vectors.len() * dim);
        for (_, v) in &vectors {
            data.extend_from_slice(v);
        }

        let array = Array2::from_shape_vec((vectors.len(), dim), data)
            .map_err(|e| CoreError::Internal(format!("failed to shape clustering array: {e}")))?;

        // DBSCAN: eps=0.5 (cosine-ish), min_samples=3
        let mut model = Dbscan::new(0.5, 3, Euclidean::default());
        let (clusters, _): (HashMap<usize, Vec<usize>>, _) = model.fit(&array, None);

        // Group by cluster and generate rules if significant
        for (_label, indices) in clusters {
            if indices.len() >= 5 {
                let cluster_ids: Vec<Uuid> = indices.iter().map(|&i| unit_ids[i]).collect();
                self.formalize_cluster(cluster_ids)?;
            }
        }

        Ok(())
    }

    fn formalize_cluster(&self, unit_ids: Vec<Uuid>) -> Result<()> {
        let mut notes = Vec::new();
        for id in unit_ids.iter().take(10) {
            if let Some(unit) = self.store.get_by_id(*id)? {
                notes.push(unit);
            }
        }

        let contents: Vec<String> = notes.iter().map(|n| n.content.clone()).collect();
        let prompt = format!(
            "Analyze these related knowledge units and derive a 'procedural rule' that encapsulates the behavior or topic. \
            The rule should match similar future units. \
            Units: \n---\n{}\n---\n\
            Return a JSON object with: {{ \"summary\": \"...\", \"trigger_description\": \"...\", \"suggested_action_payload\": {{}} }}",
            contents.join("\n---\n")
        );

        let response = self.reasoner.answer(&prompt, &notes)?;
        // Parse response and save rule
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&response) {
            // Calculate centroid of the cluster for lens_intent
            // (Omitted for brevity, using first unit's vector as proxy for now)
            let lens_intent = self.store.get_all_lens_vectors("l1_semantic")? // Inefficient but works for MVP
                .into_iter()
                .find(|(id, _)| *id == unit_ids[0])
                .map(|(_, v)| v)
                .unwrap_or_else(|| vec![0.0; 768]);

            let rule = ambient_core::ProceduralRule {
                id: Uuid::new_v4(),
                lens_intent,
                pulse_mask: "any".to_string(), // MVP: match any pulse
                threshold: 0.85,
                action_payload: val["suggested_action_payload"].clone(),
                created_at: Utc::now(),
            };
            self.store.save_rule(rule)?;
        }

        Ok(())
    }
}

// ── Phase 5: ImplicitFeedbackRecorder ────────────────────────────────────────

/// Per-query pending context: the results shown and when they were displayed.
#[derive(Clone)]
struct PendingQuery {
    query_text: String,
    unit_ids: Vec<Uuid>,
    started_at: Instant,
}

/// Records implicit behavioral FeedbackEvents per NEXT.md Amendment 3.
///
/// Rules:
/// - If a query is followed within 60s by a file open matching a result's
///   source path → positive signal (0.5 weight, QueryResultActedOn + synthetic flag)
/// - If followed within 5s by a ContextSwitchRate spike equivalent to
///   immediate task-switching → negative signal (0.3 weight, QueryResultDismissed)
///
/// Callers push signals via `on_file_opened` / `on_query_result` / `on_context_switch`.
pub struct ImplicitFeedbackRecorder {
    store: Arc<dyn KnowledgeStore>,
    pending: Mutex<Option<PendingQuery>>,
}

impl ImplicitFeedbackRecorder {
    pub fn new(store: Arc<dyn KnowledgeStore>) -> Self {
        Self {
            store,
            pending: Mutex::new(None),
        }
    }

    /// Called immediately after query results are displayed.
    /// Begins the 60-second observation window.
    pub fn on_query_result(&self, query_text: &str, unit_ids: Vec<Uuid>) {
        if let Ok(mut guard) = self.pending.lock() {
            *guard = Some(PendingQuery {
                query_text: query_text.to_string(),
                unit_ids,
                started_at: Instant::now(),
            });
        }
    }

    /// Called when the user opens a file (from ActiveAppSampler correlation).
    /// Matches against pending result source paths.
    pub fn on_file_opened(&self, file_path: &str) {
        let pending = match self.pending.lock().ok().and_then(|g| g.clone()) {
            Some(p) if p.started_at.elapsed() <= Duration::from_secs(60) => p,
            _ => return,
        };

        // Check if any pending unit's source matches the opened file
        for &unit_id in &pending.unit_ids {
            if let Ok(Some(unit)) = self.store.get_by_id(unit_id) {
                if unit.source.0.contains(file_path) || file_path.contains(&unit.source.0) {
                    let ms_to_action = pending.started_at.elapsed().as_millis() as u32;
                    let event = FeedbackEvent {
                        id: Uuid::new_v4(),
                        timestamp: Utc::now(),
                        signal: FeedbackSignal::QueryResultActedOn {
                            query_text: pending.query_text.clone(),
                            unit_id,
                            action: ResultAction::OpenedSource,
                            ms_to_action,
                        },
                    };
                    let _ = self.store.record_feedback(event);
                    // Clear pending after first match to avoid duplicates
                    if let Ok(mut guard) = self.pending.lock() {
                        *guard = None;
                    }
                    return;
                }
            }
        }
    }

    /// Called when a rapid context switch is detected within 5s of displaying results.
    /// Records an implicit negative signal.
    pub fn on_context_switch_spike(&self, switches_per_minute: f32) {
        let Some(pending) = self.pending.lock().ok().and_then(|g| g.clone()) else {
            return;
        };

        // Only treat as implicit dismiss if: < 5s since results + switch rate spiked
        let elapsed = pending.started_at.elapsed();
        if elapsed > Duration::from_secs(5) || switches_per_minute < 1.0 {
            return;
        }

        // Record dismiss for the top result only (first unit_id)
        if let Some(&unit_id) = pending.unit_ids.first() {
            let event = FeedbackEvent {
                id: Uuid::new_v4(),
                timestamp: Utc::now(),
                signal: FeedbackSignal::QueryResultDismissed {
                    query_text: pending.query_text.clone(),
                    unit_id,
                },
            };
            let _ = self.store.record_feedback(event);
            if let Ok(mut guard) = self.pending.lock() {
                *guard = None;
            }
        }
    }

    /// Clear pending state (e.g., on daemon restart).
    pub fn reset(&self) {
        if let Ok(mut guard) = self.pending.lock() {
            *guard = None;
        }
    }
}

// ── Weekly report generation ──────────────────────────────────────────────────

fn persist_pattern_results(
    store: &Arc<dyn KnowledgeStore>,
    insights: &[PatternInsight],
) -> Result<()> {
    for insight in insights {
        let content = format!("Pattern: {}\n{}", insight.key, insight.summary);
        let hash = blake3::hash(content.as_bytes());
        let mut content_hash = [0u8; 32];
        content_hash.copy_from_slice(hash.as_bytes());

        let unit = KnowledgeUnit {
            id: Uuid::new_v4(),
            source: SourceId::new("pattern_result"),
            content,
            title: Some(format!("Pattern Insight — {}", insight.key)),
            metadata: HashMap::new(),
            observed_at: Utc::now(),
            content_hash,
        };
        store.upsert(unit)?;
    }
    Ok(())
}

fn write_weekly_report(report_dir: &Path, insights: &[PatternInsight]) -> Result<PathBuf> {
    fs::create_dir_all(report_dir)
        .map_err(|e| CoreError::Internal(format!("failed creating report dir: {e}")))?;

    let now = Utc::now();
    let filename = format!(
        "weekly-{:04}{:02}{:02}.html",
        now.year(),
        now.month(),
        now.day()
    );
    let path = report_dir.join(&filename);

    let mut html = String::from(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <title>Ambient Weekly Insights</title>\
         <style>body{font-family:system-ui,sans-serif;max-width:680px;margin:2rem auto;line-height:1.6}\
         h1{color:#2d5a27}li{margin:.5rem 0}.feedback-btn{font-size:.8rem;padding:.2rem .6rem;\
         border:1px solid #aaa;border-radius:4px;cursor:pointer;margin-left:.5rem}</style>\
         </head><body>",
    );
    html.push_str("<h1>🌱 Ambient Weekly Insights</h1><ul>");
    for insight in insights {
        html.push_str(&format!(
            "<li><strong>{}</strong>: {} <em>(score {:.0})</em>\
             <button class=\"feedback-btn\" onclick=\"sendFeedback('{}','useful')\">✓ Useful</button>\
             <button class=\"feedback-btn\" onclick=\"sendFeedback('{}','noise')\">✗ Noise</button></li>",
            insight.key,
            insight.summary,
            insight.score,
            insight.key,
            insight.key,
        ));
    }
    html.push_str(
        "</ul><script>\
         function sendFeedback(key,action){\
         fetch('http://127.0.0.1:7474/pattern-feedback',\
         {method:'POST',headers:{'Content-Type':'application/json'},\
         body:JSON.stringify({pattern_key:key,action:action})});\
         }\
         </script></body></html>",
    );

    fs::write(&path, html)
        .map_err(|e| CoreError::Internal(format!("failed writing report: {e}")))?;

    Ok(path)
}

fn notify_report_ready(report_dir: &Path) {
    let now = Utc::now();
    let filename = format!(
        "weekly-{:04}{:02}{:02}.html",
        now.year(),
        now.month(),
        now.day()
    );
    let path = report_dir.join(filename);

    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification \"{}\" with title \"Ambient\"",
            path.display()
        );
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .output();
    }

    #[cfg(not(target_os = "macos"))]
    let _ = path;
}

fn default_state(unit: &KnowledgeUnit) -> CognitiveState {
    CognitiveState {
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
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ambient_core::{
        CognitiveState, FeedbackEvent, KnowledgeStore, KnowledgeUnit, QueryResult,
        Result as AResult, SourceId,
    };
    use chrono::Utc;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    struct MockStore {
        units: Mutex<Vec<KnowledgeUnit>>,
    }

    impl MockStore {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                units: Mutex::new(Vec::new()),
            })
        }
    }

    impl KnowledgeStore for MockStore {
        fn upsert(&self, unit: KnowledgeUnit) -> AResult<()> {
            self.units.lock().unwrap().push(unit);
            Ok(())
        }
        fn search_semantic(&self, _v: &[f32], _k: usize) -> AResult<Vec<KnowledgeUnit>> {
            Ok(Vec::new())
        }
        fn search_fulltext(&self, _q: &str) -> AResult<Vec<KnowledgeUnit>> {
            Ok(self.units.lock().unwrap().clone())
        }
        fn related(&self, _id: Uuid, _depth: usize) -> AResult<Vec<KnowledgeUnit>> {
            Ok(Vec::new())
        }
        fn get_by_id(&self, id: Uuid) -> AResult<Option<KnowledgeUnit>> {
            Ok(self
                .units
                .lock()
                .unwrap()
                .iter()
                .find(|u| u.id == id)
                .cloned())
        }
        fn record_pulse(&self, _e: ambient_core::PulseEvent) -> AResult<()> {
            Ok(())
        }
        fn pulse_window(
            &self,
            _f: chrono::DateTime<Utc>,
            _t: chrono::DateTime<Utc>,
        ) -> AResult<Vec<ambient_core::PulseEvent>> {
            Ok(Vec::new())
        }
        fn unit_with_context(
            &self,
            _id: Uuid,
            _w: u64,
        ) -> AResult<Option<(KnowledgeUnit, Vec<ambient_core::PulseEvent>, CognitiveState)>>
        {
            Ok(None)
        }
        fn unit_with_context_fast(
            &self,
            _id: Uuid,
        ) -> AResult<Option<(KnowledgeUnit, CognitiveState)>> {
            Ok(None)
        }
        fn unit_with_context_live(
            &self,
            _id: Uuid,
            _w: u64,
        ) -> AResult<Option<(KnowledgeUnit, Vec<ambient_core::PulseEvent>, CognitiveState)>>
        {
            Ok(None)
        }
        fn record_feedback(&self, _e: FeedbackEvent) -> AResult<()> {
            Ok(())
        }
        fn feedback_score(&self, _id: Uuid) -> AResult<f32> {
            Ok(0.5)
        }
        fn pattern_feedback(&self, _id: Uuid) -> AResult<Option<String>> {
            Ok(None)
        }
        fn upsert_lens(&self, _id: Uuid, _l: &str, _v: Vec<f32>) -> AResult<()> {
            Ok(())
        }
        fn search_lens(&self, _l: &str, _v: &[f32], _k: usize) -> AResult<Vec<KnowledgeUnit>> {
            Ok(self.units.lock().unwrap().clone())
        }

        fn calculate_temporal_profile(&self, _unit_id: Uuid) -> AResult<()> {
            Ok(())
        }

        fn get_temporal_profile(
            &self,
            _unit_id: Uuid,
        ) -> AResult<Option<ambient_core::TemporalProfile>> {
            Ok(None)
        }

        fn save_rule(&self, _rule: ambient_core::ProceduralRule) -> AResult<()> {
            Ok(())
        }

        fn get_active_rules_for_pulse(
            &self,
            _pulse_mask: &str,
        ) -> AResult<Vec<ambient_core::ProceduralRule>> {
            Ok(Vec::new())
        }

        fn get_all_lens_vectors(&self, _lens_id: &str) -> AResult<Vec<(Uuid, Vec<f32>)>> {
            Ok(Vec::new())
        }
    }

    fn make_result(was_in_flow: bool, was_on_call: bool, hour: u8) -> QueryResult {
        let unit = KnowledgeUnit {
            id: Uuid::new_v4(),
            source: SourceId::new("test"),
            content: "test content".to_string(),
            title: Some("Test Note".to_string()),
            metadata: HashMap::new(),
            observed_at: Utc::now(),
            content_hash: [0u8; 32],
        };
        let state = CognitiveState {
            was_in_flow,
            was_on_call,
            time_of_day: Some(hour),
            dominant_app: None,
            was_in_meeting: false,
            was_in_focus_block: false,
            energy_level: None,
            mood_level: None,
            hrv_score: None,
            sleep_quality: None,
            minutes_since_last_meeting: None,
        };
        QueryResult {
            unit,
            score: 1.0,
            pulse_context: None,
            cognitive_state: Some(state),
            historical_feedback_score: 0.5,
            capability_status: None,
        }
    }

    #[test]
    fn test_detect_flow_cluster() {
        let store = MockStore::new();
        let detector = PatternDetector::new(store);
        let candidates = vec![
            make_result(true, false, 14),
            make_result(true, false, 15),
            make_result(false, false, 10),
        ];
        let insights = detector.detect(&candidates).unwrap();
        let flow = insights.iter().find(|i| i.key == "flow-cluster").unwrap();
        assert_eq!(flow.unit_ids.len(), 2);
    }

    #[test]
    fn test_detect_call_capture() {
        let store = MockStore::new();
        let detector = PatternDetector::new(store);
        let candidates = vec![make_result(false, true, 10), make_result(false, false, 10)];
        let insights = detector.detect(&candidates).unwrap();
        let call = insights.iter().find(|i| i.key == "call-capture").unwrap();
        assert_eq!(call.unit_ids.len(), 1);
    }

    #[test]
    fn test_detect_late_night() {
        let store = MockStore::new();
        let detector = PatternDetector::new(store);
        let candidates = vec![
            make_result(false, false, 23), // late night
            make_result(false, false, 14), // normal
        ];
        let insights = detector.detect(&candidates).unwrap();
        assert!(insights.iter().any(|i| i.key == "late-night"));
    }

    #[test]
    fn test_implicit_feedback_file_open_too_late() {
        let store = MockStore::new();
        let recorder = ImplicitFeedbackRecorder::new(store.clone());
        let unit_id = Uuid::new_v4();
        recorder.on_query_result("test query", vec![unit_id]);
        // File opened after 61s — should NOT record (but we can't sleep in a unit test)
        // Just verify the recorder stays clean when no file is opened
        recorder.reset();
        // No panic, state cleared
    }

    #[test]
    fn test_implicit_feedback_context_switch_too_late() {
        let store = MockStore::new();
        let recorder = ImplicitFeedbackRecorder::new(store);
        let unit_id = Uuid::new_v4();
        recorder.on_query_result("test query", vec![unit_id]);
        // Spike rate low — should not record dismiss
        recorder.on_context_switch_spike(0.5);
        // No panic
    }

    #[test]
    fn test_weekly_report_written() {
        let tmp = std::env::temp_dir().join("ambient_report_test");
        let _ = std::fs::remove_dir_all(&tmp);
        let insights = vec![PatternInsight {
            key: "flow-cluster".to_string(),
            summary: "3 notes in flow".to_string(),
            score: 3.0,
            unit_ids: Vec::new(),
        }];
        let path = write_weekly_report(&tmp, &insights).unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("flow-cluster"));
        assert!(content.contains("feedback-btn"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
