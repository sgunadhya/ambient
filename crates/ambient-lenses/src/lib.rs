use ambient_core::{KnowledgeUnit, ReasoningEngine, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LensId {
    L1Semantic,
    L2Technical,
    L3Fulltext,
    L4Temporal,
    L5Social,
}

impl LensId {
    pub fn as_str(&self) -> &'static str {
        match self {
            LensId::L1Semantic => "l1_semantic",
            LensId::L2Technical => "l2_technical",
            LensId::L3Fulltext => "l3_fulltext",
            LensId::L4Temporal => "l4_temporal",
            LensId::L5Social => "l5_social",
        }
    }
}

#[async_trait]
pub trait LensRouter: Send + Sync {
    async fn embed_all(&self, unit: &KnowledgeUnit) -> Result<HashMap<LensId, Vec<f32>>>;
}

pub struct MultiLensRouter {
    reasoning: Arc<dyn ReasoningEngine>,
}

impl MultiLensRouter {
    pub fn new(reasoning: Arc<dyn ReasoningEngine>) -> Self {
        Self { reasoning }
    }
}

#[async_trait]
impl LensRouter for MultiLensRouter {
    async fn embed_all(&self, unit: &KnowledgeUnit) -> Result<HashMap<LensId, Vec<f32>>> {
        let mut results = HashMap::new();

        // L1: Semantic (Nomic-v2)
        if let Ok(vec) = self
            .reasoning
            .embed_with_model(&unit.content, "nomic-embed-text")
        {
            results.insert(LensId::L1Semantic, vec);
        }

        // L2: Technical (Jina-Code-v2)
        if let Ok(vec) = self
            .reasoning
            .embed_with_model(&unit.content, "jina-embeddings-v2-base-code")
        {
            results.insert(LensId::L2Technical, vec);
        }

        // L5: Social (GTE-Small)
        // Note: L5 uses 384D. The store will need a 384D index version.
        if let Ok(vec) = self.reasoning.embed_with_model(&unit.content, "gte-small") {
            results.insert(LensId::L5Social, vec);
        }

        Ok(results)
    }
}
