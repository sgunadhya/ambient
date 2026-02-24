# Migrating HNSW Vector Storage to LanceDB

The `ambient` daemon currently uses `CozoDB` (via SQLite) for both relational Datalog operations and `HNSW` vector embeddings. As the application scales, maintaining embedded graph edges for dense float arrays inside a single-writer SQLite transaction deeply bottlenecks ingestion. 

To resolve this, we will write a decoupled architecture that retains `CozoDB` for relational/Datalog queries while cleanly migrating pure vector data to `LanceDB`.

## Current Architecture Context
`ambient-core` abstractly isolates database interactions behind two primary traits:
1. `KnowledgeStore` (Relational, Datalog, FTS Text)
2. `LensIndexStore` (Dense Vectors, Similarity Search)

Because this barrier exists, the split architecture does **not** require rewriting the query pipelines (`ambient-query`) or predictive patterns (`ambient-patterns`).

## Step-by-Step Implementation

### 1. Integrate `lancedb` Dependency
- Add `lancedb = "0.26"` and `arrow = "51.0"` to `crates/ambient-store/Cargo.toml`.
- Configure `LanceDbStore` alongside `CozoStore` inside the library. 

### 2. Implement `LensIndexStore` on `LanceDbStore`
LanceDB uses Arrow schemas instead of JSON. You will create a new struct `LanceDbStore` that manages a Lance table explicitly bound to `unit_id` and the raw tensor.

```rust
struct LanceDbStore {
    db: lancedb::Connection,
    // Caches pre-opened tables for n-dimensions (384, 768)
}

impl LensIndexStore for LanceDbStore {
    fn upsert_lens_vec(&self, unit_id: Uuid, lens: &LensConfig, vec: &[f32]) -> Result<()> {
       // Serialize to simple `arrow::RecordBatch`
       // Write to lancedb table `lens_{dimensions}` (e.g. `lens_768`)
    }

    fn search_lens_vec(&self, lens: &LensConfig, query_vec: &[f32], k: usize) -> Result<Vec<Uuid>> {
       // Execute lancedb similarity filter: `table.search(query_vec).limit(k).execute()`
    }
}
```

### 3. Maintain Eventual Consistency via the WAL
Because you lose atomic transactions across two separate databases, the `ambient-stream` Write-Ahead Log must serve as your synchronization boundary.

Presently, `crates/ambient-cli/src/main.rs` establishes a single `LensConsumer` that fetches offline embeddings *and* writes to `CozoStore` synchronously. You will split this stream:
- `CozoConsumer`: Reads WAL offset A -> writes Text/Metadata to `CozoStore`.
- `VectorConsumer`: Reads WAL offset B -> generates Embedding -> writes to `LanceDbStore`.

If the system crashes halfway through, the `tokio` channels will resume from the specific consumer offsets securely on reboot without split-brain metadata gaps.

### 4. Remove `CozoStore` Vector Tables
Once verified, purge the `lens_map` and `lens_map_384` schema initialization scripts from `ambient-store/src/lib.rs`. Strip out the `search_lens_vec` logic that emits complex Datalog index bindings.

### 5. Wire the HTTP Daemon (CLI)
Modify `ambient-cli/src/main.rs`:

```rust
let cozo_store = Arc::new(CozoStore::new()?);
let lance_store = Arc::new(LanceDbStore::new()?);

// ambient-query safely consumes them adjacently!
let engine = AmbientQueryEngine::new(
    cozo_store.clone(),
    Some(lance_store.clone()),
    vec![...]
);
```

## Considerations
Because `ambient-query` already executes concurrent **Scatter-Gather** searches (fanning out parallel threads to FTS and Vector indices) before aggregating the results via **Reciprocal Rank Fusion (RRF)**, there is no performance penalty or complex graph-join query needed to merge the LanceDB and CozoDB datasets at read time.
