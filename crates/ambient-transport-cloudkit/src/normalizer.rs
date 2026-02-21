use ambient_core::{
    CoreError, FeedbackEvent, HealthMetric, KnowledgeUnit, RawEvent, RawPayload, Result, SourceId,
};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::record_types;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudKitPushPayload {
    #[serde(default)]
    pub new_change_token: Option<String>,
    #[serde(default)]
    pub records: Vec<CloudKitRecordEnvelope>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudKitRecordEnvelope {
    pub record_id: String,
    pub record_type: String,
    pub modified_at: Value,
    #[serde(default)]
    pub fields: Value,
}

pub fn payload_from_bytes(payload: &[u8]) -> Result<CloudKitPushPayload> {
    if let Ok(push) = serde_json::from_slice::<CloudKitPushPayload>(payload) {
        return Ok(push);
    }

    let value: Value = serde_json::from_slice(payload)
        .map_err(|_| CoreError::InvalidInput("invalid cloudkit push payload".to_string()))?;
    for key in ["cloudkit_push", "push", "data", "ck"] {
        if let Some(nested) = value.get(key) {
            if let Ok(push) = serde_json::from_value::<CloudKitPushPayload>(nested.clone()) {
                return Ok(push);
            }
        }
    }

    Err(CoreError::InvalidInput(
        "invalid cloudkit push payload".to_string(),
    ))
}

pub fn record_to_raw_event(record: &CloudKitRecordEnvelope) -> Result<RawEvent> {
    let modified_at = parse_datetime(&record.modified_at)?;
    let source = SourceId::new("cloudkit");
    let payload = match record.record_type.as_str() {
        record_types::BEHAVIORAL_SUMMARY => map_behavioral_summary(record, modified_at)?,
        record_types::PHYSIOLOGICAL_SAMPLE => map_physiological_sample(record, modified_at)?,
        record_types::KNOWLEDGE_UNIT_SYNC => map_knowledge_unit_sync(record)?,
        record_types::FEEDBACK_EVENT_SYNC => map_feedback_sync(record)?,
        other => {
            return Err(CoreError::InvalidInput(format!(
                "unknown cloudkit record type: {other}"
            )))
        }
    };
    Ok(RawEvent {
        source,
        timestamp: modified_at,
        payload,
    })
}

fn map_behavioral_summary(
    record: &CloudKitRecordEnvelope,
    modified_at: DateTime<Utc>,
) -> Result<RawPayload> {
    let fields = record.fields.as_object().ok_or_else(|| {
        CoreError::InvalidInput("BehavioralSummary.fields must be an object".to_string())
    })?;

    let period_start = fields
        .get("period_start")
        .map(parse_datetime)
        .transpose()?
        .unwrap_or(modified_at);
    let period_end = fields
        .get("period_end")
        .map(parse_datetime)
        .transpose()?
        .unwrap_or(modified_at);
    let avg_context_switch_rate = number_as_f32(fields.get("avg_context_switch_rate"))?;
    let peak_context_switch_rate = number_as_f32(fields.get("peak_context_switch_rate"))?;
    let audio_active_seconds = number_as_u32(fields.get("audio_active_seconds"))?;
    let dominant_app = fields
        .get("dominant_app")
        .and_then(|v| v.as_str())
        .map(|v| v.to_string());
    let app_switch_count = number_as_u32(fields.get("app_switch_count"))?;
    let was_in_meeting = bool_with_default(fields.get("was_in_meeting"), false);
    let was_in_focus_block = bool_with_default(fields.get("was_in_focus_block"), false);
    let device_origin = fields
        .get("device_origin")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(RawPayload::BehavioralSummary {
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
    })
}

fn map_physiological_sample(
    record: &CloudKitRecordEnvelope,
    modified_at: DateTime<Utc>,
) -> Result<RawPayload> {
    let fields = record.fields.as_object().ok_or_else(|| {
        CoreError::InvalidInput("PhysiologicalSample.fields must be an object".to_string())
    })?;
    let metric = fields
        .get("metric")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::InvalidInput("missing physiological metric".to_string()))?;
    let metric = parse_health_metric(metric)?;
    let value = fields
        .get("value")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| CoreError::InvalidInput("missing physiological value".to_string()))?;
    let unit = fields
        .get("unit")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let recorded_at = fields
        .get("recorded_at")
        .map(parse_datetime)
        .transpose()?
        .unwrap_or(modified_at);
    Ok(RawPayload::HealthKitSample {
        metric,
        value,
        unit,
        recorded_at,
    })
}

fn map_knowledge_unit_sync(record: &CloudKitRecordEnvelope) -> Result<RawPayload> {
    let fields = record.fields.as_object().ok_or_else(|| {
        CoreError::InvalidInput("KnowledgeUnitSync.fields must be an object".to_string())
    })?;
    let unit_value = fields
        .get("unit")
        .ok_or_else(|| CoreError::InvalidInput("missing synced unit".to_string()))?
        .clone();
    let unit: KnowledgeUnit = serde_json::from_value(unit_value)
        .map_err(|e| CoreError::InvalidInput(format!("invalid synced unit payload: {e}")))?;
    let device_origin = fields
        .get("device_origin")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    Ok(RawPayload::KnowledgeUnitSync {
        unit: Box::new(unit),
        device_origin,
    })
}

fn map_feedback_sync(record: &CloudKitRecordEnvelope) -> Result<RawPayload> {
    let fields = record.fields.as_object().ok_or_else(|| {
        CoreError::InvalidInput("FeedbackEventSync.fields must be an object".to_string())
    })?;
    let event_value = fields
        .get("event")
        .ok_or_else(|| CoreError::InvalidInput("missing synced feedback event".to_string()))?
        .clone();
    let event: FeedbackEvent = serde_json::from_value(event_value)
        .map_err(|e| CoreError::InvalidInput(format!("invalid synced feedback payload: {e}")))?;
    let device_origin = fields
        .get("device_origin")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    Ok(RawPayload::FeedbackEventSync {
        event: Box::new(event),
        device_origin,
    })
}

fn parse_datetime(value: &Value) -> Result<DateTime<Utc>> {
    if let Some(s) = value.as_str() {
        return DateTime::parse_from_rfc3339(s)
            .map(|v| v.with_timezone(&Utc))
            .map_err(|e| CoreError::InvalidInput(format!("invalid datetime value: {e}")));
    }
    if let Some(n) = value.as_f64() {
        let secs = n.trunc() as i64;
        let nanos = ((n.fract() * 1_000_000_000.0).round() as i64).clamp(0, 999_999_999);
        let dt = Utc
            .timestamp_opt(secs, nanos as u32)
            .single()
            .ok_or_else(|| CoreError::InvalidInput("invalid epoch datetime value".to_string()))?;
        return Ok(dt);
    }
    Err(CoreError::InvalidInput(
        "datetime must be RFC3339 string or unix epoch number".to_string(),
    ))
}

fn parse_health_metric(metric: &str) -> Result<HealthMetric> {
    match metric {
        "hrv" | "HRV" => Ok(HealthMetric::HRV),
        "resting_heart_rate" | "RestingHeartRate" => Ok(HealthMetric::RestingHeartRate),
        "sleep_duration" | "SleepDuration" => Ok(HealthMetric::SleepDuration),
        "sleep_quality" | "SleepQuality" => Ok(HealthMetric::SleepQuality),
        "step_count" | "StepCount" => Ok(HealthMetric::StepCount),
        "active_calories" | "ActiveCalories" => Ok(HealthMetric::ActiveCalories),
        "respiratory_rate" | "RespiratoryRate" => Ok(HealthMetric::RespiratoryRate),
        _ => Err(CoreError::InvalidInput(format!(
            "unsupported physiological metric: {metric}"
        ))),
    }
}

fn number_as_f32(value: Option<&Value>) -> Result<f32> {
    Ok(value.and_then(|v| v.as_f64()).unwrap_or(0.0) as f32)
}

fn number_as_u32(value: Option<&Value>) -> Result<u32> {
    value
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .ok_or_else(|| CoreError::InvalidInput("expected unsigned integer field".to_string()))
}

fn bool_with_default(value: Option<&Value>, default: bool) -> bool {
    value.and_then(|v| v.as_bool()).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::payload_from_bytes;

    #[test]
    fn parses_direct_payload() {
        let raw = br#"{"new_change_token":"tok-1","records":[]}"#;
        let parsed = payload_from_bytes(raw).expect("payload parses");
        assert_eq!(parsed.new_change_token.as_deref(), Some("tok-1"));
        assert!(parsed.records.is_empty());
    }

    #[test]
    fn parses_wrapped_payload() {
        let raw = br#"{"cloudkit_push":{"new_change_token":"tok-2","records":[]}}"#;
        let parsed = payload_from_bytes(raw).expect("payload parses");
        assert_eq!(parsed.new_change_token.as_deref(), Some("tok-2"));
        assert!(parsed.records.is_empty());
    }

    #[test]
    fn parses_ck_wrapped_payload() {
        let raw = br#"{"ck":{"new_change_token":"tok-3","records":[]}}"#;
        let parsed = payload_from_bytes(raw).expect("payload parses");
        assert_eq!(parsed.new_change_token.as_deref(), Some("tok-3"));
        assert!(parsed.records.is_empty());
    }
}
