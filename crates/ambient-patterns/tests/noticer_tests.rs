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

struct MockIndex;
impl LensIndexStore for MockIndex {
    fn upsert_lens_vec(&self, _unit_id: Uuid, _lens: &LensConfig, _vec: &[f32]) -> Result<()> {
        Ok(())
    }
    fn search_lens_vec(
        &self,
        _lens: &LensConfig,
        _query_vec: &[f32],
        _k: usize,
    ) -> Result<Vec<Uuid>> {
        Ok(vec![])
    }
    fn get_all_lens_vectors(&self, _lens: &LensConfig) -> Result<Vec<(Uuid, Vec<f32>)>> {
        Ok(vec![])
    }
}

struct MockReasoner;
impl ReasoningEngine for MockReasoner {
    fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        Ok(vec![0.0; 768])
    }
    fn embed_with_model(&self, _text: &str, _model: &str) -> Result<Vec<f32>> {
        Ok(vec![0.0; 768])
    }
    fn answer(&self, _prompt: &str, _context: &[KnowledgeUnit]) -> Result<String> {
        Ok(r#"{"summary": "Test Summary", "trigger_description": "Test Trigger", "suggested_action_payload": {}}"#.to_string())
    }
}

#[test]
fn test_mvp_noticer_pipeline() {
    let store = Arc::new(MockStore);
    let index = Arc::new(MockIndex);
    let reasoner = Arc::new(MockReasoner);

    let engine = NoticerEngine::new(
        Arc::new(ambient_patterns::MvpDiscoveryTrigger),
        Arc::new(ambient_patterns::MvpCandidateSelector),
        Arc::new(ambient_patterns::MvpClusterer),
        Arc::new(ambient_patterns::MvpRuleSynthesizer::new(reasoner)),
        Arc::new(ambient_patterns::MvpRulePublisher),
    );

    let state = NoticerState {
        last_run: None,
        new_events_count: 500,
    };

    let result = engine.run(&state, store.as_ref(), index.as_ref());
    assert!(result.is_ok());
}

#[test]
fn test_spec_noticer_pipeline() {
    let store = Arc::new(MockStore);
    let index = Arc::new(MockIndex);

    let engine = NoticerEngine::new(
        Arc::new(ambient_patterns::SpecDiscoveryTrigger),
        Arc::new(ambient_patterns::SpecCandidateSelector),
        Arc::new(ambient_patterns::SpecClusterer),
        Arc::new(ambient_patterns::SpecRuleSynthesizer),
        Arc::new(ambient_patterns::SpecRulePublisher),
    );

    let state = NoticerState {
        last_run: None,
        new_events_count: 500,
    };

    let result = engine.run(&state, store.as_ref(), index.as_ref());
    assert!(result.is_ok());
}
