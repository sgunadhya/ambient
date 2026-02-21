use std::collections::HashMap;
use std::path::Path;

use ambient_core::{CoreError, KnowledgeUnit, Normalizer, RawEvent, RawPayload, Result, SourceId};
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
        }
    }

    fn normalize(&self, event: RawEvent) -> Result<KnowledgeUnit> {
        let (content, path) = match event.payload {
            RawPayload::Markdown { content, path } => (content, path),
            RawPayload::AppleNote { .. } => {
                return Err(CoreError::InvalidInput("expected markdown payload".to_string()))
            }
            RawPayload::SpotlightItem { .. } => {
                return Err(CoreError::InvalidInput("expected markdown payload".to_string()))
            }
            RawPayload::PlainText { .. } => {
                return Err(CoreError::InvalidInput("expected markdown payload".to_string()))
            }
            RawPayload::HealthKitSample { .. }
            | RawPayload::CalendarEvent { .. }
            | RawPayload::SelfReport { .. } => {
                return Err(CoreError::InvalidInput("expected markdown payload".to_string()))
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
                return Err(CoreError::InvalidInput("expected apple note payload".to_string()))
            }
            RawPayload::SpotlightItem { .. } => {
                return Err(CoreError::InvalidInput("expected apple note payload".to_string()))
            }
            RawPayload::PlainText { .. } => {
                return Err(CoreError::InvalidInput("expected apple note payload".to_string()))
            }
            RawPayload::HealthKitSample { .. }
            | RawPayload::CalendarEvent { .. }
            | RawPayload::SelfReport { .. } => {
                return Err(CoreError::InvalidInput("expected apple note payload".to_string()))
            }
        };

        let content = String::from_utf8_lossy(blob.as_ref()).to_string();
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
                return Err(CoreError::InvalidInput("expected spotlight payload".to_string()))
            }
            RawPayload::AppleNote { .. } => {
                return Err(CoreError::InvalidInput("expected spotlight payload".to_string()))
            }
            RawPayload::PlainText { .. } => {
                return Err(CoreError::InvalidInput("expected spotlight payload".to_string()))
            }
            RawPayload::HealthKitSample { .. }
            | RawPayload::CalendarEvent { .. }
            | RawPayload::SelfReport { .. } => {
                return Err(CoreError::InvalidInput("expected spotlight payload".to_string()))
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
        }
    }

    fn normalize(&self, event: RawEvent) -> Result<KnowledgeUnit> {
        let (content, path) = match event.payload {
            RawPayload::PlainText { content, path } => (content, path),
            RawPayload::Markdown { .. } => {
                return Err(CoreError::InvalidInput("expected plaintext payload".to_string()))
            }
            RawPayload::AppleNote { .. } => {
                return Err(CoreError::InvalidInput("expected plaintext payload".to_string()))
            }
            RawPayload::SpotlightItem { .. } => {
                return Err(CoreError::InvalidInput("expected plaintext payload".to_string()))
            }
            RawPayload::HealthKitSample { .. }
            | RawPayload::CalendarEvent { .. }
            | RawPayload::SelfReport { .. } => {
                return Err(CoreError::InvalidInput("expected plaintext payload".to_string()))
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
            | RawPayload::SelfReport { .. } => false,
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
            | RawPayload::SelfReport { .. } => {
                return Err(CoreError::InvalidInput(
                    "expected healthkit sample payload".to_string(),
                ))
            }
        };

        let metric_name = format!("{metric:?}");
        let title = Some(format!("{} reading — {}", metric_name, recorded_at.date_naive()));
        let content = format!("{metric_name}: {value}{unit}");

        let mut metadata = HashMap::new();
        metadata.insert("metric".to_string(), Value::String(metric_name.clone()));
        metadata.insert("value".to_string(), Value::from(value));
        metadata.insert("unit".to_string(), Value::String(unit.clone()));

        Ok(KnowledgeUnit {
            id: Uuid::new_v4(),
            source: event.source,
            content_hash: hash_content(&format!("{metric_name}:{}", recorded_at.timestamp_millis())),
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
            | RawPayload::SelfReport { .. } => false,
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
                | RawPayload::SelfReport { .. } => {
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
        metadata.insert("attendee_count".to_string(), Value::from(attendee_count as u64));
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
            | RawPayload::CalendarEvent { .. } => false,
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
            | RawPayload::CalendarEvent { .. } => {
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

fn first_h1(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|line| line.strip_prefix("# ").map(|v| v.trim().to_string()))
        .filter(|s| !s.is_empty())
}

fn markdown_to_text(content: &str) -> String {
    content
        .replace('#', "")
        .replace("**", "")
        .replace('*', "")
        .replace('`', "")
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
    path.file_stem().map(|stem| stem.to_string_lossy().to_string())
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
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    let value = hasher.finish();
    let mut out = [0u8; 32];
    for idx in 0..4 {
        out[idx * 8..(idx + 1) * 8].copy_from_slice(&value.to_le_bytes());
    }
    out
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
    ])
}

pub fn make_raw_event(payload: RawPayload) -> RawEvent {
    RawEvent {
        source: SourceId::new("filesystem"),
        timestamp: Utc::now(),
        payload,
    }
}
