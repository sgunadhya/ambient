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
    let store = Arc::new(CozoStore::new().expect("store"));

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

    if let Some(first) = units.first() {
        store
            .upsert(first.clone())
            .expect("re-upsert to index links");
    }

    let all = store.search_fulltext("").expect("search");
    assert!(all.len() >= 3);

    let b = all
        .iter()
        .find(|u| u.title.as_deref() == Some("B"))
        .expect("B unit exists");
    let related = store.related(b.id, 2).expect("related");
    assert!(!related.is_empty());

    let _ = fs::remove_dir_all(vault);
}
