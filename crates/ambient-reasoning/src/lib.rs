use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use ambient_core::{
    CoreError, KnowledgeStore, KnowledgeUnit, LoadAware, ReasoningBackend, ReasoningEngine, Result,
    SystemLoad,
};
use rig::client::{CompletionClient, EmbeddingsClient, Nothing};
use rig::completion::Prompt;
use rig::embeddings::EmbeddingModel as RigEmbeddingModel;
use rig::providers::ollama;
use uuid::Uuid;

pub struct RigReasoningEngine {
    pub backend: ReasoningBackend,
    ollama_client: Option<ollama::Client>,
    embedding_model_name: String,
    completion_model_name: String,
    embedding_available: Arc<AtomicBool>,
}

impl RigReasoningEngine {
    pub fn new(backend: ReasoningBackend) -> Self {
        let ollama_client = match &backend {
            ReasoningBackend::Local { ollama_base_url } => ollama::Client::builder()
                .api_key(Nothing)
                .base_url(ollama_base_url)
                .build()
                .ok(),
            ReasoningBackend::Remote { .. } => None,
        };

        Self {
            backend,
            ollama_client,
            embedding_model_name: ollama::NOMIC_EMBED_TEXT.to_string(),
            completion_model_name: ollama::LLAMA3_2.to_string(),
            embedding_available: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn start_ollama_probe(&self) {
        let available = Arc::clone(&self.embedding_available);
        let backend = self.backend.clone();
        thread::spawn(move || loop {
            let ok = match &backend {
                ReasoningBackend::Local { ollama_base_url } => {
                    let url = format!("{}/api/tags", ollama_base_url.trim_end_matches('/'));
                    reqwest::blocking::Client::builder()
                        .timeout(Duration::from_secs(3))
                        .build()
                        .ok()
                        .and_then(|client| client.get(url).send().ok())
                        .is_some_and(|resp| resp.status().is_success())
                }
                ReasoningBackend::Remote { .. } => true,
            };
            available.store(ok, Ordering::Relaxed);
            thread::sleep(Duration::from_secs(60));
        });
    }

    pub fn embedding_available(&self) -> bool {
        self.embedding_available.load(Ordering::Relaxed)
    }
}

impl ReasoningEngine for RigReasoningEngine {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if !self.embedding_available() {
            return Err(CoreError::Unsupported("embedding backend unavailable"));
        }
        let Some(client) = &self.ollama_client else {
            return Err(CoreError::Unsupported("ollama client unavailable"));
        };
        let client = client.clone();
        let model_name = self.embedding_model_name.clone();
        let text = text.to_string();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| CoreError::Internal(format!("tokio runtime init failed: {e}")))
                .and_then(|rt| {
                    rt.block_on(async move {
                        let model = client.embedding_model_with_ndims(model_name, 768);
                        let embedding = model
                            .embed_text(&text)
                            .await
                            .map_err(|e| CoreError::Internal(format!("embedding failed: {e}")))?;
                        Ok(embedding.vec.into_iter().map(|v| v as f32).collect::<Vec<f32>>())
                    })
                });
            let _ = tx.send(result);
        });
        rx.recv_timeout(Duration::from_secs(30))
            .map_err(|_| CoreError::Internal("embedding timed out after 30s".to_string()))?
    }

    fn answer(&self, question: &str, context: &[KnowledgeUnit]) -> Result<String> {
        let Some(client) = &self.ollama_client else {
            return Err(CoreError::Unsupported("ollama client unavailable"));
        };
        let client = client.clone();
        let model_name = self.completion_model_name.clone();
        let mut joined = String::new();
        for unit in context {
            joined.push_str("\n- ");
            joined.push_str(unit.title.as_deref().unwrap_or("untitled"));
            joined.push_str(": ");
            joined.push_str(&unit.content);
        }
        let prompt = format!(
            "Use the context to answer the question.\nquestion: {question}\ncontext:{joined}"
        );
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| CoreError::Internal(format!("tokio runtime init failed: {e}")))
                .and_then(|rt| {
                    rt.block_on(async move {
                        let agent = client.agent(model_name).build();
                        agent
                            .prompt(prompt)
                            .await
                            .map_err(|e| CoreError::Internal(format!("completion failed: {e}")))
                    })
                });
            let _ = tx.send(result);
        });
        rx.recv_timeout(Duration::from_secs(60))
            .map_err(|_| CoreError::Internal("answer generation timed out after 60s".to_string()))?
    }
}

pub struct EmbeddingQueue {
    tx: mpsc::Sender<(Uuid, String)>,
    paused: Arc<AtomicBool>,
    queue_depth: Arc<Mutex<usize>>,
}

impl EmbeddingQueue {
    pub fn new(store: Arc<dyn KnowledgeStore>, reasoning: Arc<dyn ReasoningEngine>) -> Self {
        let (tx, rx) = mpsc::channel::<(Uuid, String)>();
        let shared_rx = Arc::new(Mutex::new(rx));
        let paused = Arc::new(AtomicBool::new(false));
        let queue_depth = Arc::new(Mutex::new(0usize));

        for _ in 0..2 {
            let worker_rx = Arc::clone(&shared_rx);
            let worker_store = Arc::clone(&store);
            let worker_reasoning = Arc::clone(&reasoning);
            let worker_paused = Arc::clone(&paused);
            let worker_depth = Arc::clone(&queue_depth);
            thread::spawn(move || loop {
                if worker_paused.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }

                let job = {
                    let Ok(guard) = worker_rx.lock() else {
                        return;
                    };
                    guard.recv()
                };

                let Ok((id, text)) = job else {
                    return;
                };

                if let Ok(mut depth) = worker_depth.lock() {
                    *depth = depth.saturating_sub(1);
                }

                let Ok(mut unit) = worker_store.get_by_id(id).and_then(|u| {
                    u.ok_or_else(|| CoreError::NotFound(format!("unit {id} not found")))
                }) else {
                    continue;
                };

                if let Ok(embedding) = worker_reasoning.embed(&text) {
                    unit.embedding = Some(embedding);
                    let _ = worker_store.upsert(unit);
                }
            });
        }

        Self {
            tx,
            paused,
            queue_depth,
        }
    }

    pub fn enqueue(&self, id: Uuid, text: String) -> Result<()> {
        self.tx
            .send((id, text))
            .map_err(|e| CoreError::Internal(format!("enqueue failed: {e}")))?;
        if let Ok(mut depth) = self.queue_depth.lock() {
            *depth += 1;
        }
        Ok(())
    }

    pub fn queue_depth(&self) -> usize {
        self.queue_depth.lock().map(|d| *d).unwrap_or(0)
    }
}

impl LoadAware for EmbeddingQueue {
    fn on_load_change(&self, load: SystemLoad) {
        match load {
            SystemLoad::Unconstrained => self.paused.store(false, Ordering::Relaxed),
            SystemLoad::Conservative => self.paused.store(true, Ordering::Relaxed),
            SystemLoad::Minimal => self.paused.store(true, Ordering::Relaxed),
        }
    }
}
