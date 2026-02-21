use std::collections::HashMap;

use ambient_core::{KnowledgeStore, KnowledgeUnit, PulseEvent, PulseSignal, SourceId};
use ambient_store::CozoStore;
use chrono::Utc;

#[test]
fn phase2_unit_with_context_derives_state() {
    let store = CozoStore::new().expect("store");
    let observed_at = Utc::now();
    let id = uuid::Uuid::new_v4();

    for idx in 0..5 {
        store
            .record_pulse(PulseEvent {
                timestamp: observed_at + chrono::Duration::seconds(idx),
                signal: PulseSignal::ContextSwitchRate {
                    switches_per_minute: 1.0,
                },
            })
            .expect("pulse write");
    }

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
                window_title: Some("T".to_string()),
            },
        })
        .expect("pulse write");

    store
        .upsert(KnowledgeUnit {
            id,
            source: SourceId::new("obsidian"),
            content: "note".to_string(),
            title: Some("note".to_string()),
            metadata: HashMap::new(),
            embedding: None,
            observed_at,
            content_hash: [9; 32],
        })
        .expect("upsert");

    let (_, pulse, state) = store
        .unit_with_context(id, 120)
        .expect("query")
        .expect("unit present");

    assert!(!pulse.is_empty());
    assert!(state.was_in_flow);
    assert!(state.was_on_call);
    assert_eq!(state.dominant_app.as_deref(), Some("com.test"));
}
