use ambient_core::*;
use ambient_patterns::*;
use std::sync::Arc;

struct MockStore;
impl KnowledgeStore for MockStore {
    fn search_fulltext(&self, _query: &str) -> Result<Vec<KnowledgeUnit>> {
        Ok(vec![])
    }
    fn search_semantic(&self, _query: &[f32], _limit: usize) -> Result<Vec<KnowledgeUnit>> {
        Ok(vec![])
    }
    fn related(&self, _id: Uuid, _limit: usize) -> Result<Vec<KnowledgeUnit>> {
        Ok(vec![])
    }
    fn record_pulse(&self, _event: PulseEvent) -> Result<()> {
        Ok(())
    }
    fn pulse_window(
        &self,
        _start: chrono::DateTime<chrono::Utc>,
        _end: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<PulseEvent>> {
        Ok(vec![])
    }
    fn feedback_score(&self, _id: Uuid) -> Result<f32> {
        Ok(0.0)
    }
    fn pattern_feedback(&self, _id: Uuid) -> Result<Option<String>> {
        Ok(None)
    }
    fn get_active_rules_for_pulse(&self, _app: &str) -> Result<Vec<ProceduralRule>> {
        Ok(vec![])
    }
    fn get_temporal_profile(&self, _id: Uuid) -> Result<Option<ambient_core::TemporalProfile>> {
        Ok(None)
    }

    fn get_by_id(&self, _id: Uuid) -> Result<Option<KnowledgeUnit>> {
        Ok(None)
    }
    fn unit_with_context(
        &self,
        _id: Uuid,
        _window_secs: u64,
    ) -> Result<Option<(KnowledgeUnit, Vec<PulseEvent>, CognitiveState)>> {
        Ok(None)
    }
    fn upsert(&self, _unit: KnowledgeUnit) -> Result<()> {
        Ok(())
    }
    fn record_feedback(&self, _event: FeedbackEvent) -> Result<()> {
        Ok(())
    }
    fn calculate_temporal_profile(&self, _unit_id: Uuid) -> Result<()> {
        Ok(())
    }
    fn save_rule(&self, _rule: ProceduralRule) -> Result<()> {
        Ok(())
    }
    fn get_all_rules(&self) -> Result<Vec<ProceduralRule>> {
        Ok(vec![])
    }
    fn get_recent_snapshots(&self, _limit: usize) -> Result<Vec<(Uuid, CognitiveState)>> {
        Ok(vec![])
    }
}

#[test]
fn test_mvp_rule_evaluation_pipeline() {
    let store = Arc::new(MockStore);
    let engine = RuleEngine::new(
        Arc::new(ambient_patterns::MvpPulseMatcher),
        Arc::new(ambient_patterns::MvpIntentScorer),
        Arc::new(ambient_patterns::MvpThresholdPolicy),
        Arc::new(ambient_patterns::NoopRuleExecutor),
    );

    let pulse = PulseSnapshot {
        current: CognitiveState {
            was_in_flow: true,
            dominant_app: Some("Xcode".to_string()),
            was_on_call: false,
            was_in_focus_block: false,
            time_of_day: None,
            was_in_meeting: false,
            energy_level: None,
            mood_level: None,
            hrv_score: None,
            sleep_quality: None,
            minutes_since_last_meeting: None,
        },
        history: vec![],
    };

    let result = engine.evaluate(store.as_ref(), &pulse, &[]);
    assert!(result.is_ok());
}

fn dummy_state() -> CognitiveState {
    CognitiveState {
        was_in_flow: false,
        was_on_call: false,
        was_in_focus_block: false,
        time_of_day: Some(12),
        dominant_app: None,
        was_in_meeting: false,
        energy_level: None,
        mood_level: None,
        hrv_score: None,
        sleep_quality: None,
        minutes_since_last_meeting: None,
    }
}

#[test]
fn test_spec_rule_evaluation_pipeline() {
    let store = Arc::new(MockStore);
    let engine = RuleEngine::new(
        Arc::new(ambient_patterns::SpecPulseMatcher),
        Arc::new(ambient_patterns::SpecIntentScorer),
        Arc::new(ambient_patterns::SpecThresholdPolicy),
        Arc::new(ambient_patterns::NoopRuleExecutor),
    );

    let pulse = PulseSnapshot {
        current: dummy_state(),
        history: vec![],
    };

    let result = engine.evaluate(store.as_ref(), &pulse, &[0.1, 0.2]);
    assert!(result.is_ok());
}
