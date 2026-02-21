use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ambient_core::{
    CognitiveState, KnowledgeStore, KnowledgeUnit, QueryRequest, QueryResult, Result, SourceId,
};
use chrono::{Datelike, Timelike, Utc};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct PatternInsight {
    pub key: String,
    pub summary: String,
    pub score: f32,
}

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

pub struct PatternDetector {
    store: Arc<dyn KnowledgeStore>,
}

impl PatternDetector {
    pub fn new(store: Arc<dyn KnowledgeStore>) -> Self {
        Self { store }
    }

    pub fn detect(&self, candidates: &[QueryResult]) -> Result<Vec<PatternInsight>> {
        let mut out = Vec::new();

        let flow_count = candidates
            .iter()
            .filter(|item| item.cognitive_state.as_ref().is_some_and(|s| s.was_in_flow))
            .count();
        if flow_count > 0 {
            out.push(PatternInsight {
                key: "flow-cluster".to_string(),
                summary: format!("{flow_count} notes were captured during flow"),
                score: flow_count as f32,
            });
        }

        let on_call_count = candidates
            .iter()
            .filter(|item| item.cognitive_state.as_ref().is_some_and(|s| s.was_on_call))
            .count();
        if on_call_count > 0 {
            out.push(PatternInsight {
                key: "call-capture".to_string(),
                summary: format!("{on_call_count} notes were captured during calls"),
                score: on_call_count as f32,
            });
        }

        let late_night_count = candidates
            .iter()
            .filter(|item| {
                item.cognitive_state
                    .as_ref()
                    .and_then(|s| s.time_of_day)
                    .is_some_and(|hour| !(6..22).contains(&hour))
            })
            .count();
        if late_night_count > 0 {
            out.push(PatternInsight {
                key: "late-night".to_string(),
                summary: format!("{late_night_count} notes were captured late at night"),
                score: late_night_count as f32,
            });
        }

        // Topic co-occurrence approximation using tags in metadata.
        let mut tag_counts: HashMap<String, usize> = HashMap::new();
        for item in candidates {
            if let Some(tags) = item.unit.metadata.get("tags").and_then(|v| v.as_array()) {
                for tag in tags.iter().filter_map(|v| v.as_str()) {
                    *tag_counts.entry(tag.to_string()).or_insert(0) += 1;
                }
            }
        }
        if let Some((top_tag, count)) = tag_counts.into_iter().max_by_key(|(_, c)| *c) {
            out.push(PatternInsight {
                key: "topic-cooccurrence".to_string(),
                summary: format!("Topic '{top_tag}' appears in {count} clustered notes"),
                score: count as f32,
            });
        }

        let revisit = self.revisit_candidates()?;
        if !revisit.is_empty() {
            out.push(PatternInsight {
                key: "revisit-candidates".to_string(),
                summary: format!(
                    "{} highly stale notes are candidates for revisit",
                    revisit.len()
                ),
                score: revisit.len() as f32,
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
        idle_minutes > 10 && (plugged_in || battery_percent > 80.0) && (1..6).contains(&hour)
    }

    pub fn revisit_candidates(&self) -> Result<Vec<uuid::Uuid>> {
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
}

pub struct PatternScheduler {
    detector: PatternDetector,
    probe: Arc<dyn SchedulerProbe>,
    poll_interval: Duration,
    report_dir: PathBuf,
    last_run_day: Arc<Mutex<Option<(i32, u32, u32)>>>,
}

impl PatternScheduler {
    pub fn new(store: Arc<dyn KnowledgeStore>, report_dir: PathBuf) -> Self {
        Self {
            detector: PatternDetector::new(store),
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
        });
    }
}

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
            embedding: None,
            observed_at: Utc::now(),
            content_hash,
        };
        store.upsert(unit)?;
    }
    Ok(())
}

fn write_weekly_report(report_dir: &Path, insights: &[PatternInsight]) -> Result<PathBuf> {
    fs::create_dir_all(report_dir).map_err(|e| {
        ambient_core::CoreError::Internal(format!("failed creating report dir: {e}"))
    })?;

    let now = Utc::now();
    let filename = format!(
        "weekly-{:04}{:02}{:02}.html",
        now.year(),
        now.month(),
        now.day()
    );
    let path = report_dir.join(filename);

    let mut html = String::from(
        "<html><head><meta charset=\"utf-8\"><title>Ambient Weekly Insights</title></head><body>",
    );
    html.push_str("<h1>Ambient Weekly Insights</h1><ul>");
    for insight in insights {
        html.push_str(&format!(
            "<li><strong>{}</strong>: {} (score {:.2})</li>",
            insight.key, insight.summary, insight.score
        ));
    }
    html.push_str("</ul></body></html>");

    fs::write(&path, html)
        .map_err(|e| ambient_core::CoreError::Internal(format!("failed writing report: {e}")))?;

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
    {
        let _ = path;
    }
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
