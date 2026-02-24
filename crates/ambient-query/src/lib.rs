use std::sync::Arc;

use ambient_core::{
    CapabilityGate, CapabilityStatus, CoreError, GatedCapability, KnowledgeStore, LensConfig,
    LensIndexStore, LensRetriever, QueryEngine, QueryRequest, QueryResult, ReasoningEngine, Result,
    Uuid,
};
use ambient_store::{CozoStore, LanceDbStore};
use chrono::{Datelike, Timelike};
use tracing::{info, instrument, warn};

pub struct VectorLensRetriever {
    config: LensConfig,
    index_store: Arc<dyn LensIndexStore>,
    reasoning: Arc<dyn ReasoningEngine>,
}

impl VectorLensRetriever {
    pub fn new(
        config: LensConfig,
        index_store: Arc<dyn LensIndexStore>,
        reasoning: Arc<dyn ReasoningEngine>,
    ) -> Self {
        Self {
            config,
            index_store,
            reasoning,
        }
    }
}

impl LensRetriever for VectorLensRetriever {
    fn config(&self) -> Option<&LensConfig> {
        Some(&self.config)
    }

    #[instrument(skip(self, request))]
    fn retrieve(&self, request: &QueryRequest) -> Result<Vec<Uuid>> {
        let vec = self
            .reasoning
            .embed_with_model(&request.text, &self.config.model_name)?;
        self.index_store
            .search_lens_vec(&self.config, &vec, request.k * 2)
    }
}

pub struct FtsRetriever {
    store: Arc<dyn KnowledgeStore>,
}

impl FtsRetriever {
    pub fn new(store: Arc<dyn KnowledgeStore>) -> Self {
        Self { store }
    }
}

impl LensRetriever for FtsRetriever {
    fn config(&self) -> Option<&LensConfig> {
        None
    }

    #[instrument(skip(self, request))]
    fn retrieve(&self, request: &QueryRequest) -> Result<Vec<Uuid>> {
        let units = self.store.search_fulltext(&request.text)?;
        Ok(units.into_iter().map(|u| u.id).collect())
    }
}

pub struct AmbientQueryEngine {
    store: Arc<dyn KnowledgeStore>,
    reasoning: Option<Arc<dyn ReasoningEngine>>,
    gate: Option<Arc<dyn CapabilityGate>>,
    retrievers: Vec<Arc<dyn LensRetriever>>,
    semantic_weight: f32,
    feedback_weight: f32,
}

impl AmbientQueryEngine {
    pub fn new(
        store: Arc<dyn KnowledgeStore>,
        reasoning: Option<Arc<dyn ReasoningEngine>>,
        retrievers: Vec<Arc<dyn LensRetriever>>,
    ) -> Self {
        Self {
            store,
            reasoning,
            gate: None,
            retrievers,
            semantic_weight: 0.7,
            feedback_weight: 0.3,
        }
    }

    pub fn with_capability_gate(mut self, gate: Arc<dyn CapabilityGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    pub fn with_reasoning(mut self, reasoning: Arc<dyn ReasoningEngine>) -> Self {
        self.reasoning = Some(reasoning);
        self
    }

    pub fn with_weights(mut self, semantic_weight: f32, feedback_weight: f32) -> Self {
        self.semantic_weight = semantic_weight;
        self.feedback_weight = feedback_weight;
        self
    }

    /// Reciprocal Rank Fusion (RRF)
    /// rank_lists: Vec of ranked results (unit_id) per lens.
    /// k: denominator constant (usually 60).
    fn rrf(&self, rank_lists: Vec<Vec<Uuid>>, k_param: f32) -> Vec<(Uuid, f32)> {
        let mut scores: std::collections::HashMap<Uuid, f32> = std::collections::HashMap::new();

        for list in rank_lists {
            for (rank, id) in list.into_iter().enumerate() {
                let score = 1.0 / (k_param + (rank + 1) as f32);
                *scores.entry(id).or_insert(0.0) += score;
            }
        }

        let mut fused: Vec<(Uuid, f32)> = scores.into_iter().collect();
        fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        fused
    }

    fn semantic_capability_status(&self) -> Option<CapabilityStatus> {
        self.gate
            .as_ref()
            .map(|gate| gate.status(GatedCapability::SemanticSearch))
    }

    fn semantic_allowed(&self) -> bool {
        matches!(
            self.semantic_capability_status(),
            None | Some(CapabilityStatus::Ready)
        )
    }

    #[instrument(skip(self, req))]
    pub fn answer_internal(&self, question: &str, req: &QueryRequest) -> Result<Option<String>> {
        let Some(reasoning) = &self.reasoning else {
            warn!("Reasoning engine unavailable for answer_internal");
            return Ok(None);
        };

        let results = self.query(req.clone()).unwrap_or_default();
        let units: Vec<_> = results.into_iter().map(|r| r.unit).collect();

        let response = reasoning.answer(question, &units)?;
        Ok(Some(response))
    }
}

impl QueryEngine for AmbientQueryEngine {
    #[instrument(skip(self, req), fields(text = %req.text, k = req.k))]
    fn query(&self, req: QueryRequest) -> Result<Vec<QueryResult>> {
        let mut rank_lists = Vec::new();

        for retriever in &self.retrievers {
            let is_fts = retriever.config().is_none();
            let retriever_type = if is_fts { "fts" } else { "vector" };

            if !is_fts && !self.semantic_allowed() {
                continue; // Skip vector searches if semantic intelligence is disabled
            }

            match retriever.retrieve(&req) {
                Ok(ids) => {
                    info!(
                        retriever = retriever_type,
                        hits = ids.len(),
                        "Retriever yielded candidates"
                    );
                    if !ids.is_empty() {
                        rank_lists.push(ids);
                    }
                }
                Err(e) => {
                    warn!(retriever = retriever_type, error = %e, "Retriever failed to execute");
                }
            }
        }

        // 4. Fusion via RRF
        let fused = self.rrf(rank_lists, 60.0);
        info!(fused_candidates = fused.len(), "RRF fusion completed");
        if fused.is_empty() {
            return Err(CoreError::NotFound("no matching units found".to_string()));
        }

        let mut out = Vec::new();
        let cap = self.semantic_capability_status();

        // L4 Temporal Context
        let now = chrono::Utc::now();
        let current_hour = now.hour() as i8;
        let current_day_mask = 1 << (now.weekday() as u32);

        for (id, rrf_score) in fused.into_iter().take(req.k) {
            let unit = match self.store.get_by_id(id)? {
                Some(u) => u,
                None => continue,
            };

            let (pulse_context, cognitive_state) = if req.include_pulse_context {
                match self
                    .store
                    .unit_with_context_live(unit.id, req.context_window_secs.unwrap_or(120))
                {
                    Ok(Some((_, pulse, state))) => (Some(pulse), Some(state)),
                    _ => (None, None),
                }
            } else {
                match self.store.unit_with_context_fast(unit.id) {
                    Ok(Some((_, state))) => (None, Some(state)),
                    _ => (None, None),
                }
            };

            // Hybrid score: (RRF * semantic_weight) + (Feedback * feedback_weight)
            let feedback_score = self.store.feedback_score(unit.id).unwrap_or(0.5);
            let mut score = (rrf_score * 100.0 * self.semantic_weight)
                + (feedback_score * self.feedback_weight);

            // L4 Temporal Boost
            if let Ok(Some(profile)) = self.store.get_temporal_profile(id) {
                let mut temporal_boost = 1.0f32;

                // Hour alignment (peak hour ± 1h)
                if profile.hour_peak != -1 {
                    let diff = (profile.hour_peak - current_hour).abs();
                    if diff <= 1 || diff >= 23 {
                        temporal_boost += 0.2; // 20% boost for time alignment
                    }
                }

                // Day alignment
                if (profile.day_mask & current_day_mask) != 0 {
                    temporal_boost += 0.1; // 10% boost for day alignment
                }

                // Recency weight (0.0 to 1.0)
                temporal_boost += profile.recency_weight * 0.2; // up to 20% boost for recency

                score *= temporal_boost;
                info!(unit_id = %id, temporal_boost, final_score = score, "Temporal rerank applied");
            }

            out.push(QueryResult {
                unit,
                score,
                pulse_context,
                cognitive_state,
                historical_feedback_score: feedback_score,
                capability_status: cap.clone(),
                predicted_actions: None,
            });
        }

        if out.is_empty() {
            return Err(CoreError::NotFound("no matching units".to_string()));
        }

        // Final sort by hybrid score
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(out)
    }

    fn answer(&self, question: &str, req: QueryRequest) -> Result<Option<String>> {
        self.answer_internal(question, &req)
    }
}

pub type RuntimeComponents = (
    Arc<dyn QueryEngine>,
    Arc<dyn KnowledgeStore>,
    Arc<dyn LensIndexStore>,
);

pub fn build_runtime_components(
    reasoning: Option<Arc<dyn ReasoningEngine>>,
) -> Result<RuntimeComponents> {
    build_runtime_components_with_weights(0.7, 0.3, reasoning)
}

pub fn build_runtime_components_with_weights(
    semantic_weight: f32,
    feedback_weight: f32,
    reasoning: Option<Arc<dyn ReasoningEngine>>,
) -> Result<RuntimeComponents> {
    let cozo = Arc::new(CozoStore::new()?);
    let store: Arc<dyn KnowledgeStore> = cozo.clone();
    let index_store: Arc<dyn LensIndexStore> = Arc::new(LanceDbStore::new()?);

    let mut retrievers: Vec<Arc<dyn LensRetriever>> =
        vec![Arc::new(FtsRetriever::new(store.clone()))];

    if let Some(r) = reasoning.as_ref() {
        retrievers.push(Arc::new(VectorLensRetriever::new(
            LensConfig::semantic(),
            index_store.clone(),
            r.clone(),
        )));
        retrievers.push(Arc::new(VectorLensRetriever::new(
            LensConfig::technical(),
            index_store.clone(),
            r.clone(),
        )));
    }

    let engine: Arc<dyn QueryEngine> = Arc::new(
        AmbientQueryEngine::new(store.clone(), reasoning, retrievers)
            .with_weights(semantic_weight, feedback_weight),
    );
    Ok((engine, store, index_store))
}
