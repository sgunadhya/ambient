use std::collections::HashMap;
use std::fs;
use std::sync::mpsc;
use std::time::Duration;

use ambient_core::KnowledgeStore;
use ambient_normalizer::default_dispatch;
use ambient_store::CozoStore;
use ambient_watcher::ObsidianAdapter;

fn mk_temp_vault() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("ambient-phase1-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&path).expect("create temp vault");
    path
}

#[test]
fn phase1_obsidian_ingestion_and_links() {
    let vault = mk_temp_vault();

    fs::write(vault.join("A.md"), "# A\nlinks [[B]]").expect("write A");
    fs::write(vault.join("B.md"), "# B\nlinks [[C]]").expect("write B");
    fs::write(vault.join("C.md"), "# C").expect("write C");

    let adapter = ObsidianAdapter::new(&vault);
    let (tx, rx) = mpsc::channel();
    adapter.watch(tx).expect("watch starts");

    let dispatch = default_dispatch();
    let store = CozoStore::new().expect("store");

    let mut units = Vec::new();
    while units.len() < 3 {
        let raw = rx.recv_timeout(Duration::from_secs(2)).expect("raw event");
        let unit = dispatch.normalize(raw).expect("normalize markdown");
        store.upsert(unit.clone()).expect("upsert");
        units.push(unit);
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
    let _ = HashMap::<String, String>::new();
}
