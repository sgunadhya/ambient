use std::sync::Arc;

use ambient_core::{
    CapabilityGate, CapabilityStatus, CoreError, GatedCapability, KnowledgeStore, QueryEngine,
    QueryRequest, QueryResult, ReasoningEngine, Result,
};
use ambient_store::LadybugStore;

pub struct AmbientQueryEngine {
    store: Arc<dyn KnowledgeStore>,
    reasoning: Option<Arc<dyn ReasoningEngine>>,
    gate: Option<Arc<dyn CapabilityGate>>,
    semantic_weight: f32,
    feedback_weight: f32,
}

impl AmbientQueryEngine {
    pub fn new(store: Arc<dyn KnowledgeStore>, reasoning: Option<Arc<dyn ReasoningEngine>>) -> Self {
        Self {
            store,
            reasoning,
            gate: None,
            semantic_weight: 0.7,
            feedback_weight: 0.3,
        }
    }

    pub fn with_capability_gate(mut self, gate: Arc<dyn CapabilityGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    pub fn with_weights(mut self, semantic_weight: f32, feedback_weight: f32) -> Self {
        self.semantic_weight = semantic_weight;
        self.feedback_weight = feedback_weight;
        self
    }

    fn semantic_capability_status(&self) -> Option<CapabilityStatus> {
        self.gate
            .as_ref()
            .map(|gate| gate.status(GatedCapability::SemanticSearch))
    }

    fn semantic_allowed(&self) -> bool {
        matches!(self.semantic_capability_status(), None | Some(CapabilityStatus::Ready))
    }

    fn fallback_query(&self, req: &QueryRequest) -> Result<Vec<QueryResult>> {
        let mut out = Vec::new();
        let units = self.store.search_fulltext(&req.text)?;
        let cap = self.semantic_capability_status();
        for (idx, unit) in units.into_iter().take(req.k).enumerate() {
            let (pulse_context, cognitive_state) = if req.include_pulse_context {
                match self
                    .store
                    .unit_with_context(unit.id, req.context_window_secs.unwrap_or(120))
                {
                    Ok(Some((_, pulse, state))) => (Some(pulse), Some(state)),
                    Ok(None) => (None, None),
                    Err(_) => (None, None),
                }
            } else {
                (None, None)
            };

            let semantic_score = if req.k == 0 {
                0.0
            } else {
                1.0 - (idx as f32 / req.k as f32)
            };
            let feedback_score = self.store.feedback_score(unit.id).unwrap_or(0.5);
            let reranked_score =
                (semantic_score * self.semantic_weight) + (feedback_score * self.feedback_weight);

            out.push(QueryResult {
                unit,
                score: reranked_score,
                pulse_context,
                cognitive_state,
                historical_feedback_score: feedback_score,
                capability_status: cap.clone(),
            });
        }
        Ok(out)
    }

    pub fn answer(&self, question: &str, req: &QueryRequest) -> Result<Option<String>> {
        let Some(reasoning) = &self.reasoning else {
            return Ok(None);
        };

        let units = self
            .store
            .search_semantic(&req.text, req.k)
            .or_else(|_| self.store.search_fulltext(&req.text))?;

        let response = reasoning.answer(question, &units)?;
        Ok(Some(response))
    }
}

impl QueryEngine for AmbientQueryEngine {
    fn query(&self, req: QueryRequest) -> Result<Vec<QueryResult>> {
        let units = if self.semantic_allowed() {
            self.store
                .search_semantic(&req.text, req.k)
                .or_else(|_| self.store.search_fulltext(&req.text))
        } else {
            self.store.search_fulltext(&req.text)
        };

        let units = match units {
            Ok(units) => units,
            Err(_) => return self.fallback_query(&req),
        };

        let mut out = Vec::new();
        let cap = self.semantic_capability_status();
        for (idx, unit) in units.into_iter().take(req.k).enumerate() {
            let (pulse_context, cognitive_state) = if req.include_pulse_context {
                match self
                    .store
                    .unit_with_context(unit.id, req.context_window_secs.unwrap_or(120))
                {
                    Ok(Some((_, pulse, state))) => (Some(pulse), Some(state)),
                    Ok(None) => (None, None),
                    Err(_) => (None, None),
                }
            } else {
                (None, None)
            };

            let semantic_score = if req.k == 0 {
                0.0
            } else {
                1.0 - (idx as f32 / req.k as f32)
            };
            let feedback_score = self.store.feedback_score(unit.id).unwrap_or(0.5);
            let reranked_score =
                (semantic_score * self.semantic_weight) + (feedback_score * self.feedback_weight);

            out.push(QueryResult {
                unit,
                score: reranked_score,
                pulse_context,
                cognitive_state,
                historical_feedback_score: feedback_score,
                capability_status: cap.clone(),
            });
        }

        if out.is_empty() {
            return Err(CoreError::NotFound("no matching units".to_string()));
        }

        Ok(out)
    }
}

pub fn build_runtime_components() -> Result<(Arc<dyn QueryEngine>, Arc<dyn KnowledgeStore>)> {
    build_runtime_components_with_weights(0.7, 0.3)
}

pub fn build_runtime_components_with_weights(
    semantic_weight: f32,
    feedback_weight: f32,
) -> Result<(Arc<dyn QueryEngine>, Arc<dyn KnowledgeStore>)> {
    let store: Arc<dyn KnowledgeStore> = Arc::new(LadybugStore::new()?);
    let engine: Arc<dyn QueryEngine> = Arc::new(
        AmbientQueryEngine::new(store.clone(), None).with_weights(semantic_weight, feedback_weight),
    );
    Ok((engine, store))
}
