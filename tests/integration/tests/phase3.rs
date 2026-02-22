use std::collections::HashMap;
use std::sync::Arc;

use ambient_cli::{build_router, run_query, HttpAppState};
use ambient_core::{KnowledgeStore, KnowledgeUnit, QueryEngine, QueryRequest, SourceId};
use ambient_query::AmbientQueryEngine;
use ambient_store::CozoStore;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use tower::ServiceExt;

#[test]
fn phase3_query_engine_ranking_and_fallback() {
    let store = Arc::new(CozoStore::new().expect("store"));

    for i in 0..10 {
        let id = uuid::Uuid::new_v4();
        let vec = vec![i as f32, (10 - i) as f32];
        store
            .upsert(KnowledgeUnit {
                id,
                source: SourceId::new("obsidian"),
                content: format!("distributed systems note {i}"),
                title: Some(format!("Note {i}")),
                metadata: HashMap::new(),
                observed_at: Utc::now(),
                content_hash: [i as u8; 32],
            })
            .expect("upsert");
        store
            .upsert_lens(id, "l1_semantic", vec)
            .expect("upsert lens");
    }

    let engine = AmbientQueryEngine::new(store.clone(), None);

    let direct = engine
        .query(QueryRequest {
            text: "distributed".to_string(),
            k: 10,
            include_pulse_context: true,
            context_window_secs: Some(120),
        })
        .expect("direct query");

    assert_eq!(direct.len(), 10);

    let cli = run_query(&engine, "distributed", true).expect("cli query");
    assert_eq!(cli.len(), direct.len());

    // FTS fallback path: no embeddings.
    let store_no_embeddings = Arc::new(CozoStore::new().expect("store"));
    store_no_embeddings
        .upsert(KnowledgeUnit {
            id: uuid::Uuid::new_v4(),
            source: SourceId::new("obsidian"),
            content: "panic notes fallback".to_string(),
            title: Some("Fallback".to_string()),
            metadata: HashMap::new(),
            observed_at: Utc::now(),
            content_hash: [7; 32],
        })
        .expect("upsert");
    let fallback_engine = AmbientQueryEngine::new(store_no_embeddings, None);
    let fallback = fallback_engine
        .query(QueryRequest {
            text: "fallback".to_string(),
            k: 3,
            include_pulse_context: false,
            context_window_secs: None,
        })
        .expect("fallback query");
    assert_eq!(fallback.len(), 1);

    // HTTP /query should produce the same result count as direct query path.
    let app = build_router(HttpAppState {
        engine: Arc::new(engine),
        store: store.clone(),
        auth_token: None,
        status_probe: None,
        deep_link_focus: Arc::new(std::sync::Mutex::new(None)),
        transport_registry: None,
    });
    let request = Request::builder()
        .method("POST")
        .uri("/query")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"text":"distributed","k":10,"include_pulse_context":true,"context_window_secs":120}"#,
        ))
        .expect("request");

    let response = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async { app.oneshot(request).await })
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async { to_bytes(response.into_body(), usize::MAX).await })
        .expect("body bytes");
    let via_http: Vec<ambient_core::QueryResult> =
        serde_json::from_slice(&bytes).expect("json decode");
    assert_eq!(via_http.len(), direct.len());
}
