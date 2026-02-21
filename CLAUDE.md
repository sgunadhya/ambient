# Project: Ambient — Local-First Cognitive Intelligence Layer for macOS

## What We're Building

A macOS background daemon written in Rust that:
1. Watches multiple knowledge sources (Obsidian, Apple Notes, filesystem, and anything Spotlight has already indexed) for changes
2. Samples cognitive context signals (active app, keystroke density, context switches, audio state) via tsink
3. Normalizes knowledge content into a canonical `KnowledgeUnit` representation
4. Stores a persistent property graph + vector knowledge base via LadybugDB (embedded, in-process)
5. Correlates knowledge units with cognitive context windows from tsink at query time
6. Runs local LLM reasoning (via ollama sidecar) for Q&A, embedding, and pattern detection
7. Fires triggers and automations based on detected patterns
8. Exposes three search surfaces: a `Cmd+Shift+Space` menu bar overlay, a local HTTP API for third-party integrations (Raycast, Obsidian plugins, Alfred), and a CLI for power users
9. Integrates bidirectionally with Spotlight: reads `NSMetadataQuery` as a universal source adapter, and exports `KnowledgeUnit`s to `CSSearchableIndex` so they appear in `Cmd+Space` results

This is a privacy-first, local-only system. No cloud calls except optional ollama remote fallback.

---

## Architecture Principles

- **Thin crossing points**: all modules communicate exclusively through `RawEvent`, `KnowledgeUnit`, and `PulseEvent`. No module reaches into another's internals.
- **Substitutability**: every source adapter, the reasoning engine, and the storage backend must be swappable behind a trait interface.
- **Core platform + optional feature modules**: the Watcher → Normalizer → KnowledgeStore spine is the non-negotiable core. Reasoning, PatternDetector, TriggerEngine, and QueryInterface are optional feature modules that plug into the core via well-defined interfaces.
- **Fail-safe observation**: the daemon must never crash the host system or corrupt source files. All knowledge source access is read-only. tsink writes are append-only.
- **Two stores, one interface**: LadybugDB owns the graph + vector domain. tsink owns the time-series domain. The `KnowledgeStore` trait unifies both behind a single crossing point for all downstream consumers.
- **Three search surfaces, one query engine**: the menu bar overlay, HTTP API, and CLI all call the same `QueryEngine` trait. No search logic lives in any UI surface — surfaces are pure presentation.
- **Spotlight as infrastructure, not feature**: Spotlight integration runs in two directions. Inbound: `NSMetadataQuery` is a `SourceAdapter` that replaces a dozen individual file-type adapters for free. Outbound: `CSSearchableIndex` makes Ambient's knowledge base discoverable via `Cmd+Space`. Both are encapsulated in `ambient-spotlight` and neither direction couples to any other module.

---

## Canonical Data Types (Thin Crossing Points)

```rust
// ── Crossing Point 1: Source Adapter → Normalizer ──────────────────────────
// Typed payload — never opaque Bytes. Each source variant is explicit.
// Adding a new source requires adding a variant here; the compiler enforces
// exhaustive handling across the entire codebase.
pub enum RawPayload {
    Markdown {
        content: String,            // Adapter owns file I/O, Normalizer owns parsing
        path: PathBuf,
    },
    AppleNote {
        protobuf_blob: Bytes,       // Raw zData from NoteStore.sqlite
        note_id: String,            // Apple's internal UUID
        modified_at: DateTime<Utc>,
    },
    SpotlightItem {
        bundle_id: String,
        display_name: String,
        content_type: String,       // kMDItemContentType
        text_content: String,       // kMDItemTextContent — Spotlight already extracted this
        last_modified: DateTime<Utc>,
        file_url: Option<String>,
    },
    PlainText {
        content: String,
        path: PathBuf,
    },
}

pub struct RawEvent {
    pub source: SourceId,
    pub timestamp: DateTime<Utc>,
    pub payload: RawPayload,        // Typed, never opaque
}

// ── Crossing Point 2: Normalizer → All downstream modules ──────────────────
pub struct KnowledgeUnit {
    pub id: Uuid,
    pub source: SourceId,
    pub content: String,          // Normalized plain text
    pub title: Option<String>,
    pub metadata: HashMap<String, Value>,
    pub embedding: Option<Vec<f32>>,
    pub observed_at: DateTime<Utc>,
    pub content_hash: [u8; 32],   // For deduplication
}

// ── Crossing Point 3: System Sampler → ambient-pulse ───────────────────────
pub enum PulseSignal {
    // Sampled every 5 seconds
    ContextSwitchRate   { switches_per_minute: f32 },
    ActiveApp           { bundle_id: String, window_title: Option<String> },

    // Edge-triggered
    AudioInputActive    { active: bool },

    // Derived at KnowledgeUnit ingestion time — NOT sampled continuously
    TimeContext         { hour_of_day: u8, day_of_week: u8, is_weekend: bool },
}

pub struct PulseEvent {
    pub timestamp: DateTime<Utc>,
    pub signal: PulseSignal,
}

// ── Crossing Point 4: All search surfaces → QueryEngine ────────────────────
pub struct QueryRequest {
    pub text: String,                          // Natural language query
    pub k: usize,                              // Max results
    pub include_pulse_context: bool,           // Attach cognitive context to results
    pub context_window_secs: Option<u64>,      // Pulse window radius (default: 120s)
}

pub struct QueryResult {
    pub unit: KnowledgeUnit,
    pub score: f32,                            // Semantic similarity score
    pub pulse_context: Option<Vec<PulseEvent>>,// Cognitive state at creation time
    pub cognitive_state: Option<CognitiveState>,// Derived summary
}

pub struct CognitiveState {
    pub was_in_flow: bool,       // context_switch_rate < 2.0/min
    pub was_on_call: bool,       // audio_input_active during window
    pub dominant_app: Option<String>,
    pub time_of_day: Option<u8>,
}
```

**Rule:** `RawEvent`, `RawPayload`, `KnowledgeUnit`, `PulseEvent`, `QueryRequest`, `QueryResult`, `SystemLoad`, and `License` are the ONLY types that cross crate boundaries in the data pipeline. No crate may expose its internal domain types through its public API. `PayloadHint` is removed — source type is encoded in `RawPayload` variants, enforced by the compiler.

---

## Crate Structure

```
ambient/
├── Cargo.toml                    # workspace
├── CLAUDE.md                     # this file
├── crates/
│   ├── ambient-core/             # RawEvent, RawPayload, KnowledgeUnit, PulseEvent, QueryRequest, QueryResult, SystemLoad, License, all traits
│   ├── ambient-watcher/          # notify-based FS watcher + system telemetry sampler
│   ├── ambient-normalizer/       # per-source adapters behind SourceAdapter trait
│   ├── ambient-store/            # LadybugDB (graph+vector) + tsink (time-series), unified KnowledgeStore impl
│   ├── ambient-spotlight/        # Inbound: NSMetadataQuery SourceAdapter. Outbound: CSSearchableIndex exporter
│   ├── ambient-reasoning/        # Rig framework — EmbeddingModel + RAG agent, ollama local backend, remote fallback
│   ├── ambient-patterns/         # batch temporal correlation across LadybugDB + tsink
│   ├── ambient-triggers/         # rule engine, webhook/notification firing
│   ├── ambient-query/            # QueryEngine impl — semantic search + pulse correlation + LLM answer
│   ├── ambient-menubar/          # Cmd+Shift+Space overlay (AppKit via objc2) — calls ambient-query only
│   └── ambient-cli/              # CLI binary + axum HTTP API — calls ambient-query only
└── tests/
    └── integration/
```

---

## Key Traits (Module Interfaces)

```rust
// ── ambient-core: every knowledge source adapter implements this ────────────
pub trait SourceAdapter: Send + Sync {
    fn source_id(&self) -> SourceId;
    fn watch(&self, tx: Sender<RawEvent>) -> Result<WatchHandle>;
}

// ── ambient-core: every system sampler implements this ─────────────────────
pub trait PulseSampler: Send + Sync {
    fn start(&self, tx: Sender<PulseEvent>) -> Result<SamplerHandle>;
}

// ── ambient-core: pluggable per RawPayload variant ────────────────────────
// can_handle is a pattern match, not a hint comparison.
// Each Normalizer matches exactly one RawPayload variant.
// Adding a new RawPayload variant produces compile errors at every match
// site until all Normalizers are updated — silent failures are impossible.
pub trait Normalizer: Send + Sync {
    fn can_handle(&self, payload: &RawPayload) -> bool;
    fn normalize(&self, event: RawEvent) -> Result<KnowledgeUnit>;
}

// Normalizer dispatch table (owned by ambient-normalizer):
// MarkdownNormalizer    → RawPayload::Markdown
// AppleNotesNormalizer  → RawPayload::AppleNote
// SpotlightNormalizer   → RawPayload::SpotlightItem (dispatches internally by content_type)
// PlainTextNormalizer   → RawPayload::PlainText

// ── ambient-core: unified store interface (LadybugDB + tsink behind this) ──
pub trait KnowledgeStore: Send + Sync {
    // Graph + Vector side — LadybugDB
    fn upsert(&self, unit: KnowledgeUnit) -> Result<()>;
    fn search_semantic(&self, query: &str, k: usize) -> Result<Vec<KnowledgeUnit>>;
    fn search_fulltext(&self, query: &str) -> Result<Vec<KnowledgeUnit>>;
    fn related(&self, id: Uuid, depth: usize) -> Result<Vec<KnowledgeUnit>>;
    fn get_by_id(&self, id: Uuid) -> Result<Option<KnowledgeUnit>>;

    // Time-series side — tsink
    fn record_pulse(&self, event: PulseEvent) -> Result<()>;
    fn pulse_window(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<PulseEvent>>;

    // Correlated query — the strategic differentiator
    // Returns a unit alongside the pulse window surrounding its creation
    fn unit_with_context(
        &self,
        id: Uuid,
        context_window_secs: u64,
    ) -> Result<Option<(KnowledgeUnit, Vec<PulseEvent>)>>;
}

// ── ambient-reasoning: Rig-backed implementation ──────────────────────────
// ReasoningEngine is the thin crossing point. Rig is the implementation
// detail inside ambient-reasoning — invisible to all other crates.
// Default backend: ollama (local). Paid tier: remote Claude/OpenAI via Rig.
// Swapping backends requires changing only ambient-reasoning's Cargo.toml
// and the RigReasoningEngine initializer — zero changes elsewhere.
pub trait ReasoningEngine: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn answer(&self, question: &str, context: &[KnowledgeUnit]) -> Result<String>;
}

// ── ambient-triggers ───────────────────────────────────────────────────────
pub trait TriggerCondition: Send + Sync {
    fn evaluate(
        &self,
        unit: &KnowledgeUnit,
        store: &dyn KnowledgeStore,
    ) -> Result<bool>;
}

pub trait TriggerAction: Send + Sync {
    fn fire(&self, unit: &KnowledgeUnit) -> Result<()>;
}

// ── ambient-query: single query engine, called by all three search surfaces ─
pub trait QueryEngine: Send + Sync {
    fn query(&self, req: QueryRequest) -> Result<Vec<QueryResult>>;
}

// ── ambient-spotlight: two-direction Spotlight integration ──────────────────
// Inbound — implements SourceAdapter, emits RawEvents from NSMetadataQuery
pub struct SpotlightAdapter;  // implements SourceAdapter

// Outbound — exports KnowledgeUnits to CSSearchableIndex for Cmd+Space
pub trait SpotlightExporter: Send + Sync {
    fn export(&self, unit: &KnowledgeUnit) -> Result<()>;
    fn delete(&self, id: Uuid) -> Result<()>;
}
```

---

## Storage Architecture

### LadybugDB (ambient-store — graph + vector domain)

LadybugDB is an embedded, in-process graph database built on KuzuDB. It owns:
- The property graph of notes, concepts, and their relationships
- Vector embeddings for semantic search (HNSW index, native)
- Full-text search index

**Graph schema (Cypher DDL):**
```cypher
CREATE NODE TABLE Note (
    id       STRING PRIMARY KEY,
    source   STRING,
    title    STRING,
    content  STRING,
    hash     STRING,
    created  TIMESTAMP
);

CREATE NODE TABLE Concept (
    id    STRING PRIMARY KEY,
    label STRING
);

CREATE REL TABLE LINKS_TO   (FROM Note TO Note);
CREATE REL TABLE MENTIONS   (FROM Note TO Concept);
CREATE REL TABLE CO_ACTIVE  (FROM Note TO Note, overlap_seconds INT64);
```

**Rule:** All LadybugDB access is encapsulated inside `ambient-store`. No other crate imports `ladybug` directly.

### tsink (ambient-store — time-series domain)

tsink is a lightweight embedded time-series database. It owns all `PulseEvent` data.

**Storage configuration:**
```rust
let pulse_store = StorageBuilder::new()
    .with_data_path("~/.ambient/pulse")
    .with_partition_duration(Duration::from_secs(3600))    // 1-hour partitions
    .with_retention(Duration::from_secs(90 * 24 * 3600))  // 90-day retention
    .with_wal_sync_mode(WalSyncMode::Periodic(Duration::from_secs(1)))
    .build()?;
```

**Metric names (label convention):**
```
context_switch_rate      labels: {}
active_app               labels: { bundle_id, window_title }
audio_input_active       labels: { active: "true" | "false" }
time_context             labels: { hour, day_of_week, is_weekend }
```

**Rule:** tsink is append-only. No deletions. Retention handles expiry automatically.

---

## Spotlight Integration (ambient-spotlight)

Spotlight integration runs in two independent directions. Neither depends on the other.

### Inbound — NSMetadataQuery as Universal Source Adapter

Instead of building individual adapters for PDFs, Mail, Safari bookmarks, Pages documents, and other file types, `SpotlightAdapter` queries `NSMetadataQuery` for file change events across everything macOS has already indexed. This collapses Phase 5 "hard sources" into a single adapter.

**What it watches:**
```
kMDItemContentType    — file type (PDF, email, web archive, etc.)
kMDItemTextContent    — extracted text (Spotlight has already done the parsing)
kMDItemDisplayName    — filename / title
kMDItemLastUsedDate   — last accessed timestamp
kMDItemFSContentChangeDate — last modified
```

**Implementation approach:** `SpotlightAdapter` is an ObjC bridge via `objc2`. It registers an `NSMetadataQuery` with a predicate for content types of interest, observes `NSMetadataQueryDidUpdateNotification`, and emits `RawEvent { payload: RawPayload::SpotlightItem { text_content: kMDItemTextContent, .. } }` for each changed item. A `SpotlightNormalizer` in `ambient-normalizer` handles `RawPayload::SpotlightItem` — it extracts `text_content` directly, bypassing the need to open or parse the file. Internal dispatch by `content_type` handles per-format variations (email vs PDF vs web archive).

**Entitlement required:** `com.apple.security.files.user-selected.read-only` for files outside the sandbox. For a non-App-Store distribution this is straightforward.

**What Spotlight inbound replaces:** dedicated adapters for Safari reading list, Mail, Pages, Numbers, Keynote, PDF files, and any other app that writes Spotlight metadata. This is the majority of Phase 5.

### Outbound — CSSearchableIndex for Cmd+Space

`SpotlightExporter` writes `KnowledgeUnit`s to `CSSearchableIndex` so they surface in native Spotlight (`Cmd+Space`) searches. Each unit becomes a `CSSearchableItem` with:

```swift
// Expressed as objc2 Rust bindings
CSSearchableItemAttributeSet {
    title:       unit.title,
    contentDescription: unit.content[..200],  // truncated summary
    keywords:    derived_concepts,             // from LadybugDB Concept nodes
    contentURL:  "ambient://unit/{unit.id}",   // deep link back into Ambient
}
```

When a user clicks an Ambient result in Spotlight, the `ambient://` URL scheme opens the menu bar overlay focused on that unit.

**Rule:** `SpotlightExporter` is called by the ingestion pipeline after every successful `KnowledgeStore.upsert()`. It is fire-and-forget — export failures must never block ingestion.

---

`ambient-watcher` has two responsibilities: watching knowledge sources and sampling cognitive context. These run as separate tokio tasks feeding the same `ambient-store`.

### Samplers to implement

| Sampler | macOS API | Cadence | Permission |
|---|---|---|---|
| `ContextSwitchSampler` | `NSWorkspace.activeApplication` diff | 5s tick | None |
| `ActiveAppSampler` | `NSWorkspace.activeApplication` + `AXUIElement` for window title | 5s tick | Accessibility (opt-in) |
| `AudioInputSampler` | `AVCaptureDevice` input activity | Edge-triggered | Microphone |

**Default config (`~/.ambient/config.toml`):**
```toml
[samplers]
context_switches = true     # safe, NSWorkspace only, no permission needed
active_app_titles = false   # opt-in — requires Accessibility permission
audio_input = true          # edge-triggered, requires Microphone permission
```

**First-launch permission strategy:** request only Microphone on first launch (required for `audio_input = true` default). Accessibility is requested only when the user explicitly enables `active_app_titles` in config — explain the value proposition before asking. This keeps the initial permission dialog minimal and non-alarming.

**Privacy rule:** `ActiveAppSampler` captures window titles only for allow-listed apps (configurable in `~/.ambient/config.toml`). This rule is enforced at the sampler level, not assumed by callers.

**`TimeContext` rule:** Derived at `KnowledgeUnit` ingestion time from `unit.observed_at`. Not sampled. Written to tsink alongside the unit's timestamp so it can be queried in the same time window.

---

## Cognitive Context Correlation

The `unit_with_context` query is the system's core differentiator. It retrieves a `KnowledgeUnit` alongside the pulse window surrounding its creation:

```rust
// Example: retrieve a note with 2 minutes of context on each side
let (unit, pulse) = store.unit_with_context(note_id, 120)?;

// Derive cognitive state from the pulse window
let avg_switch_rate = pulse.iter()
    .filter_map(|e| match &e.signal {
        PulseSignal::ContextSwitchRate { switches_per_minute } => Some(switches_per_minute),
        _ => None,
    })
    .average();

let was_in_flow = avg_switch_rate < 2.0;
let was_on_call = pulse.iter().any(|e| matches!(
    &e.signal,
    PulseSignal::AudioInputActive { active: true }
));
```

The Pattern Detector uses this to cluster notes by cognitive state and surface insights like:
- "Your clearest architectural thinking happens Tuesday mornings in low-switch-rate sessions"
- "Notes written during calls are 3x more likely to be linked to follow-up action items"

---

---

## Search Surfaces

All three surfaces are pure presentation. All search logic lives exclusively in `ambient-query` behind the `QueryEngine` trait. No surface may import `ambient-store`, `ambient-reasoning`, or `ambient-patterns` directly.

### 1. Menu Bar Overlay (ambient-menubar) — Primary Surface

A floating search window triggered by `Cmd+Shift+Space` (global hotkey via `CGEventTap`). Implemented with AppKit via `objc2`.

**Interaction model:**
- Hotkey → window appears centered on screen, text field focused
- User types natural language query → debounced 300ms → `QueryEngine.query()`
- Results render as a list: title, source badge, cognitive state badge ("flow", "on call", "late night")
- `Enter` opens the source file at the correct location
- `Cmd+Enter` opens the unit in a detail view with full pulse context timeline
- `Esc` dismisses — no persistent window, no dock icon

**Cognitive state badges** (derived from `QueryResult.cognitive_state`):

| Badge | Condition |
|---|---|
| 🟢 Flow | `was_in_flow: true` |
| 📞 On Call | `was_on_call: true` |
| 🌙 Late Night | `time_of_day >= 22 \|\| time_of_day < 6` |
| 🌅 Early Morning | `time_of_day >= 5 && time_of_day < 8` |

**Rule:** `ambient-menubar` depends only on `ambient-core` (for `QueryRequest` / `QueryResult`) and `ambient-query`. It has no knowledge of LadybugDB, tsink, or ollama.

### 2. Local HTTP API (ambient-cli) — Integration Surface

An `axum` server on `localhost:7474` (configurable). This is the surface for Raycast plugins, Obsidian community plugins, Alfred workflows, and any other tool that wants to query Ambient's intelligence layer.

**Endpoints:**

```
POST /query
  Body:  { "text": "...", "k": 10, "include_pulse_context": false }
  Returns: QueryResult[]

GET  /unit/:id
  Returns: QueryResult (single unit with full pulse context)

GET  /related/:id?depth=2
  Returns: KnowledgeUnit[] (graph traversal)

GET  /health
  Returns: { "status": "ok", "units": 1234, "sources": ["obsidian", "spotlight"] }
```

**Authentication:** localhost-only binding by default. Optional shared secret header (`X-Ambient-Token`) for users who expose the port via SSH tunnel.

**Rule:** The HTTP API is a thin axum layer over `QueryEngine`. No business logic in route handlers.

### 3. CLI (ambient-cli) — Power User / Developer Surface

```bash
ambient watch                          # start the daemon
ambient query "distributed consensus"  # semantic search
ambient query --pulse "panic notes"    # search with cognitive context
ambient unit <id>                      # show unit + full pulse timeline
ambient related <id>                   # graph neighbors
ambient status                         # daemon health + ingestion stats
ambient export --format json           # dump knowledge base
```

**Rule:** CLI commands call `QueryEngine` or the daemon's HTTP API — never `KnowledgeStore` directly. The CLI binary is also the daemon process entry point.

---

## Tech Stack
|---|---|
| Graph + Vector store | `ladybug` (LadybugDB — embedded KuzuDB fork) |
| Time-series store | `tsink` |
| File watching | `notify` |
| Spotlight inbound (NSMetadataQuery) | `objc2` + `objc2-foundation` |
| Spotlight outbound (CSSearchableIndex) | `objc2` + `objc2-core-spotlight` |
| macOS app/window sampling | `core-foundation`, `accessibility-sys` |
| Global hotkey (menu bar) | `core-graphics` (CGEventTap — menu bar only, not system-wide) |
| Audio input detection | `coreaudio-rs` |
| Menu bar UI | `objc2` + `objc2-app-kit` |
| Async runtime | `tokio` |
| Markdown parsing | `pulldown-cmark` |
| Protobuf (Apple Notes) | `prost` |
| LLM framework | `rig-core` (provider abstraction, RAG agent, EmbeddingModel trait) |
| Local inference backend | `ollama` (sidecar process, default) |
| Remote inference backend | Rig's Claude / OpenAI provider (paid tier opt-in) |
| HTTP server (query API) | `axum` |
| CLI | `clap` |
| Serialization | `serde` + `serde_json` |
| Logging | `tracing` + `tracing-subscriber` |
| Errors (library crates) | `thiserror` |
| Errors (binary / tests) | `anyhow` |

---

## System Load Management

A daemon that spins up fans or drains battery will be uninstalled within a day. Load management is not an optimization — it is a first-class architectural concern. Every component has a budget. Budgets are enforced in code, not assumed.

### Memory Budget

| Component | Resident Budget |
|---|---|
| LadybugDB (mature graph, ~50k notes) | 80 MB |
| tsink (90-day pulse retention) | 30 MB |
| ambient-watcher + samplers | 15 MB |
| ambient-query + reasoning cache | 20 MB |
| **Total daemon (excl. ollama)** | **< 150 MB** |

ollama runs as a separate process. Its memory (1–4 GB depending on model) does not count against the daemon. If ollama is not running, all non-semantic features must continue to work.

### CPU Budget

**FSEvents / file watching:** zero cost at rest. `notify` uses kernel-level `FSEvents` — no polling. Cost only materializes on actual file writes.

**Pulse samplers:** must run exclusively on E-cores (efficiency cores). All sampler work is simple ObjC calls — no compute. The 5-second tick must consume < 1ms of CPU per cycle.

**`AXUIElement` (window title):** accessibility API calls can block if the target app is unresponsive. Hard timeout: 500ms. On timeout: skip, log, do not retry in the same cycle.

**Embedding generation:** the highest CPU risk. Embeddings are generated via ollama (out-of-process) but the queue management and request dispatch is in-process.

```rust
// ambient-reasoning: EmbeddingQueue rules
// - Rig's EmbeddingModel trait is the provider interface
// - Default backend: ollama local (rig-core ollama provider)
// - Paid tier backend: remote provider via Rig (Claude/OpenAI) — swapped at init
// - Max 2 concurrent Rig embedding requests at any time
// - Pause queue entirely when ANY of these conditions are true:
//   - on_battery && battery_percent < 50
//   - cpu_sustained_above_60pct_for_30s (sampled via sysinfo)
//   - user_active (context_switch_rate > 0.0 in last 60s from tsink)
// - Embeddings are optional — KnowledgeUnit.embedding = None is valid
// - Full-text search and graph queries work without embeddings
// - Semantic search degrades gracefully to FTS when embedding is None
```

**Pattern Detector:** batch graph analysis is expensive. It must never run during active work hours.

```rust
// ambient-patterns: PatternDetector scheduling rules
// Only run when ALL of these conditions are true:
//   - system_idle_minutes > 10  (via IOPMAssertion)
//   - plugged_in OR battery_percent > 80
//   - local_hour >= 1 && local_hour < 6
// Use DISPATCH_QUEUE_PRIORITY_BACKGROUND for all LadybugDB queries
// Abort and reschedule if user becomes active mid-run
```

### Battery Test (Mandatory Gate After Phase 2)

Before building Phase 3, run the daemon for 4 hours on battery under normal work conditions and verify:

```bash
sudo powermetrics --samplers cpu_power -i 60000 -n 60 | grep "ambient"
# Pass threshold: average CPU power draw < 200mW
# Fail → profile with Instruments, fix polling or blocking calls before proceeding
```

This test is a hard gate. Do not proceed to Phase 3 if the daemon fails it.

### Load-Aware Feature Flags

Add a `SystemLoad` enum to `ambient-core` that all background components must respect:

```rust
pub enum SystemLoad {
    Unconstrained,   // plugged in, idle, full speed
    Conservative,    // on battery OR user active — pause embeddings
    Minimal,         // battery < 20% — pause all background work except watching
}

pub trait LoadAware {
    fn on_load_change(&self, load: SystemLoad);
}
```

The daemon samples `SystemLoad` every 30 seconds via `sysinfo` + `IOPMLib` and broadcasts changes to all registered `LoadAware` components. `EmbeddingQueue`, `PatternDetector`, and `SpotlightExporter` all implement `LoadAware`.

---

## Monetization

### Tier Structure

**Free (forever) — The Core**

Everything that runs locally and makes the system useful:
- Full ingestion pipeline (all source adapters)
- LadybugDB graph + vector store
- tsink pulse store + cognitive context correlation
- Spotlight inbound + outbound integration
- CLI (`ambient watch`, `ambient query`, `ambient status`, `ambient export`)
- Local HTTP API on `localhost:7474`
- Full-text and semantic search (requires local ollama)

The free tier is the distribution engine and the moat builder. Every day it runs, it deepens a personal knowledge graph that is non-portable and uniquely valuable. Gating it defeats both purposes.

**Paid (~$9/month or $85/year) — The Intelligence Layer**

Features that deliver recurring visible value and justify recurring payment:

*Menu bar overlay (`ambient-menubar`)* — the `Cmd+Shift+Space` floating search with cognitive state badges. This is the highest-value, most visible feature and the right thing to gate. It is architecturally clean to gate because `ambient-menubar` is already a separate crate that depends only on `ambient-query`.

*Weekly Pattern Report* — a local HTML report delivered as a macOS notification every Monday morning. Generated by `ambient-patterns` with no cloud dependency. Contents: flow state trends, topic clusters, your highest-insight cognitive contexts, notes that deserve revisiting. The report runs locally but represents ongoing value delivery that justifies ongoing payment.

*Cross-device sync* — encrypted sync of the LadybugDB graph and tsink pulse data across multiple Macs. End-to-end encrypted — the sync server never sees plaintext. Raw note content is never synced (it lives in the source apps). Only the graph structure, embeddings, and pulse time-series are synced. This is the one place a server legitimately enters the architecture.

*Hosted inference (opt-in)* — an optional hosted ollama endpoint for users who don't want to run a local model. Priced at cost + margin. Never the default. Users who run local ollama are unaffected.

**What is never gated:**
- HTTP API (kills third-party ecosystem before it starts)
- Spotlight integration (users would feel surveilled if their Cmd+Space results disappeared on expiry)
- CLI (power users and developers need this; gating it generates resentment)
- Data export (users must always be able to leave)

### Pricing Psychology

The target user already pays for Readwise ($8/mo), Obsidian Sync ($8/mo), and Raycast Pro ($10/mo) — a mental "tools that make me smarter" budget of ~$30/month. $9/month sits comfortably below the friction threshold for this demographic. Annual pricing at $85 (~20% discount) improves retention and cash flow.

### Gating Implementation

Add a `License` type to `ambient-core` and a `LicenseGate` trait. Gated crates (`ambient-menubar`, `ambient-patterns` report feature, sync) check the gate at startup and degrade gracefully — they do not crash or corrupt data on license expiry.

```rust
pub enum License {
    Free,
    Pro { expires_at: DateTime<Utc> },
}

pub trait LicenseGate: Send + Sync {
    fn is_pro(&self) -> bool;
    fn check(&self, feature: GatedFeature) -> Result<(), LicenseError>;
}

pub enum GatedFeature {
    MenuBarOverlay,
    PatternReport,
    CrossDeviceSync,
    HostedInference,
}
```

License verification is local-first — a cached license token validated against a public key. No network call required for daily use. Verification only phones home on first activation and on renewal.

---



Work strictly in this order. Do not implement a later phase until the current phase has a passing integration test.

### Phase 1 — Spine
- [ ] Scaffold full workspace with all crates as empty libs
- [ ] Define all types and traits completely in `ambient-core` (including `RawPayload` enum, `QueryRequest`, `QueryResult`, `CognitiveState`, `SystemLoad`, `License`)
- [ ] Implement `ObsidianAdapter` in `ambient-watcher` — emits `RawPayload::Markdown`
- [ ] Implement `MarkdownNormalizer` in `ambient-normalizer` — handles `RawPayload::Markdown`
- [ ] Implement `LadybugStore` in `ambient-store` (graph + FTS only, no vectors yet)
- [ ] Wire in `ambient-cli` with a `watch` subcommand that prints ingested `KnowledgeUnit`s
- [ ] **Integration test:** modify an Obsidian note → unit appears in LadybugDB

### Phase 2 — Pulse
- [ ] Implement `ContextSwitchSampler` and `ActiveAppSampler` in `ambient-watcher`
- [ ] Implement `KeystrokeDensitySampler` (event count only, no content)
- [ ] Implement `AudioInputSampler`
- [ ] Integrate `tsink` into `ambient-store`, implement `record_pulse` and `pulse_window`
- [ ] Implement `unit_with_context` correlated query
- [ ] Implement `SystemLoad` sampling and `LoadAware` broadcast in `ambient-cli` daemon loop
- [ ] **Integration test:** write a note → pulse window is retrievable alongside the unit
- [ ] **Battery gate:** run daemon 4 hours on battery → `powermetrics` average < 200mW. Do not proceed to Phase 3 until this passes.

### Phase 3 — Intelligence
- [ ] Add `rig-core` to `ambient-reasoning` with ollama backend
- [ ] Implement `RigReasoningEngine` wrapping Rig's `EmbeddingModel` and `Agent` behind `ReasoningEngine` trait
- [ ] Implement `EmbeddingQueue` with load-aware rate limiting (max 2 concurrent, pause on battery/active)
- [ ] Extend `LadybugStore` with vector index via LadybugDB's native HNSW
- [ ] Implement `QueryEngine` in `ambient-query` (semantic search + pulse correlation + Rig RAG answer)
- [ ] Add `query` subcommand to CLI and axum `/query` endpoint — both call `QueryEngine` only
- [ ] Verify graceful degradation: semantic search falls back to FTS when `embedding = None`
- [ ] **Integration test:** ingest 10 notes → `QueryEngine.query()` returns ranked results with cognitive state

### Phase 4 — Pattern & Trigger
- [ ] Implement batch `PatternDetector` in `ambient-patterns` using correlated queries
- [ ] Implement rule-based `TriggerEngine` in `ambient-triggers` with JSON-configurable rules
- [ ] Add macOS notification action via `notify-rust`
- [ ] **Integration test:** pattern fires trigger → notification delivered

### Phase 5 — Spotlight
- [ ] Implement `SpotlightAdapter` in `ambient-spotlight` (inbound: `NSMetadataQuery` via `objc2`) — emits `RawPayload::SpotlightItem`
- [ ] Implement `SpotlightNormalizer` in `ambient-normalizer` — handles `RawPayload::SpotlightItem`, dispatches internally by `content_type`
- [ ] Verify all existing `match payload` sites across the workspace compile with the new `SpotlightItem` variant handled
- [ ] Implement `SpotlightExporter` in `ambient-spotlight` (outbound: `CSSearchableIndex`)
- [ ] Wire exporter into ingestion pipeline as fire-and-forget post-upsert step
- [ ] Register `ambient://` URL scheme handler in `ambient-cli`
- [ ] **Integration test:** ingest a unit → appears in Spotlight search results

### Phase 6 — Menu Bar
- [ ] Implement `ambient-menubar`: global hotkey (`Cmd+Shift+Space`), floating search window, result list with cognitive state badges
- [ ] Deep link handler: `ambient://unit/<id>` opens menu bar overlay focused on unit
- [ ] **Integration test:** hotkey → query → result with correct cognitive state badge

### Phase 7 — Hard Sources
- [ ] `AppleNotesAdapter`: read-only SQLite access to `NoteStore.sqlite`, Protobuf decode via `prost`
- [ ] Additional source adapters as needed (anything not covered by Spotlight inbound)

### Phase 8 — Distribution
- [ ] Homebrew formula
- [ ] Signed `.dmg` via GitHub Actions
- [ ] Raycast extension (calls localhost HTTP API)
- [ ] Setapp submission

---

## Coding Conventions

- All public crate APIs use `thiserror` error types — never `anyhow` at library boundaries
- No `unwrap()` in library crates — only in `ambient-cli` main and tests
- Every trait has a corresponding `Mock*` impl in `#[cfg(test)]` for unit testing
- `RawEvent`, `RawPayload`, `KnowledgeUnit`, `PulseEvent`, `QueryRequest`, and `QueryResult` are the ONLY types crossing crate boundaries in the data pipeline
- `RawPayload` variants must be exhaustively matched everywhere — never use a wildcard `_` arm in production code, only in tests
- Source adapters must never hold locks longer than a single read operation
- `ActiveAppSampler` must filter window titles through a configurable allowlist before writing to tsink
- `ActiveAppSampler` must use `spawn_blocking` for all `AXUIElement` calls with a hard 500ms OS-level timeout — skip and log on timeout, never block the sampler thread
- LadybugDB access is encapsulated entirely within `ambient-store` — no other crate imports `ladybug`
- tsink access is encapsulated entirely within `ambient-store` — no other crate imports `tsink`
- `SpotlightExporter.export()` is fire-and-forget — failures must be logged but must never propagate to the ingestion pipeline
- `ambient-menubar` and `ambient-cli` must not import `ambient-store`, `ambient-reasoning`, or `ambient-patterns` — they depend only on `ambient-core` and `ambient-query`
- All `objc2` calls must be wrapped in `autoreleasepool` blocks to prevent memory leaks
- All background components (`EmbeddingQueue`, `PatternDetector`, `SpotlightExporter`) must implement `LoadAware` and respect `SystemLoad::Conservative` and `SystemLoad::Minimal`
- `AXUIElement` calls must use a 500ms timeout — skip and log on timeout, never block
- Gated features (`ambient-menubar`, pattern reports, sync) must degrade gracefully on `License::Free` — no panics, no data corruption, no silent data loss
- License verification never makes a network call during normal daemon operation — local token + public key only

---

## Current Task

**Phase 1 — Spine.**

Scaffold the full workspace. Implement `ambient-core` types and traits completely. Then implement the Obsidian → LadybugDB pipeline end to end. Do not touch `ambient-pulse`, `ambient-reasoning`, `ambient-patterns`, or `ambient-triggers` until the Phase 1 integration test passes.

---

## Session Prompts

Use these verbatim as the first message in each Claude Code session.

### Session 1 — Scaffold

```
Read CLAUDE.md fully before writing any code.

Your only task in this session is Phase 1, Step 1: scaffold the workspace.

Create the following and nothing else:
- Root Cargo.toml as a workspace manifest listing all member crates
- All 10 crates as empty libs with their own Cargo.toml and src/lib.rs
- Each Cargo.toml should declare only the dependencies that crate will
  directly use based on CLAUDE.md — no placeholders, no extras
- Inter-crate dependencies must match CLAUDE.md exactly:
    ambient-query   → depends on ambient-core, ambient-store, ambient-reasoning
    ambient-menubar → depends on ambient-core, ambient-query ONLY
    ambient-cli     → depends on ambient-core, ambient-query, axum, clap
    All other crates depend only on ambient-core and their own external deps

When done, run: cargo check --workspace
Fix all errors until cargo check passes clean. Then stop.

Do not implement any types, traits, or logic. Empty libs only.
The goal is a clean workspace where the dependency graph matches CLAUDE.md
and the Rust compiler will enforce module boundaries going forward.
```

### Session 2 — ambient-core

```
Read CLAUDE.md. Scaffold is complete, cargo check passes.

Implement ambient-core completely. Nothing else.

Types to implement:
- RawPayload (enum with Markdown, AppleNote, SpotlightItem, PlainText variants)
- RawEvent
- KnowledgeUnit
- PulseSignal (enum), PulseEvent
- QueryRequest, QueryResult, CognitiveState
- SystemLoad (enum), License (enum)
- SourceId (newtype over String)

Traits to implement (with full method signatures as specified in CLAUDE.md):
- SourceAdapter, PulseSampler, Normalizer
- KnowledgeStore, ReasoningEngine, QueryEngine
- SpotlightExporter, TriggerCondition, TriggerAction
- LoadAware, LicenseGate

For every trait, add a Mock* implementation under #[cfg(test)] that:
- Stores calls in a Vec for assertion
- Returns Ok(Default::default()) or empty Vec for query methods
- Panics with a clear message if an unexpected method is called

Rules:
- No #[allow(dead_code)] — if a field or method seems unused, check CLAUDE.md
- All RawPayload match sites must be exhaustive — no wildcard _ arms
- Derive Debug, Clone, serde::Serialize, serde::Deserialize on all data types

Run: cargo test -p ambient-core
All tests must pass. Then stop.
```

### Session 3 — Phase 1 Pipeline

```
Read CLAUDE.md. ambient-core is complete, cargo test -p ambient-core passes.

Implement the full Phase 1 pipeline. Work strictly in this order.
Complete and verify each step before moving to the next.

Step 1 — ambient-watcher: ObsidianAdapter
- Watch a configurable vault path (from ~/.ambient/config.toml)
- Use notify crate with recursive watching and FSEvents backend on macOS
- Emit RawEvent { payload: RawPayload::Markdown { content, path } }
- Debounce: coalesce events for the same path within 500ms
- Ignore: .obsidian/ directory, hidden files, non-.md files
- Unit test: write a .md file to a temp dir → assert RawEvent emitted

Step 2 — ambient-normalizer: MarkdownNormalizer
- Implement Normalizer for RawPayload::Markdown
- can_handle: matches RawPayload::Markdown only
- Extract title: first H1 heading, fallback to filename stem
- Extract content: plain text via pulldown-cmark (strip all markdown syntax)
- Extract links: [[wikilinks]] → metadata["links"] as Vec<String>
- Extract tags: #tags → metadata["tags"] as Vec<String>
- Set content_hash: blake3 of normalized content string
- Set embedding: None (Phase 3)
- Unit test: sample markdown input → assert all KnowledgeUnit fields correct

Step 3 — ambient-store: LadybugStore
- Implement KnowledgeStore using LadybugDB at ~/.ambient/graph
- Use the exact Cypher DDL schema from CLAUDE.md
- upsert: create or update Note node, idempotent on content_hash
- search_fulltext: LadybugDB FTS on content and title fields
- get_by_id: lookup by UUID
- related: Cypher traversal via LINKS_TO edges, respect depth param
- Unimplemented stubs (return Err(AmbientError::NotImplemented)):
    search_semantic, record_pulse, pulse_window, unit_with_context
- Unit test: upsert → get_by_id → assert round-trip fidelity

Step 4 — ambient-cli: watch subcommand
- Wire ObsidianAdapter → MarkdownNormalizer → LadybugStore in tokio event loop
- After each successful upsert, also create LINKS_TO edges for metadata["links"]
- Print each ingested KnowledgeUnit to stdout as pretty JSON
- Graceful Ctrl+C shutdown
- Create ~/.ambient/config.toml with defaults if missing

Integration test (tests/integration/phase1.rs):
- Spin up temp vault with 3 .md files, one containing [[wikilinks]]
- Start watch loop in background tokio task
- Modify one file
- Wait up to 2 seconds for ingestion
- Assert all 3 units present in LadybugStore via get_by_id
- Assert modified unit has updated content
- Assert LINKS_TO edges exist for wikilinks

Run: cargo test --workspace
All tests must pass. Then stop.
Do not implement anything from Phase 2 or beyond.
```

### Session 4 — Phase 2: Pulse

```
Read CLAUDE.md. Phase 1 pipeline is complete, cargo test --workspace passes.

This session implements Phase 2: pulse sampling and correlated query.
Work strictly in this order.

Step 1 — ambient-watcher: System Telemetry Samplers
Implement three samplers, each implementing PulseSampler and LoadAware.
All emit PulseEvent into the provided Sender<PulseEvent> channel.

ContextSwitchSampler:
- 5-second tick via tokio::time::interval
- Call NSWorkspace.activeApplication via objc2 each tick
- Compare bundle_id to previous tick — increment switch count on change
- Emit PulseSignal::ContextSwitchRate { switches_per_minute } as rolling 60s window
- On SystemLoad::Minimal: pause entirely, do not emit

ActiveAppSampler (only starts if config active_app_titles = true):
- 5-second tick
- NSWorkspace.activeApplication for bundle_id
- AXUIElement for window title via spawn_blocking with hard 500ms OS-level timeout
- Skip and log on timeout — never block the sampler thread
- Filter window_title through allowlist in ~/.ambient/config.toml
- Emit PulseSignal::ActiveApp { bundle_id, window_title }
- On SystemLoad::Minimal: emit bundle_id only, skip AXUIElement call
- If config active_app_titles = false: do not start, do not request Accessibility

AudioInputSampler:
- Edge-triggered via objc2 KVO on AVCaptureDevice
- Emit PulseSignal::AudioInputActive { active: true } on input start
- Emit PulseSignal::AudioInputActive { active: false } on input stop
- On SystemLoad::Minimal: continue — low cost, high signal

Unit tests:
- ContextSwitchSampler: on_load_change(Minimal) → no events emitted
- ActiveAppSampler: on_load_change(Minimal) → bundle_id only, no window_title
- AudioInputSampler: on_load_change(Minimal) → continues emitting normally

Step 2 — ambient-watcher: SystemLoad Sampler + LoadBroadcaster
- Sample SystemLoad every 30 seconds via sysinfo + IOPMLib:
    Unconstrained: plugged_in AND cpu_30s_avg < 40%
    Conservative:  on_battery OR cpu_30s_avg > 60% OR context_switch_rate > 0 in last 60s
    Minimal:       battery_percent < 20%
- LoadBroadcaster: holds Vec<Arc<dyn LoadAware>>, broadcasts only on state transitions

Step 3 — ambient-store: tsink integration
- Add tsink to ambient-store Cargo.toml
- Initialize StorageBuilder per CLAUDE.md storage configuration
- Implement KnowledgeStore::record_pulse
  Map PulseSignal variants to tsink metric names and labels per CLAUDE.md
- Implement KnowledgeStore::pulse_window
  Query tsink for [from, to] range, reconstruct PulseEvents in timestamp order
- Implement KnowledgeStore::unit_with_context:
    1. get_by_id(id) from LadybugDB
    2. pulse_window(observed_at - window, observed_at + window) from tsink
    3. Derive CognitiveState:
         was_in_flow:   avg ContextSwitchRate < 2.0 over window
         was_on_call:   any AudioInputActive { active: true } in window
         dominant_app:  most frequent bundle_id in ActiveApp events
         time_of_day:   hour from unit.observed_at
    4. Return (KnowledgeUnit, Vec<PulseEvent>, CognitiveState)
- Unit test: record_pulse 5 events → pulse_window → assert correct order and fields

Step 4 — ambient-cli: wire pulse pipeline
- Start enabled samplers as tokio tasks based on config
- Route Sender<PulseEvent> into KnowledgeStore::record_pulse
- Register all samplers with LoadBroadcaster
- Add ambient status subcommand:
    Last 10 PulseEvents from tsink
    Current SystemLoad
    Unit count, last ingested title, last ingested timestamp

Integration test (tests/integration/phase2.rs):
- Inject 5 synthetic PulseEvents with known timestamps via record_pulse
- Ingest one Obsidian note
- Call unit_with_context with 120s window
- Assert correct PulseEvents in window
- Assert CognitiveState correctly derived from injected events
- Assert LoadBroadcaster fans out to all registered samplers on state change

Battery gate — mandatory before proceeding to Phase 3:
- Run daemon 4 hours on battery under normal work conditions
- Run: sudo powermetrics --samplers cpu_power -i 60000 -n 60 | grep ambient
- Pass: average CPU power draw < 200mW
- Fail: profile with Instruments Energy Log, fix offending call, re-run
  Most likely cause: AXUIElement timeout not enforced cleanly in spawn_blocking
  Do not proceed to Phase 3 until this passes.

Run: cargo test --workspace
All tests must pass. Then stop.
Do not implement anything from Phase 3 or beyond.
```

### Session 5 — Phase 3: Intelligence

```
Read CLAUDE.md. Phase 2 pulse pipeline is complete, battery gate passed,
cargo test --workspace passes.

This session implements Phase 3: embedding generation, semantic search,
and natural language Q&A via Rig. Work strictly in this order.

Step 1 — ambient-reasoning: RigReasoningEngine
Add rig-core to ambient-reasoning Cargo.toml with the ollama feature flag.
Verify rig-core has a stable ollama provider before writing any code —
check https://docs.rs/rig-core for the current ollama backend API.

Implement RigReasoningEngine behind the ReasoningEngine trait:

struct RigReasoningEngine {
    embedder: Box<dyn EmbeddingModel>,       // rig-core trait
    agent:    Agent<Box<dyn CompletionModel>>, // rig-core RAG agent
    backend:  ReasoningBackend,
}

pub enum ReasoningBackend {
    Local { ollama_base_url: String },       // default, free tier
    Remote { provider: String },             // paid tier, set at init
}

embed():
- Call Rig's EmbeddingModel::embed_text
- Return Vec<f32>
- Timeout: 30s — return Err if ollama is unresponsive, never block

answer():
- Build RAG context from &[KnowledgeUnit] — format each unit as
  "Title: {title}\nSource: {source}\nContent: {content}"
- Pass context to Rig Agent with question as prompt
- Return String response
- Timeout: 60s

EmbeddingQueue (also in ambient-reasoning):
- tokio mpsc channel receiving (Uuid, String) — unit id + content to embed
- Max 2 concurrent Rig requests via tokio::sync::Semaphore
- Implements LoadAware:
    Unconstrained: process normally
    Conservative:  pause queue, drain in-flight requests
    Minimal:       pause queue immediately
- On completion: call KnowledgeStore::upsert with updated embedding
- Embeddings are optional — queue failures must never block ingestion
- Unit test: inject 5 items, assert max 2 in-flight at any time

Ollama availability check:
- On daemon startup, GET http://localhost:11434/api/tags
- If unreachable: log warning, set embedding_available = false
- All semantic features degrade gracefully to FTS
- Recheck every 60 seconds — re-enable when ollama comes back
- Never crash or refuse to start because ollama is absent

Step 2 — ambient-store: vector index
Extend LadybugStore with LadybugDB's native HNSW vector index:
- Add vector column to Note table (dimension must match embedding model)
- Implement KnowledgeStore::search_semantic:
    If embedding_available: generate query embedding via ReasoningEngine,
    run HNSW nearest-neighbor search, return top-k KnowledgeUnits
    If not available: fall back to search_fulltext, log degradation
- Update upsert: if KnowledgeUnit.embedding is Some, write to vector column
- Unit test: upsert 5 units with embeddings → search_semantic returns
  correct nearest neighbor

Step 3 — ambient-query: QueryEngine implementation
Implement QueryEngine in ambient-query:

struct AmbientQueryEngine {
    store:     Arc<dyn KnowledgeStore>,
    reasoning: Arc<dyn ReasoningEngine>,
}

query():
1. search_semantic(text, k) — or search_fulltext if embeddings unavailable
2. For each result: unit_with_context(id, context_window_secs) if
   include_pulse_context = true
3. Pass top results as RAG context to ReasoningEngine::answer
4. Assemble QueryResult vec with scores, pulse_context, cognitive_state
5. Return ranked results

Graceful degradation chain (must be tested explicitly):
- ollama down → FTS search + no LLM answer, return results with
  answer = None
- LadybugDB HNSW unavailable → FTS fallback
- tsink unavailable → return results without pulse_context

Unit test: MockReasoningEngine + MockKnowledgeStore → assert
QueryEngine assembles QueryResult correctly from mock responses

Step 4 — ambient-cli: wire query surface
- Wire EmbeddingQueue into daemon loop:
    After each KnowledgeStore::upsert, send (id, content) to EmbeddingQueue
    Register EmbeddingQueue with LoadBroadcaster
- Start axum server on localhost:7474 with POST /query and GET /health
- Implement ambient query CLI subcommand:
    ambient query "distributed consensus"
    ambient query --pulse "panic notes"   (includes cognitive context)
    Both call QueryEngine, print results as formatted table or JSON (--json flag)
- ambient status: add embedding_available field and queue depth

Integration test (tests/integration/phase3.rs):
- Ingest 10 .md notes with varied content into LadybugDB
- Inject embeddings directly (bypass ollama — use random unit vectors for test)
- Call QueryEngine::query with a text query
- Assert results are ranked (highest score first)
- Assert graceful degradation: set embedding_available = false →
  query still returns results via FTS
- Assert /query HTTP endpoint returns same results as CLI

Run: cargo test --workspace
All tests must pass. Then stop.
Do not implement anything from Phase 4 or beyond.
```
