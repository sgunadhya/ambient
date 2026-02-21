pub mod applenotes;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use ambient_core::{
    ConsumerId, CoreError, EventLogEntry, KnowledgeStore, KnowledgeUnit, Normalizer, Offset,
    RawEvent, RawPayload, Result, SourceId, StreamProvider,
};
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

pub struct NormalizerDispatch {
    normalizers: Vec<Box<dyn Normalizer>>,
}

impl NormalizerDispatch {
    pub fn new(normalizers: Vec<Box<dyn Normalizer>>) -> Self {
        Self { normalizers }
    }

    pub fn normalize(&self, event: RawEvent) -> Result<KnowledgeUnit> {
        for normalizer in &self.normalizers {
            if normalizer.can_handle(&event.payload) {
                return normalizer.normalize(event);
            }
        }
        Err(CoreError::Unsupported("no matching normalizer for payload"))
    }
}

pub struct NormalizerConsumer {
    provider: Arc<dyn StreamProvider>,
    store: Arc<dyn KnowledgeStore>,
    dispatch: Arc<NormalizerDispatch>,
    consumer_id: ConsumerId,
    offset: Mutex<Offset>,
}

impl NormalizerConsumer {
    pub fn new(
        provider: Arc<dyn StreamProvider>,
        store: Arc<dyn KnowledgeStore>,
        dispatch: Arc<NormalizerDispatch>,
        consumer_id: ConsumerId,
    ) -> Result<Self> {
        let offset = provider.last_committed(&consumer_id)?.unwrap_or(0);
        Ok(Self {
            provider,
            store,
            dispatch,
            consumer_id,
            offset: Mutex::new(offset),
        })
    }

    pub fn poll_once(&self, limit: usize) -> Result<Vec<KnowledgeUnit>> {
        let mut offset = self.offset.lock().map_err(|_| {
            CoreError::Internal("normalizer consumer offset lock poisoned".to_string())
        })?;
        let batch = self.provider.read(&self.consumer_id, *offset, limit)?;
        let mut out = Vec::new();

        for (next_offset, entry) in batch {
            match entry {
                EventLogEntry::Raw(raw) => match raw.payload.clone() {
                    RawPayload::FeedbackEventSync { event, .. } => {
                        let _ = self.store.record_feedback(*event);
                    }
                    RawPayload::KnowledgeUnitSync { unit, .. } => {
                        if self.store.upsert((*unit).clone()).is_ok() {
                            out.push(*unit);
                        }
                    }
                    _ => {
                        if let Ok(unit) = self.dispatch.normalize(raw) {
                            if self.store.upsert(unit.clone()).is_ok() {
                                out.push(unit);
                            }
                        }
                    }
                },
                EventLogEntry::Pulse(_) => {}
            }
            self.provider.commit(&self.consumer_id, next_offset)?;
            *offset = next_offset;
        }

        Ok(out)
    }
}

#[derive(Default)]
pub struct MarkdownNormalizer;

impl Normalizer for MarkdownNormalizer {
    fn can_handle(&self, payload: &RawPayload) -> bool {
        match payload {
            RawPayload::Markdown { .. } => true,
            RawPayload::AppleNote { .. } => false,
            RawPayload::SpotlightItem { .. } => false,
            RawPayload::PlainText { .. } => false,
            RawPayload::HealthKitSample { .. } => false,
            RawPayload::CalendarEvent { .. } => false,
            RawPayload::SelfReport { .. } => false,
            RawPayload::BehavioralSummary { .. } => false,
            RawPayload::KnowledgeUnitSync { .. } => false,
            RawPayload::FeedbackEventSync { .. } => false,
        }
    }

    fn normalize(&self, event: RawEvent) -> Result<KnowledgeUnit> {
        let (content, path) = match event.payload {
            RawPayload::Markdown { content, path } => (content, path),
            RawPayload::AppleNote { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected markdown payload".to_string(),
                ))
            }
            RawPayload::SpotlightItem { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected markdown payload".to_string(),
                ))
            }
            RawPayload::PlainText { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected markdown payload".to_string(),
                ))
            }
            RawPayload::HealthKitSample { .. }
            | RawPayload::CalendarEvent { .. }
            | RawPayload::SelfReport { .. }
            | RawPayload::BehavioralSummary { .. }
            | RawPayload::KnowledgeUnitSync { .. }
            | RawPayload::FeedbackEventSync { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected markdown payload".to_string(),
                ))
            }
        };

        let title = first_h1(&content).or_else(|| stem(&path));
        let links = extract_wikilinks(&content);
        let tags = extract_tags(&content);
        let plain = markdown_to_text(&content);

        let mut metadata = HashMap::new();
        metadata.insert(
            "path".to_string(),
            Value::String(path.to_string_lossy().to_string()),
        );
        metadata.insert("links".to_string(), json!(links));
        metadata.insert("tags".to_string(), json!(tags));

        Ok(KnowledgeUnit {
            id: Uuid::new_v4(),
            source: event.source,
            content_hash: hash_content(&plain),
            content: plain,
            title,
            metadata,
            embedding: None,
            observed_at: event.timestamp,
        })
    }
}

#[derive(Default)]
pub struct AppleNotesNormalizer;

impl Normalizer for AppleNotesNormalizer {
    fn can_handle(&self, payload: &RawPayload) -> bool {
        match payload {
            RawPayload::Markdown { .. } => false,
            RawPayload::AppleNote { .. } => true,
            RawPayload::SpotlightItem { .. } => false,
            RawPayload::PlainText { .. } => false,
            RawPayload::HealthKitSample { .. } => false,
            RawPayload::CalendarEvent { .. } => false,
            RawPayload::SelfReport { .. } => false,
            RawPayload::BehavioralSummary { .. } => false,
            RawPayload::KnowledgeUnitSync { .. } => false,
            RawPayload::FeedbackEventSync { .. } => false,
        }
    }

    fn normalize(&self, event: RawEvent) -> Result<KnowledgeUnit> {
        let (blob, note_id, modified_at) = match event.payload {
            RawPayload::AppleNote {
                protobuf_blob,
                note_id,
                modified_at,
            } => (protobuf_blob, note_id, modified_at),
            RawPayload::Markdown { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected apple note payload".to_string(),
                ))
            }
            RawPayload::SpotlightItem { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected apple note payload".to_string(),
                ))
            }
            RawPayload::PlainText { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected apple note payload".to_string(),
                ))
            }
            RawPayload::HealthKitSample { .. }
            | RawPayload::CalendarEvent { .. }
            | RawPayload::SelfReport { .. }
            | RawPayload::BehavioralSummary { .. }
            | RawPayload::KnowledgeUnitSync { .. }
            | RawPayload::FeedbackEventSync { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected apple note payload".to_string(),
                ))
            }
        };

        use crate::applenotes::applenotes::Document;
        use prost::Message;

        let content = match Document::decode(blob.as_ref()) {
            Ok(doc) => {
                let mut text = String::new();
                for node in doc.nodes {
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&node.text);
                }
                text.trim().to_string()
            }
            Err(_) => {
                // Fallback to text extraction if schema is incomplete, making sure to strip unprintable chars.
                let raw = String::from_utf8_lossy(blob.as_ref()).into_owned();
                raw.chars()
                    .filter(|c| {
                        c.is_alphanumeric() || c.is_whitespace() || c.is_ascii_punctuation()
                    })
                    .collect::<String>()
            }
        };

        let mut metadata = HashMap::new();
        metadata.insert("apple_note_id".to_string(), Value::String(note_id));

        Ok(KnowledgeUnit {
            id: Uuid::new_v4(),
            source: event.source,
            content_hash: hash_content(&content),
            content,
            title: Some("Apple Note".to_string()),
            metadata,
            embedding: None,
            observed_at: modified_at,
        })
    }
}

#[derive(Default)]
pub struct SpotlightNormalizer;

impl Normalizer for SpotlightNormalizer {
    fn can_handle(&self, payload: &RawPayload) -> bool {
        match payload {
            RawPayload::Markdown { .. } => false,
            RawPayload::AppleNote { .. } => false,
            RawPayload::SpotlightItem { .. } => true,
            RawPayload::PlainText { .. } => false,
            RawPayload::HealthKitSample { .. } => false,
            RawPayload::CalendarEvent { .. } => false,
            RawPayload::SelfReport { .. } => false,
            RawPayload::BehavioralSummary { .. } => false,
            RawPayload::KnowledgeUnitSync { .. } => false,
            RawPayload::FeedbackEventSync { .. } => false,
        }
    }

    fn normalize(&self, event: RawEvent) -> Result<KnowledgeUnit> {
        let item = match event.payload {
            RawPayload::SpotlightItem {
                bundle_id,
                display_name,
                content_type,
                text_content,
                last_modified,
                file_url,
            } => (
                bundle_id,
                display_name,
                content_type,
                text_content,
                last_modified,
                file_url,
            ),
            RawPayload::Markdown { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected spotlight payload".to_string(),
                ))
            }
            RawPayload::AppleNote { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected spotlight payload".to_string(),
                ))
            }
            RawPayload::PlainText { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected spotlight payload".to_string(),
                ))
            }
            RawPayload::HealthKitSample { .. }
            | RawPayload::CalendarEvent { .. }
            | RawPayload::SelfReport { .. }
            | RawPayload::BehavioralSummary { .. }
            | RawPayload::KnowledgeUnitSync { .. }
            | RawPayload::FeedbackEventSync { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected spotlight payload".to_string(),
                ))
            }
        };

        let (bundle_id, display_name, content_type, text_content, last_modified, file_url) = item;
        let title = title_from_content_type(&content_type, &display_name);

        let mut metadata = HashMap::new();
        metadata.insert("content_type".to_string(), Value::String(content_type));
        metadata.insert("bundle_id".to_string(), Value::String(bundle_id));
        metadata.insert(
            "file_url".to_string(),
            file_url.map(Value::String).unwrap_or(Value::Null),
        );

        Ok(KnowledgeUnit {
            id: Uuid::new_v4(),
            source: event.source,
            content_hash: hash_content(&text_content),
            content: text_content,
            title: Some(title),
            metadata,
            embedding: None,
            observed_at: last_modified,
        })
    }
}

#[derive(Default)]
pub struct PlainTextNormalizer;

impl Normalizer for PlainTextNormalizer {
    fn can_handle(&self, payload: &RawPayload) -> bool {
        match payload {
            RawPayload::Markdown { .. } => false,
            RawPayload::AppleNote { .. } => false,
            RawPayload::SpotlightItem { .. } => false,
            RawPayload::PlainText { .. } => true,
            RawPayload::HealthKitSample { .. } => false,
            RawPayload::CalendarEvent { .. } => false,
            RawPayload::SelfReport { .. } => false,
            RawPayload::BehavioralSummary { .. } => false,
            RawPayload::KnowledgeUnitSync { .. } => false,
            RawPayload::FeedbackEventSync { .. } => false,
        }
    }

    fn normalize(&self, event: RawEvent) -> Result<KnowledgeUnit> {
        let (content, path) = match event.payload {
            RawPayload::PlainText { content, path } => (content, path),
            RawPayload::Markdown { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected plaintext payload".to_string(),
                ))
            }
            RawPayload::AppleNote { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected plaintext payload".to_string(),
                ))
            }
            RawPayload::SpotlightItem { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected plaintext payload".to_string(),
                ))
            }
            RawPayload::HealthKitSample { .. }
            | RawPayload::CalendarEvent { .. }
            | RawPayload::SelfReport { .. }
            | RawPayload::BehavioralSummary { .. }
            | RawPayload::KnowledgeUnitSync { .. }
            | RawPayload::FeedbackEventSync { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected plaintext payload".to_string(),
                ))
            }
        };

        let mut metadata = HashMap::new();
        metadata.insert(
            "path".to_string(),
            Value::String(path.to_string_lossy().to_string()),
        );

        Ok(KnowledgeUnit {
            id: Uuid::new_v4(),
            source: event.source,
            content_hash: hash_content(&content),
            content,
            title: stem(&path),
            metadata,
            embedding: None,
            observed_at: event.timestamp,
        })
    }
}

#[derive(Default)]
pub struct HealthKitNormalizer;

impl Normalizer for HealthKitNormalizer {
    fn can_handle(&self, payload: &RawPayload) -> bool {
        match payload {
            RawPayload::HealthKitSample { .. } => true,
            RawPayload::Markdown { .. }
            | RawPayload::AppleNote { .. }
            | RawPayload::SpotlightItem { .. }
            | RawPayload::PlainText { .. }
            | RawPayload::CalendarEvent { .. }
            | RawPayload::SelfReport { .. }
            | RawPayload::BehavioralSummary { .. }
            | RawPayload::KnowledgeUnitSync { .. }
            | RawPayload::FeedbackEventSync { .. } => false,
        }
    }

    fn normalize(&self, event: RawEvent) -> Result<KnowledgeUnit> {
        let (metric, value, unit, recorded_at) = match event.payload {
            RawPayload::HealthKitSample {
                metric,
                value,
                unit,
                recorded_at,
            } => (metric, value, unit, recorded_at),
            RawPayload::Markdown { .. }
            | RawPayload::AppleNote { .. }
            | RawPayload::SpotlightItem { .. }
            | RawPayload::PlainText { .. }
            | RawPayload::CalendarEvent { .. }
            | RawPayload::SelfReport { .. }
            | RawPayload::BehavioralSummary { .. }
            | RawPayload::KnowledgeUnitSync { .. }
            | RawPayload::FeedbackEventSync { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected healthkit sample payload".to_string(),
                ))
            }
        };

        let metric_name = format!("{metric:?}");
        let title = Some(format!(
            "{} reading — {}",
            metric_name,
            recorded_at.date_naive()
        ));
        let content = format!("{metric_name}: {value}{unit}");

        let mut metadata = HashMap::new();
        metadata.insert("metric".to_string(), Value::String(metric_name.clone()));
        metadata.insert("value".to_string(), Value::from(value));
        metadata.insert("unit".to_string(), Value::String(unit.clone()));

        Ok(KnowledgeUnit {
            id: Uuid::new_v4(),
            source: event.source,
            content_hash: hash_content(&format!(
                "{metric_name}:{}",
                recorded_at.timestamp_millis()
            )),
            content,
            title,
            metadata,
            embedding: None,
            observed_at: recorded_at,
        })
    }
}

#[derive(Default)]
pub struct CalendarNormalizer;

impl Normalizer for CalendarNormalizer {
    fn can_handle(&self, payload: &RawPayload) -> bool {
        match payload {
            RawPayload::CalendarEvent { .. } => true,
            RawPayload::Markdown { .. }
            | RawPayload::AppleNote { .. }
            | RawPayload::SpotlightItem { .. }
            | RawPayload::PlainText { .. }
            | RawPayload::HealthKitSample { .. }
            | RawPayload::SelfReport { .. }
            | RawPayload::BehavioralSummary { .. }
            | RawPayload::KnowledgeUnitSync { .. }
            | RawPayload::FeedbackEventSync { .. } => false,
        }
    }

    fn normalize(&self, event: RawEvent) -> Result<KnowledgeUnit> {
        let (title, start, duration_minutes, attendee_count, is_focus_block, calendar_name) =
            match event.payload {
                RawPayload::CalendarEvent {
                    title,
                    start,
                    end: _,
                    duration_minutes,
                    attendee_count,
                    is_focus_block,
                    calendar_name,
                } => (
                    title,
                    start,
                    duration_minutes,
                    attendee_count,
                    is_focus_block,
                    calendar_name,
                ),
                RawPayload::Markdown { .. }
                | RawPayload::AppleNote { .. }
                | RawPayload::SpotlightItem { .. }
                | RawPayload::PlainText { .. }
                | RawPayload::HealthKitSample { .. }
                | RawPayload::SelfReport { .. }
                | RawPayload::BehavioralSummary { .. }
                | RawPayload::KnowledgeUnitSync { .. }
                | RawPayload::FeedbackEventSync { .. } => {
                    return Err(CoreError::InvalidInput(
                        "expected calendar event payload".to_string(),
                    ))
                }
            };

        let content = format!(
            "Calendar event. Duration: {} minutes. Attendees: {}. Focus block: {}.",
            duration_minutes, attendee_count, is_focus_block
        );
        let mut metadata = HashMap::new();
        metadata.insert("is_focus_block".to_string(), Value::Bool(is_focus_block));
        metadata.insert(
            "duration_minutes".to_string(),
            Value::from(duration_minutes as u64),
        );
        metadata.insert(
            "attendee_count".to_string(),
            Value::from(attendee_count as u64),
        );
        metadata.insert("calendar_name".to_string(), Value::String(calendar_name));

        Ok(KnowledgeUnit {
            id: Uuid::new_v4(),
            source: event.source,
            content_hash: hash_content(&format!("{}:{}", title, start.timestamp_millis())),
            content,
            title: Some(title),
            metadata,
            embedding: None,
            observed_at: start,
        })
    }
}

#[derive(Default)]
pub struct SelfReportNormalizer;

impl Normalizer for SelfReportNormalizer {
    fn can_handle(&self, payload: &RawPayload) -> bool {
        match payload {
            RawPayload::SelfReport { .. } => true,
            RawPayload::Markdown { .. }
            | RawPayload::AppleNote { .. }
            | RawPayload::SpotlightItem { .. }
            | RawPayload::PlainText { .. }
            | RawPayload::HealthKitSample { .. }
            | RawPayload::CalendarEvent { .. }
            | RawPayload::BehavioralSummary { .. }
            | RawPayload::KnowledgeUnitSync { .. }
            | RawPayload::FeedbackEventSync { .. } => false,
        }
    }

    fn normalize(&self, event: RawEvent) -> Result<KnowledgeUnit> {
        let (energy, mood, note, reported_at) = match event.payload {
            RawPayload::SelfReport {
                energy,
                mood,
                note,
                reported_at,
            } => (energy, mood, note, reported_at),
            RawPayload::Markdown { .. }
            | RawPayload::AppleNote { .. }
            | RawPayload::SpotlightItem { .. }
            | RawPayload::PlainText { .. }
            | RawPayload::HealthKitSample { .. }
            | RawPayload::CalendarEvent { .. }
            | RawPayload::BehavioralSummary { .. }
            | RawPayload::KnowledgeUnitSync { .. }
            | RawPayload::FeedbackEventSync { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected self-report payload".to_string(),
                ))
            }
        };

        let content = format!(
            "Energy: {}/5, Mood: {}/5. {}",
            energy,
            mood,
            note.unwrap_or_default()
        );
        let mut metadata = HashMap::new();
        metadata.insert("energy".to_string(), Value::from(energy as u64));
        metadata.insert("mood".to_string(), Value::from(mood as u64));

        Ok(KnowledgeUnit {
            id: Uuid::new_v4(),
            source: event.source,
            content_hash: hash_content(&format!(
                "{}:{}:{}",
                energy,
                mood,
                reported_at.timestamp_millis()
            )),
            content,
            title: Some(format!("Check-in — {}", reported_at.date_naive())),
            metadata,
            embedding: None,
            observed_at: reported_at,
        })
    }
}

#[derive(Default)]
pub struct BehavioralSummaryNormalizer;

impl Normalizer for BehavioralSummaryNormalizer {
    fn can_handle(&self, payload: &RawPayload) -> bool {
        match payload {
            RawPayload::BehavioralSummary { .. } => true,
            RawPayload::Markdown { .. }
            | RawPayload::AppleNote { .. }
            | RawPayload::SpotlightItem { .. }
            | RawPayload::PlainText { .. }
            | RawPayload::HealthKitSample { .. }
            | RawPayload::CalendarEvent { .. }
            | RawPayload::SelfReport { .. }
            | RawPayload::KnowledgeUnitSync { .. }
            | RawPayload::FeedbackEventSync { .. } => false,
        }
    }

    fn normalize(&self, event: RawEvent) -> Result<KnowledgeUnit> {
        let (
            period_start,
            period_end,
            avg_context_switch_rate,
            peak_context_switch_rate,
            audio_active_seconds,
            dominant_app,
            app_switch_count,
            was_in_meeting,
            was_in_focus_block,
            device_origin,
        ) = match event.payload {
            RawPayload::BehavioralSummary {
                period_start,
                period_end,
                avg_context_switch_rate,
                peak_context_switch_rate,
                audio_active_seconds,
                dominant_app,
                app_switch_count,
                was_in_meeting,
                was_in_focus_block,
                device_origin,
            } => (
                period_start,
                period_end,
                avg_context_switch_rate,
                peak_context_switch_rate,
                audio_active_seconds,
                dominant_app,
                app_switch_count,
                was_in_meeting,
                was_in_focus_block,
                device_origin,
            ),
            RawPayload::Markdown { .. }
            | RawPayload::AppleNote { .. }
            | RawPayload::SpotlightItem { .. }
            | RawPayload::PlainText { .. }
            | RawPayload::HealthKitSample { .. }
            | RawPayload::CalendarEvent { .. }
            | RawPayload::SelfReport { .. }
            | RawPayload::KnowledgeUnitSync { .. }
            | RawPayload::FeedbackEventSync { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected behavioral-summary payload".to_string(),
                ))
            }
        };

        let mut metadata = HashMap::new();
        metadata.insert(
            "period_start".to_string(),
            Value::String(period_start.to_rfc3339()),
        );
        metadata.insert(
            "period_end".to_string(),
            Value::String(period_end.to_rfc3339()),
        );
        metadata.insert(
            "audio_active_seconds".to_string(),
            Value::from(audio_active_seconds as u64),
        );
        metadata.insert(
            "app_switch_count".to_string(),
            Value::from(app_switch_count as u64),
        );
        metadata.insert("was_in_meeting".to_string(), Value::Bool(was_in_meeting));
        metadata.insert(
            "was_in_focus_block".to_string(),
            Value::Bool(was_in_focus_block),
        );
        metadata.insert("device_origin".to_string(), Value::String(device_origin));
        metadata.insert(
            "dominant_app".to_string(),
            dominant_app
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );

        let content = format!(
            "Behavior window {}..{} avg_switch={} peak_switch={} audio_seconds={} app_switches={} dominant_app={}",
            period_start.to_rfc3339(),
            period_end.to_rfc3339(),
            avg_context_switch_rate,
            peak_context_switch_rate,
            audio_active_seconds,
            app_switch_count,
            dominant_app.unwrap_or_else(|| "unknown".to_string())
        );

        Ok(KnowledgeUnit {
            id: Uuid::new_v4(),
            source: event.source,
            content_hash: hash_content(&content),
            content,
            title: Some("Behavioral Summary".to_string()),
            metadata,
            embedding: None,
            observed_at: period_end,
        })
    }
}

fn first_h1(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|line| line.strip_prefix("# ").map(|v| v.trim().to_string()))
        .filter(|s| !s.is_empty())
}

fn markdown_to_text(content: &str) -> String {
    use pulldown_cmark::{Event, Parser};
    let parser = Parser::new(content);
    let mut plain_text = String::new();
    let mut last_was_text = false;
    for event in parser {
        match event {
            Event::Text(text) => {
                plain_text.push_str(&text);
                last_was_text = true;
            }
            Event::Code(code) => {
                plain_text.push_str(&code);
                last_was_text = true;
            }
            Event::SoftBreak | Event::HardBreak => {
                if last_was_text {
                    plain_text.push(' ');
                }
                last_was_text = false;
            }
            _ => {
                last_was_text = false;
            }
        }
    }
    plain_text.trim().to_string()
}

fn extract_wikilinks(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0usize;
    while i + 3 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j + 1 < bytes.len() {
                if bytes[j] == b']' && bytes[j + 1] == b']' {
                    out.push(content[i + 2..j].trim().to_string());
                    i = j + 2;
                    break;
                }
                j += 1;
            }
        }
        i += 1;
    }
    out.retain(|v| !v.is_empty());
    out
}

fn extract_tags(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in content.split_whitespace() {
        if let Some(tag) = token.strip_prefix('#') {
            let clean = tag.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            if !clean.is_empty() {
                out.push(clean.to_string());
            }
        }
    }
    out
}

fn stem(path: &Path) -> Option<String> {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
}

fn title_from_content_type(content_type: &str, fallback: &str) -> String {
    if content_type.contains("pdf") {
        return format!("PDF: {fallback}");
    }
    if content_type.contains("mail") {
        return format!("Mail: {fallback}");
    }
    if content_type.contains("text") {
        return fallback.to_string();
    }
    fallback.to_string()
}

fn hash_content(content: &str) -> [u8; 32] {
    blake3::hash(content.as_bytes()).into()
}

pub fn default_dispatch() -> NormalizerDispatch {
    NormalizerDispatch::new(vec![
        Box::<MarkdownNormalizer>::default(),
        Box::<AppleNotesNormalizer>::default(),
        Box::<SpotlightNormalizer>::default(),
        Box::<PlainTextNormalizer>::default(),
        Box::<HealthKitNormalizer>::default(),
        Box::<CalendarNormalizer>::default(),
        Box::<SelfReportNormalizer>::default(),
        Box::<BehavioralSummaryNormalizer>::default(),
    ])
}

pub fn make_raw_event(payload: RawPayload) -> RawEvent {
    RawEvent {
        source: SourceId::new("filesystem"),
        timestamp: Utc::now(),
        payload,
    }
}
