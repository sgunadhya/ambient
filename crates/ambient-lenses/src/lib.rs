use ambient_core::{KnowledgeUnit, LensId, ReasoningEngine, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

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
