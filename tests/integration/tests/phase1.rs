use std::fs;
use std::time::Duration;

use ambient_core::{KnowledgeStore, StreamTransport};
use ambient_normalizer::{default_dispatch, NormalizerConsumer};
use ambient_store::CozoStore;
use ambient_stream::SqliteStreamProvider;
use ambient_transport_local::ObsidianTransport;
use std::sync::Arc;

fn mk_temp_vault() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("ambient-phase1-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&path).expect("create temp vault");
    path
}

#[tokio::test]
async fn phase1_obsidian_ingestion_and_links() {
    let vault = mk_temp_vault();

    fs::write(vault.join("A.md"), "# A\nlinks [[B]]").expect("write A");
    fs::write(vault.join("B.md"), "# B\nlinks [[C]]").expect("write B");
    fs::write(vault.join("C.md"), "# C").expect("write C");

    let stream_db_path = mk_temp_vault().join("stream.db");
    let provider = Arc::new(SqliteStreamProvider::open(&stream_db_path).expect("stream open"));

    let transport = ObsidianTransport::new(&vault);
    let _handle = transport.start(provider.clone()).expect("start transport");

    let dispatch = Arc::new(default_dispatch());
    let store = Arc::new(CozoStore::new_for_test().expect("store"));

    let consumer = NormalizerConsumer::new(
        provider.clone(),
        store.clone(),
        dispatch.clone(),
        "test_consumer".to_string(),
    )
    .expect("consumer");

    let mut units = Vec::new();
    let mut i = 0;
    while units.len() < 3 && i < 10 {
        std::thread::sleep(Duration::from_millis(100));
        let polled = consumer.poll_once(10).expect("poll");
        units.extend(polled);
        i += 1;
    }

    // First pass already happened implicitly inside poll_once during ingestion.
    // Second pass: mutate content_hash to bypass deduplication and force `index_links`
    // now that all target units are in the database.
    let mut modified_units = units.clone();
    for unit in &mut modified_units {
        unit.content_hash[0] = unit.content_hash[0].wrapping_add(1);
        store
            .upsert(unit.clone())
            .expect("second pass re-upsert to index links");
    }

    // Verify 3 unique units were processed
    assert!(units.len() >= 3, "expected 3 units, got {}", units.len());

    let b_unit = units
        .iter()
        .find(|u| u.title.as_deref() == Some("B"))
        .expect("B unit exists in ingested output");

    let related = store.related(b_unit.id, 2).expect("related");
    assert!(!related.is_empty(), "expected related units for B");

    let _ = fs::remove_dir_all(vault);
}
