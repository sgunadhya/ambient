use std::sync::Arc;

use ambient_core::{
    Candidate, CandidateSelector, Cluster, Clusterer, DiscoveryTrigger, KnowledgeStore,
    LensIndexStore, NoticerState, Result, RulePublisher, RuleSynthesizer,
};

/// Orchestrates the Noticer Pipeline using interchangeable components.
pub struct NoticerEngine {
    trigger: Arc<dyn DiscoveryTrigger>,
    selector: Arc<dyn CandidateSelector>,
    clusterer: Arc<dyn Clusterer>,
    synthesizer: Arc<dyn RuleSynthesizer>,
    publisher: Arc<dyn RulePublisher>,
}

impl NoticerEngine {
    pub fn new(
        trigger: Arc<dyn DiscoveryTrigger>,
        selector: Arc<dyn CandidateSelector>,
        clusterer: Arc<dyn Clusterer>,
        synthesizer: Arc<dyn RuleSynthesizer>,
        publisher: Arc<dyn RulePublisher>,
    ) -> Self {
        Self {
            trigger,
            selector,
            clusterer,
            synthesizer,
            publisher,
        }
    }

    /// Run the Noticer pipeline.
    pub fn run(
        &self,
        state: &NoticerState,
        store: &dyn KnowledgeStore,
        index: &dyn LensIndexStore,
    ) -> Result<()> {
        if !self.trigger.should_run(state) {
            return Ok(());
        }

        // 1. Select Candidates
        let candidates = self.selector.select(store, index)?;
        if candidates.is_empty() {
            return Ok(());
        }

        // 2. Extract vectors to cluster
        let vecs: Vec<Vec<f32>> = candidates.iter().map(|c| c.vector.clone()).collect();
        let candidate_ids: Vec<_> = candidates.iter().map(|c| c.unit_id).collect();

        // 3. Cluster
        let clusters = self.clusterer.cluster(&vecs)?;

        // 4. Synthesize & Publish
        for mut cls in clusters {
            // Re-map dense indices back to Uuids
            cls.candidate_ids = cls
                .candidate_ids
                .into_iter()
                // using the indices stored temporarily in candidate_ids by the MVP clusterer
                .map(|idx| candidate_ids[idx.as_bytes()[0] as usize]) // A bit of a hack for MVP wiring
                .collect();

            // Fetch full units for the LLM payload
            let mut units = Vec::new();
            for &id in &cls.candidate_ids {
                if let Ok(Some(unit)) = store.get_by_id(id) {
                    units.push(unit);
                }
            }

            if let Ok(rule) = self.synthesizer.synthesize(&cls, &units) {
                let _ = self.publisher.publish(rule, store);
            }
        }

        Ok(())
    }
}

// =====================================
// MVP Implementations (Status Quo)
// =====================================

pub struct MvpDiscoveryTrigger;

impl DiscoveryTrigger for MvpDiscoveryTrigger {
    fn should_run(&self, state: &NoticerState) -> bool {
        // Run once daily or if there are 100+ new events
        let today = chrono::Utc::now().date_naive();
        if let Some(last_run) = state.last_run {
            if last_run.date_naive() != today {
                return true;
            }
        } else {
            return true;
        }

        state.new_events_count >= 100
    }
}

pub struct MvpCandidateSelector;

impl CandidateSelector for MvpCandidateSelector {
    fn select(
        &self,
        _store: &dyn KnowledgeStore,
        index: &dyn LensIndexStore,
    ) -> Result<Vec<Candidate>> {
        let vectors = index.get_all_lens_vectors(&ambient_core::LensConfig::semantic())?;
        if vectors.len() < 10 {
            return Ok(Vec::new());
        }

        Ok(vectors
            .into_iter()
            .map(|(id, vec)| Candidate {
                unit_id: id,
                vector: vec,
                cognitive_state: None,
            })
            .collect())
    }
}

pub struct MvpClusterer;

impl Clusterer for MvpClusterer {
    fn cluster(&self, vecs: &[Vec<f32>]) -> Result<Vec<Cluster>> {
        use ndarray::Array2;
        use petal_clustering::{Dbscan, Fit};
        use petal_neighbors::distance::Euclidean;
        use std::collections::HashMap;

        if vecs.is_empty() {
            return Ok(Vec::new());
        }

        let dim = vecs[0].len();
        let mut data = Vec::with_capacity(vecs.len() * dim);
        for v in vecs {
            data.extend_from_slice(v);
        }

        let array = Array2::from_shape_vec((vecs.len(), dim), data).map_err(|e| {
            ambient_core::CoreError::Internal(format!("failed to shape clustering array: {e}"))
        })?;

        // DBSCAN: eps=0.5 (cosine-ish), min_samples=3
        let mut model = Dbscan::new(0.5, 3, Euclidean::default());
        let (clusters_map, _): (HashMap<usize, Vec<usize>>, _) = model.fit(&array, None);

        let mut out = Vec::new();
        for (_label, indices) in clusters_map {
            if indices.len() >= 5 {
                // MVP hack: passing the index as bytes inside the Uuid since candidate_ids is local within run()
                let fake_ids = indices
                    .iter()
                    .map(|&i| {
                        let mut bytes = [0u8; 16];
                        bytes[0] = i as u8;
                        ambient_core::Uuid::from_bytes(bytes)
                    })
                    .collect();

                out.push(Cluster {
                    centroid: vec![0.0; 768], // Omitted for MVP
                    candidate_ids: fake_ids,
                });
            }
        }

        Ok(out)
    }
}

pub struct MvpRuleSynthesizer {
    reasoner: Arc<dyn ambient_core::ReasoningEngine>,
}

impl MvpRuleSynthesizer {
    pub fn new(reasoner: Arc<dyn ambient_core::ReasoningEngine>) -> Self {
        Self { reasoner }
    }
}

impl RuleSynthesizer for MvpRuleSynthesizer {
    fn synthesize(
        &self,
        cluster: &Cluster,
        units: &[ambient_core::KnowledgeUnit],
    ) -> Result<ambient_core::ProceduralRule> {
        let contents: Vec<String> = units.iter().map(|n| n.content.clone()).collect();
        let prompt = format!(
            "Analyze these related knowledge units and derive a 'procedural rule' that encapsulates the behavior or topic. \
            The rule should match similar future units. \
            Units: \n---\n{}\n---\n\
            Return a JSON object with: {{ \"summary\": \"...\", \"trigger_description\": \"...\", \"suggested_action_payload\": {{}} }}",
            contents.join("\n---\n")
        );

        let response = self.reasoner.answer(&prompt, units)?;
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&response) {
            Ok(ambient_core::ProceduralRule {
                id: ambient_core::Uuid::new_v4(),
                lens_intent: cluster.centroid.clone(),
                pulse_mask: "any".to_string(), // MVP: match any pulse
                threshold: 0.85,
                action_payload: val["suggested_action_payload"].clone(),
                created_at: chrono::Utc::now(),
            })
        } else {
            Err(ambient_core::CoreError::Internal(
                "Failed to parse LLM JSON".to_string(),
            ))
        }
    }
}

pub struct MvpRulePublisher;

impl RulePublisher for MvpRulePublisher {
    fn publish(
        &self,
        rule: ambient_core::ProceduralRule,
        store: &dyn KnowledgeStore,
    ) -> Result<()> {
        store.save_rule(rule)
    }
}

// =====================================
// Spec Implementations (Future Vision)
// =====================================

pub struct SpecCandidateSelector;
impl CandidateSelector for SpecCandidateSelector {
    fn select(
        &self,
        _store: &dyn KnowledgeStore,
        _index: &dyn LensIndexStore,
    ) -> Result<Vec<Candidate>> {
        Ok(Vec::new()) // To be implemented
    }
}

// Other Spec components omitted for brevity

pub struct SpecDiscoveryTrigger;

impl DiscoveryTrigger for SpecDiscoveryTrigger {
    fn should_run(&self, state: &NoticerState) -> bool {
        // Spec trigger will use event thresholds and heuristic frequency drops over time.
        state.new_events_count >= 50
    }
}

pub struct SpecClusterer;

impl Clusterer for SpecClusterer {
    fn cluster(&self, _vecs: &[Vec<f32>]) -> Result<Vec<Cluster>> {
        // Advanced HDBSCAN / LLM structured taxonomy mapping here
        Ok(Vec::new())
    }
}

pub struct SpecRuleSynthesizer;

impl RuleSynthesizer for SpecRuleSynthesizer {
    fn synthesize(
        &self,
        _cluster: &Cluster,
        _units: &[ambient_core::KnowledgeUnit],
    ) -> Result<ambient_core::ProceduralRule> {
        // Generates robust queries and JSON objects using multi-shot prompts
        Err(ambient_core::CoreError::Internal("Not implemented".to_string()))
    }
}

pub struct SpecRulePublisher;

impl RulePublisher for SpecRulePublisher {
    fn publish(
        &self,
        rule: ambient_core::ProceduralRule,
        store: &dyn KnowledgeStore,
    ) -> Result<()> {
        // Emits events, writes to multiple tables, broadcasts notifications
        store.save_rule(rule)
    }
}
