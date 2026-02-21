# CLAUDE.md-2 — Amendment Layer

This document amends CLAUDE.md and CLAUDE.md-1. Where this document
conflicts with either, this document takes precedence.

Read CLAUDE.md, then CLAUDE.md-1, then this document in full before
writing any code.

---

## Amendment 7 — Materialized Correlation Table

### Decision

The cross-store join between CozoDB and tsink that produces
`unit_with_context` is moved from read time to write time. A
`cognitive_snapshots` relation is materialized into CozoDB at note
upsert time. The PatternDetector runs entirely inside CozoDB Datalog
with no tsink dependency at query time.

tsink is retained as the authoritative compressed store for raw
high-frequency pulse telemetry. Its role is archival and compression,
not correlation. The dual-store architecture is preserved.

### Why This Decision Was Made

The PatternDetector must run correlation across the entire knowledge
corpus — potentially thousands of notes, each requiring a tsink window
query. A batch cross-store join at pattern detection time has no shared
query planner. All optimization burden falls on application code.

Materializing the derived CognitiveState at write time (once per upsert)
eliminates the cross-store join from every read path. The PatternDetector
becomes pure Datalog. The query planner in CozoDB optimizes freely across
the materialized relation.

### New CozoDB Relation

Add to the schema in Amendment 1:

```datalog
:create cognitive_snapshots {
    unit_id: String             =>
    observed_at: Float,
    window_secs: Int,           # window used for derivation (default: 120)
    was_in_flow: Bool,
    was_on_call: Bool,
    dominant_app: String?,
    time_of_day: Int,
    context_switch_rate: Float,
    was_in_meeting: Bool,
    was_in_focus_block: Bool,
    energy_level: Int?,
    mood_level: Int?,
    hrv_score: Float?,
    sleep_quality: Float?,
    minutes_since_last_meeting: Int?
}
```

### Upsert Pipeline Change

After every note upsert in `ambient-store`, materialize the cognitive
snapshot synchronously before returning:

```rust
fn upsert(&self, unit: KnowledgeUnit) -> Result<()> {
    // 1. Write note to CozoDB
    self.cozo.upsert_note(&unit)?;

    // 2. Query tsink for pulse window around observed_at
    let window = Duration::seconds(120);
    let pulse = self.tsink.pulse_window(
        unit.observed_at - window,
        unit.observed_at + window,
    )?;

    // 3. Derive CognitiveState from pulse events
    let state = derive_cognitive_state(&pulse);

    // 4. Materialize into CozoDB — single write, no future tsink dependency
    self.cozo.upsert_cognitive_snapshot(unit.id, 120, &state)?;

    Ok(())
}
```

`derive_cognitive_state` lives in `ambient-core`. It is the single
place where `Vec<PulseEvent>` becomes `CognitiveState`. No inline
derivation anywhere else.

### KnowledgeStore Trait Extension

Split `unit_with_context` into two methods:

```rust
// Fast path — uses materialized cognitive_snapshots in CozoDB
// Used by: QueryEngine, menu bar overlay, CLI query
fn unit_with_context(
    &self,
    id: Uuid,
) -> Result<(KnowledgeUnit, CognitiveState)>;

// Live path — queries tsink directly, re-derives with specified window
// Used by: `ambient context --live`, HTTP /context?live=true
fn unit_with_context_live(
    &self,
    id: Uuid,
    window_secs: u32,
) -> Result<(KnowledgeUnit, Vec<PulseEvent>, CognitiveState)>;
```

### PatternDetector Becomes Pure Datalog

`ambient-patterns` queries only CozoDB. No tsink calls. Example:

```datalog
?[unit_id, title, observed_at] :=
    *notes{ id: unit_id, title, observed_at },
    *cognitive_snapshots{
        unit_id,
        was_in_flow: true,
        hrv_score: hrv,
        time_of_day: hour,
        was_in_meeting: false
    },
    hrv > 55.0,
    hour >= 6,
    hour < 12,
    is_weekday(observed_at)
```

`ambient-patterns/Cargo.toml` has no tsink dependency — enforced at
the crate boundary.

### tsink Retained Responsibilities

1. Raw pulse event archival (~1.37 bytes/sample via Gorilla compression)
2. Live system status (`ambient status` reads last 60s)
3. EmbeddingQueue gate (reads last 60s of ContextSwitchRate)
4. `unit_with_context_live` re-derivation with arbitrary window sizes
5. Long-horizon behavioral analytics over the full continuous signal

tsink is NOT used by: PatternDetector, TriggerEngine, QueryEngine,
ambient-menubar. All of these use the materialized fast path.

### Window Size Policy

Default: 120 seconds. Configurable:

```toml
[store]
correlation_window_secs = 120
```

`ambient reindex --rederive` triggers full re-materialization from tsink.

### Coding Conventions

- `derive_cognitive_state` is the single entry point — no inline derivation
- `upsert_cognitive_snapshot` always called in the same transaction as
  `upsert_note` — partial writes are invalid
- `ambient-patterns/Cargo.toml` must not import tsink

---

## Amendment 8 — StreamProvider and StreamTransport as Separate Abstractions

### Decision

The channel-based ingestion pipeline is replaced by two clearly-bounded,
independently-substitutable abstractions:

**`StreamProvider`** — owns the durable append-only log (WAL). Answers:
*how is data stored and delivered to consumers?* Implemented by
`SqliteStreamProvider` (production) and `InMemoryStreamProvider` (tests).

**`StreamTransport`** — ingests events from an external source and
writes into a `StreamProvider`. Answers: *where does data come from
and how does it arrive?* Every source — local OS, network peer, cloud
platform, third-party health API — is a `StreamTransport`.

**Why they are separate:** Transport crates must have no storage
dependency. Stream provider crates must have no network or OS dependency.
Enforcing this at the crate boundary — not as convention — means a
`BonjourTransport` author genuinely cannot reach into the WAL, and a
`SqliteStreamProvider` implementation genuinely cannot import CloudKit
bindings. The Cargo.toml of each plugin crate enforces the boundary.

The existing `SourceAdapter` and `PulseSampler` traits are retired.
All existing adapters and samplers become `StreamTransport` implementations.
Internal logic is unchanged in every case.

### StreamProvider Trait (ambient-core)

```rust
pub type Offset = u64;
pub type ConsumerId = String;

pub enum EventLogEntry {
    Raw(RawEvent),
    Pulse(PulseEvent),
}

pub trait StreamProvider: Send + Sync {
    // --- Producer interface (transports call these) ---

    /// Local event — no dedup needed
    fn append_raw(&self, event: RawEvent) -> Result<Offset>;
    fn append_pulse(&self, event: PulseEvent) -> Result<Offset>;

    /// Remote event — transport name and originating record_id for dedup
    fn append_raw_from(
        &self,
        event: RawEvent,
        transport: &str,
        record_id: &str,
    ) -> Result<Offset>;

    // --- Consumer interface ---

    fn read(
        &self,
        consumer: &ConsumerId,
        from: Offset,
        limit: usize,
    ) -> Result<Vec<(Offset, EventLogEntry)>>;

    fn commit(
        &self,
        consumer: &ConsumerId,
        offset: Offset,
    ) -> Result<()>;

    fn last_committed(
        &self,
        consumer: &ConsumerId,
    ) -> Result<Option<Offset>>;

    fn subscribe(
        &self,
        consumer: &ConsumerId,
        tx: tokio::sync::watch::Sender<Offset>,
    ) -> Result<()>;
}
```

### StreamTransport Trait (ambient-core)

```rust
pub type TransportId = String;

pub struct TransportStatus {
    pub transport_id: TransportId,
    pub state: TransportState,
    pub peers: Vec<PeerStatus>,      // empty for non-peer transports
    pub last_event_at: Option<DateTime<Utc>>,
    pub events_ingested: u64,
}

pub enum TransportState {
    Active,
    Degraded { reason: String },
    Inactive,
    RequiresSetup { action: SetupAction },
}

pub struct PeerStatus {
    pub peer_id: String,             // hashed device identifier
    pub device_name: String,
    pub last_seen: DateTime<Utc>,
    pub events_received: u64,
    pub reachable: bool,
}

pub trait StreamTransport: Send + Sync {
    fn transport_id(&self) -> TransportId;

    /// Start ingesting. Transport writes to provider via append_* methods.
    /// Returns a JoinHandle — transport runs on its own task.
    fn start(
        &self,
        provider: Arc<dyn StreamProvider>,
    ) -> Result<tokio::task::JoinHandle<()>>;

    fn stop(&self) -> Result<()>;

    /// Live status — for ambient doctor and /status endpoint.
    fn status(&self) -> TransportStatus;

    /// Cursor/token persistence across daemon restarts. Default: stateless.
    fn save_state(&self) -> Result<Option<Vec<u8>>> { Ok(None) }
    fn load_state(&self, _state: &[u8]) -> Result<()> { Ok(()) }

    /// Push-triggered pull (CloudKit APNs, webhooks). Default: no-op.
    fn on_push_notification(&self, payload: Vec<u8>) -> Result<()> { Ok(()) }
}
```

### SqliteStreamProvider Schema (ambient-stream)

Storage path: `~/.ambient/stream.db`

```sql
CREATE TABLE IF NOT EXISTS event_log (
    offset      INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type  TEXT    NOT NULL,   -- "raw" | "pulse"
    transport   TEXT,               -- NULL=local, "bonjour", "cloudkit", etc.
    source_id   TEXT    NOT NULL,
    record_id   TEXT,               -- non-NULL for remote records (dedup key)
    timestamp   REAL    NOT NULL,   -- original event timestamp (temporal ordering)
    arrived_at  REAL    NOT NULL,   -- wall clock on append (lag monitoring only)
    payload     BLOB    NOT NULL    -- bincode-serialized EventLogEntry
);

-- Deduplication: same remote record arriving via multiple transports
-- is stored once. INSERT OR IGNORE on record_id.
CREATE UNIQUE INDEX IF NOT EXISTS event_log_record_id
    ON event_log (record_id)
    WHERE record_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS event_log_timestamp ON event_log (timestamp);
CREATE INDEX IF NOT EXISTS event_log_source    ON event_log (source_id, offset);

CREATE TABLE IF NOT EXISTS consumer_offsets (
    consumer_id TEXT    PRIMARY KEY,
    last_offset INTEGER NOT NULL,
    updated_at  REAL    NOT NULL
);
```

WAL mode: `PRAGMA journal_mode=WAL` on open.

Local transports call `append_raw()` / `append_pulse()` —
`transport = NULL`, `record_id = NULL`.
Remote transports call `append_raw_from()` —
`transport = "bonjour"`, `record_id = "<origin_device_id>/<seq_no>"`.

`arrived_at` is for lag monitoring and diagnostics only. It is never
used for temporal reasoning. `timestamp` (the original event time) is
used for all temporal operations.

### InMemoryStreamProvider (ambient-stream, test only)

```rust
#[cfg(test)]
pub struct InMemoryStreamProvider { /* Vec-backed, no SQLite */ }
```

Permitted only in `#[cfg(test)]` code. Never in production.

### Migration — Existing Adapters and Samplers

`SourceAdapter` and `PulseSampler` traits are retired. Every existing
implementation becomes a `StreamTransport`. Internal logic is unchanged.

| Former | New | Output call |
|---|---|---|
| ObsidianAdapter | ObsidianTransport | `append_raw(..)` |
| SpotlightAdapter | SpotlightTransport | `append_raw(..)` |
| AppleNotesAdapter | AppleNotesTransport | `append_raw(..)` |
| HealthKitAdapter | HealthKitTransport | `append_raw(..)` |
| CalendarAdapter | CalendarTransport | `append_raw(..)` |
| ContextSwitchSampler | ContextSwitchTransport | `append_pulse(..)` |
| ActiveAppSampler | ActiveAppTransport | `append_pulse(..)` |
| AudioInputSampler | AudioInputTransport | `append_pulse(..)` |
| CalendarContextSampler | CalendarContextTransport | `append_pulse(..)` |

`on_load_change(load: SystemLoad)` from the former `PulseSampler` is
preserved on behavioral transports as a direct method called by
`TransportRegistry` when `LoadBroadcaster` emits a new `SystemLoad`.

### Consumer Architecture

Consumers depend on `Arc<dyn StreamProvider>`. They have no knowledge
of which transports are active or how events arrived.

```rust
// NormalizerConsumer — reads Raw entries, ignores Pulse
pub struct NormalizerConsumer {
    provider: Arc<dyn StreamProvider>,
    store: Arc<dyn KnowledgeStore>,
    consumer_id: ConsumerId,    // "normalizer"
}

impl NormalizerConsumer {
    pub async fn run(&self, load: Arc<Mutex<SystemLoad>>) {
        let mut offset = self.provider
            .last_committed(&self.consumer_id)
            .unwrap_or(None)
            .unwrap_or(0);

        let (tx, mut rx) = tokio::sync::watch::channel(0u64);
        self.provider.subscribe(&self.consumer_id, tx).unwrap();

        loop {
            rx.changed().await.ok();

            if *load.lock().await == SystemLoad::Minimal {
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }

            let batch = self.provider
                .read(&self.consumer_id, offset, 100)
                .unwrap();

            for (next_offset, entry) in batch {
                if let EventLogEntry::Raw(event) = entry {
                    self.process_raw(event).await;
                }
                // Commit per entry — partial batch failure is resumable
                self.provider.commit(&self.consumer_id, next_offset).unwrap();
                offset = next_offset;
            }
        }
    }
}
```

`TriggerConsumer` — `consumer_id = "triggers"`, reads both Raw and Pulse.
`AuditConsumer` — `consumer_id = "audit"`, dev only (`AMBIENT_AUDIT=1`).

### TransportRegistry (ambient-cli)

Static at startup. Config-driven. Supports reload on SIGHUP.

```rust
pub struct TransportRegistry {
    transports: Vec<Box<dyn StreamTransport>>,
}

impl TransportRegistry {
    pub fn from_config(config: &Config) -> Self { .. }

    pub fn start_all(&self, provider: Arc<dyn StreamProvider>) -> Result<()> {
        for t in &self.transports {
            // Restore persisted cursor/token
            let path = state_path(t.transport_id());
            if path.exists() {
                t.load_state(&fs::read(&path)?)?;
            }
            t.start(Arc::clone(&provider))?;
        }
        Ok(())
    }

    pub fn stop_all(&self) -> Result<()> {
        for t in &self.transports {
            t.stop()?;
            if let Some(bytes) = t.save_state()? {
                fs::write(state_path(t.transport_id()), bytes)?;
            }
        }
        Ok(())
    }

    pub fn on_push_notification(&self, id: &TransportId, payload: Vec<u8>) {
        if let Some(t) = self.transports.iter()
            .find(|t| &t.transport_id() == id)
        {
            let _ = t.on_push_notification(payload);
        }
    }

    pub fn status_all(&self) -> Vec<TransportStatus> {
        self.transports.iter().map(|t| t.status()).collect()
    }

    // SIGHUP: diff config, stop removed transports, start added ones
    pub fn reload(
        &mut self,
        config: &Config,
        provider: Arc<dyn StreamProvider>,
    ) -> Result<()> { .. }
}
```

State files: `~/.ambient/transport_state/<transport_id>` (bincode).

The registry is static — the set of active transports is determined by
config at startup or reload. Transports cannot register themselves at
runtime without a config change.

### Daemon Loop (ambient-cli)

```rust
let provider: Arc<dyn StreamProvider> =
    Arc::new(SqliteStreamProvider::open(&config.stream_db_path)?);

let registry = TransportRegistry::from_config(&config);
registry.start_all(Arc::clone(&provider))?;

tokio::spawn(
    NormalizerConsumer::new(Arc::clone(&provider), store.clone())
        .run(load.clone())
);
tokio::spawn(
    TriggerConsumer::new(Arc::clone(&provider), triggers.clone())
        .run(load.clone())
);
// Adding a new consumer: one line here. No other changes.
```

### Stream Retention

```toml
[store]
stream_retention_days = 30    # WAL entries older than this are pruned daily
```

The WAL is a durable ingestion buffer and replay window. It is not a
long-term archive. tsink is the pulse archive (90-day Gorilla compression).
CozoDB `cognitive_snapshots` are not subject to WAL retention.

### Test Infrastructure

```rust
let provider = Arc::new(InMemoryStreamProvider::new());

provider.append_raw(RawEvent {
    source: "obsidian".into(),
    timestamp: "2024-01-15T09:00:00Z".parse()?,
    payload: RawPayload::Markdown {
        content: "# Systems thinking".into(),
        path: PathBuf::from("systems.md"),
    },
})?;
provider.append_pulse(PulseEvent {
    timestamp: "2024-01-15T08:58:00Z".parse()?,
    signal: PulseSignal::ContextSwitchRate { switches_per_minute: 0.3 },
})?;

NormalizerConsumer::new(Arc::clone(&provider), store.clone())
    .process_all()
    .await?;

let (_, state) = store.unit_with_context(unit_id)?;
assert!(state.was_in_flow);
```

No OS APIs. No SQLite. No network. No mocking.

### Coding Conventions

- Never use `mpsc::channel` for ingestion — use `StreamProvider::append_*`
- Remote transports always use `append_raw_from` — `transport` and
  `record_id` are required for dedup and diagnostics
- `arrived_at` is never used for temporal reasoning — `timestamp` only
- Consumers commit after each individual entry, not at batch end
- `InMemoryStreamProvider` only in `#[cfg(test)]`
- `SqliteStreamProvider` opens with WAL mode — verified in integration test
- Transport crates must not import `rusqlite` or any `ambient-stream`
  implementation type — only `ambient-core` trait definitions

---

## Amendment 9 — Transport Plugin Crates

### Decision

Every data source — local OS, network peer, cloud platform, third-party
health API — is a `StreamTransport` plugin crate. All plugins implement
one trait. All write to one `StreamProvider`. None know about each other.
`ambient-cli` is the only crate that imports multiple plugin crates.

The `StreamProvider` / `StreamTransport` separation from Amendment 8
enforces this at the crate boundary: plugin crates cannot reach into
the WAL, and the WAL implementation cannot import plugin dependencies.

Adding a new health platform (e.g. Google Health, Garmin, Oura) requires
writing one crate implementing one trait. No other crate changes.

### Transport Plugin Taxonomy

```
ambient-transport-local/        Local OS sources and behavioral samplers
  ObsidianTransport             FSEvents vault watcher
  SpotlightTransport            NSMetadataQuery (opt-in)
  AppleNotesTransport           SQLite polling (opt-in)
  HealthKitTransport            HKObserverQuery — Apple only (opt-in)
  CalendarTransport             EventKit — Apple only (opt-in)
  ContextSwitchTransport        NSWorkspace + timer
  ActiveAppTransport            AXUIElement (opt-in, battery cost noted)
  AudioInputTransport           AVAudioEngine KVO
  CalendarContextTransport      EventKit 60s sampler (requires calendar)
  Cargo deps: ambient-core, objc2, objc2-app-kit, notify

ambient-transport-bonjour/      LAN peer discovery and data transfer
  BonjourTransport
  Cargo deps: ambient-core, mdns-sd, tokio, bincode

ambient-transport-cloudkit/     Apple iCloud sync
  CloudKitTransport
  Cargo deps: ambient-core, objc2, objc2-cloud-kit, bincode

ambient-transport-google-health/   Third-party health API (Phase 13+)
  GoogleHealthTransport
  Cargo deps: ambient-core, reqwest, oauth2
```

No plugin crate imports another plugin crate.
No plugin crate imports `rusqlite` or any `ambient-stream` type.

### BonjourTransport (ambient-transport-bonjour)

Bonjour provides peer discovery. TCP provides data transfer. The transport
manages a dynamic internal peer set — the `TransportRegistry` sees one
static `BonjourTransport`. The transport internally manages N peers.

**mDNS service:**
```
Type:       "_ambient._tcp.local."
TXT record: { device_id: "<sha256 of MAC address>", version: "1" }
Port:       17474 (configurable)
```

`BonjourTransport` both advertises (so peers can find this daemon) and
browses (so it can find `ambient-recorder` on other devices).

**Peer wire protocol (per TCP connection):**

```
Client → Server:  HELLO { consumer_id, last_seen: HashMap<DeviceId, SeqNo> }
Server → Client:  STREAM of frames:
                    Frame { seq_no: u64, record_id: String, entry: EventLogEntry }
Client → Server:  ACK { seq_no } after each successful append_raw_from()
```

`last_seen` in HELLO is the per-device sequence numbers the client has
already processed. The server resumes from the last acknowledged sequence
number. This is how the transport avoids re-delivering records already
in the local WAL without requiring global offset coordination.

**Peer membership is dynamic and internal.** Peers join via mDNS
announcements and leave via withdrawal or TCP disconnect. The daemon
is not involved — `BonjourTransport` manages the peer set entirely.
`TransportStatus.peers` reflects the live peer set at `status()` call
time. `ambient doctor` shows which Bonjour peers are reachable.

**Deduplication.** The same `BehavioralSummary` record from
`ambient-recorder` may arrive via both Bonjour (when daemon restarts
and discovers recorder on loopback) and CloudKit (if cloudkit is also
enabled). The WAL `UNIQUE INDEX` on `record_id` ensures it is stored
once regardless of which transport delivered it first.

### CloudKitTransport (ambient-transport-cloudkit)

Implements `StreamTransport`. Writes to `StreamProvider` via
`append_raw_from(event, "cloudkit", &record.id)`.

```
ambient-transport-cloudkit/src/
  transport.rs      CloudKitTransport — start, stop, status, save/load state
  record_types.rs   CloudKit record type constants (no string literals in code)
  token.rs          CKServerChangeToken — bincode serialization, version guard
  normalizer.rs     CloudKit record fields → RawEvent (before WAL write)
```

`start()`: registers `CKDatabaseSubscription` for private database zone.
`on_push_notification()`: triggers `CKFetchRecordZoneChangesOperation`
with stored token. For each fetched record: normalize → `append_raw_from`.
`save_state()` / `load_state()`: `CKServerChangeToken` via bincode.
On deserialization failure: log warning, reset to full re-fetch.

`timestamp = record.modification_date` — original temporal position.
Late-arriving records land at their correct position in the stream.

**CloudKit record types and their WAL mapping:**

| CloudKit RecordType | RawPayload variant | Downstream |
|---|---|---|
| BehavioralSummary | RawPayload::BehavioralSummary | NormalizerConsumer → tsink |
| PhysiologicalSample | RawPayload::HealthKitSample | NormalizerConsumer → CozoDB |
| KnowledgeUnitSync | RawPayload::KnowledgeUnitSync | NormalizerConsumer → CozoDB direct upsert |
| FeedbackEventSync | RawPayload::FeedbackEventSync | NormalizerConsumer → CozoDB feedback_events |

**New RawPayload variants** (add to ambient-core):

```rust
pub enum RawPayload {
    // Existing variants (Amendments 1 + 2) unchanged

    // Remote transport variants — produced by BonjourTransport and CloudKitTransport
    BehavioralSummary {
        period_start: DateTime<Utc>,
        period_end: DateTime<Utc>,
        avg_context_switch_rate: f32,
        peak_context_switch_rate: f32,
        audio_active_seconds: u32,
        dominant_app: Option<String>,
        app_switch_count: u32,
        was_in_meeting: bool,
        was_in_focus_block: bool,
        device_origin: String,          // sha256 of device identifier
    },
    KnowledgeUnitSync {
        unit: KnowledgeUnit,            // normalized on origin device
        device_origin: String,
    },
    FeedbackEventSync {
        event: FeedbackEvent,           // typed on origin device
        device_origin: String,
    },
}
```

`RawPayload::HealthKitSample` (from Amendment 2) is reused for
`PhysiologicalSample` records from both CloudKit and Google Health.
One normalizer handles all three physiological sources.

**APNs wiring in ambient-cli:**

```rust
if config.transports.cloudkit {
    NSApplication::sharedApplication()
        .registerForRemoteNotificationTypes(
            NSRemoteNotificationType::NSRemoteNotificationTypeAlert
        );
    // On notification receipt:
    // registry.on_push_notification("cloudkit", payload)
}
```

**Paid tier:** `KnowledgeUnitSync` and `FeedbackEventSync` are paid tier.
`BehavioralSummary` and `PhysiologicalSample` sync are free tier.

### GoogleHealthTransport (ambient-transport-google-health)

Phase 13+. Stub crate created at Phase 1 scaffold. No implementation
until Phase 13.

Maps Google Health Connect metric names → `HealthMetric` enum variants.
Emits `RawPayload::HealthKitSample` — identical type to native HealthKit.
Uses `save_state()` / `load_state()` for OAuth token + sync cursor.
OAuth credentials stored in macOS Keychain (never in config files).

No code beyond the empty crate stub is written before Phase 13.

### ambient-recorder Binary

Separate lightweight LaunchAgent binary. Behavioral signal recorder for
when the main daemon is inactive.

```
ambient-recorder/src/
  main.rs       LaunchAgent entry point, 60s tick loop
  sampler.rs    NSWorkspace sampling (synchronous, no async)
  coalescer.rs  60-second rolling window → BehavioralSummary
  writer.rs     write via loopback TCP (Bonjour port) or buffer file
  ipc.rs        PID file check — skip cycle if main daemon is active
```

**IPC:** checks `~/.ambient/daemon.pid`. If PID alive: skip — main
daemon records to tsink directly. If absent: sample and write.

**Write path (priority order):**
1. Loopback TCP to Bonjour port (17474) — main daemon's `BonjourTransport`
   receives records on restart
2. Buffer file `~/.ambient/recorder-buffer.bin` if daemon is absent and
   no TCP connection possible — `NormalizerConsumer` reads buffer on
   next daemon startup

`ambient-recorder` does NOT use `BonjourTransport` (no mDNS) — it
connects directly to the well-known port on loopback. mDNS is only
needed to discover peers on the LAN; for loopback the port is known.

**Cargo deps:** `ambient-core`, `objc2`, `objc2-app-kit`.
Does NOT depend on: `ambient-stream`, `ambient-store`,
`ambient-transport-*`, `cozo`, `rig`.

**LaunchAgent plist:** `~/Library/LaunchAgents/dev.ambient.recorder.plist`

### Transport Configuration

```toml
[transports]
# Local — always active
obsidian = true

# Local OS — opt-in
spotlight = false
apple_notes = false
healthkit = false           # requires entitlement
calendar = false            # requires EventKit permission

# Behavioral samplers
context_switch = true
active_app = false          # opt-in — AXUIElement battery cost
audio_input = true
calendar_context = false    # requires calendar = true

# Network peers
bonjour = true              # LAN sync — recommended default on

# Cloud transports
cloudkit = false            # requires Apple Developer account
google_health = false       # requires OAuth, Android/Wear OS device

[bonjour]
service_name = "Ambient"
port = 17474

[cloudkit]
container = "iCloud.dev.ambient.private"
zone_name  = "AmbientZone"

[google_health]
# client_id and client_secret stored in macOS Keychain after first auth
# config file never contains credentials
```

### ambient doctor — Transport Status

```
$ ambient doctor

Transport Status
────────────────────────────────────────────────────────────
✓ obsidian             Active — 3,847 notes, last event 2m ago
✓ context_switch       Active — sampling every 10s
✓ audio_input          Active — last event 8s ago
✓ bonjour              Active — 2 peers
    📱 Sushant's iPhone    reachable, 847 events received, last seen 30s
    💻 MacBook Air         reachable, 23 events received, last seen 5m
✗ cloudkit             Inactive — disabled in config
✗ google_health        Inactive — disabled in config
⚠ healthkit            Requires setup — enable in config + grant HealthKit
⚠ active_app           Inactive — enable in config (battery cost: medium)

ambient-recorder       Active (PID 48201) — last write 52s ago
                       Buffer: empty (daemon active)
```

---

## Revised Crate and Binary Structure (Supersedes Amendment 5)

15 workspace members: 13 library crates + 2 binaries.

```
# Shared types and traits — no implementations, no external deps
ambient-core/
  stream.rs        StreamProvider trait, Offset, ConsumerId, EventLogEntry
  transport.rs     StreamTransport trait, TransportId, TransportStatus, PeerStatus
  event.rs         RawEvent, PulseEvent, RawPayload, PulseSignal
  knowledge.rs     KnowledgeUnit, KnowledgeStore trait, CognitiveState
  query.rs         QueryRequest, QueryResult, FeedbackEvent
  capability.rs    CapabilityStatus, GatedCapability, SetupAction

# Stream provider — implements StreamProvider
ambient-stream/
  sqlite_stream.rs  SqliteStreamProvider (production)
  memory_stream.rs  InMemoryStreamProvider (cfg(test) only)
  consumer.rs       ConsumerHandle, auto-commit helper
  retention.rs      daily WAL pruning task

# Transport plugins — each implements StreamTransport, writes to StreamProvider
ambient-transport-local/
  All local OS transports and behavioral samplers
  Deps: ambient-core, objc2, objc2-app-kit, notify

ambient-transport-bonjour/
  BonjourTransport
  Deps: ambient-core, mdns-sd, tokio, bincode

ambient-transport-cloudkit/
  CloudKitTransport
  Deps: ambient-core, objc2, objc2-cloud-kit, bincode

ambient-transport-google-health/   (stub only until Phase 13)
  GoogleHealthTransport
  Deps: ambient-core, reqwest, oauth2

# Intelligence and storage — unchanged from CLAUDE.md-1 Amendment 5
ambient-normalizer/   All normalizers + NormalizerConsumer
ambient-store/        CozoDB + tsink + cognitive_snapshots
ambient-spotlight/    NSMetadataQuery + CSSearchableIndex
ambient-reasoning/    Rig + ollama + EmbeddingQueue
ambient-patterns/     PatternDetector — pure Datalog, no tsink dep
ambient-triggers/     TriggerConsumer + QS TriggerCondition variants
ambient-query/        QueryEngine + feedback reranking + CapabilityGate
ambient-menubar/      Overlay + cognitive badges + feedback recording
ambient-onboard/      SetupWizard + CapabilityGate + UnlockNotifier

# Entry point — the only crate that imports multiple transport crates
ambient-cli/
  Deps: all ambient-* crates + all ambient-transport-* crates
```

Binaries:
```
ambient-cli/      Main daemon binary (also the ambient-cli library crate entry)
ambient-recorder/ Standalone LaunchAgent binary
  Deps: ambient-core, objc2, objc2-app-kit
  NOT: ambient-stream, ambient-store, ambient-transport-*, cozo, rig
```

### Dependency Rules

```
ambient-core          no workspace dependencies
ambient-stream        depends on: ambient-core
ambient-transport-*   depends on: ambient-core only (not ambient-stream)
ambient-normalizer    depends on: ambient-core, ambient-stream
ambient-store         depends on: ambient-core
ambient-patterns      depends on: ambient-core, ambient-store (no tsink import)
ambient-triggers      depends on: ambient-core, ambient-stream
ambient-query         depends on: ambient-core, ambient-store, ambient-stream
ambient-onboard       depends on: ambient-core, ambient-store, ambient-query
ambient-cli           depends on: all of the above
```

No transport crate imports another transport crate.
No transport crate imports `rusqlite` or `ambient-stream`.
`ambient-cli` is the integration point — the only crate with visibility
into all transport crates simultaneously.

---

## Revised Build Phase Order (Supersedes Amendment 6)

```
Phase 1 — Spine
  Scaffold all 15 workspace members + ambient-recorder binary
  Stub crates for ambient-transport-google-health and ambient-recorder
  ambient-core types including StreamProvider + StreamTransport traits
  SqliteStreamProvider + InMemoryStreamProvider
  ObsidianTransport → MarkdownNormalizer → NormalizerConsumer → CozoDB
  TransportRegistry wired in ambient-cli
  CLI: ambient watch
  Gate: cargo test --workspace
        stream round-trip: append_raw → read → commit → resume from offset
        dedup: append_raw_from same record_id twice → stored once

Phase 2 — Pulse
  ContextSwitchTransport, ActiveAppTransport, AudioInputTransport
  tsink integration, derive_cognitive_state, CognitiveState
  cognitive_snapshots materialized on upsert (Amendment 7)
  CLI: ambient status
  Gate: battery test < 200mW average over 4 hours on battery

Phase 3 — Intelligence
  Rig + ollama, EmbeddingQueue, CozoDB HNSW vector index
  QueryEngine, CLI: ambient query, HTTP /query
  Gate: semantic search + graceful FTS degradation

Phase 4 — Onboarding
  ambient-onboard, SetupWizard, CapabilityGate
  ambient setup, ambient doctor (transport status included)
  Progressive unlock notifications
  Gate: end-to-end setup < 2 minutes on clean machine

Phase 5 — Feedback Loop
  FeedbackEvent, record_feedback, feedback_score, QueryEngine reranking
  Gate: reranking changes result order after 20 explicit signals

Phase 6 — Pattern and Trigger
  PatternDetector pure Datalog, TriggerConsumer as stream consumer
  macOS notifications, weekly HTML pattern report
  Gate: pattern fires → notification delivered

Phase 7 — Quantified Self
  HealthKitTransport, CalendarTransport, CalendarContextTransport
  SelfReportCollector, QS CognitiveState fields, QS patterns and triggers
  Gate: unit_with_context returns HRV and sleep fields

Phase 8 — Spotlight
Phase 9 — Menu Bar

Phase 10 — Bonjour + ambient-recorder
  BonjourTransport — mDNS discovery, TCP peer protocol, dedup via record_id
  ambient-recorder — LaunchAgent, PID file IPC, 60s coalescer, loopback write
  ambient doctor shows Bonjour peer status
  Gate: ambient-recorder writes while daemon stopped → daemon restarts →
        BonjourTransport ingests backfill → NormalizerConsumer processes
        in timestamp order → cognitive_snapshots reflect backfilled data

Phase 11 — CloudKit Transport
  CloudKitTransport — APNs push, CKFetchRecordZoneChangesOperation
  CKServerChangeToken persistence via save_state/load_state
  KnowledgeUnitSync and FeedbackEventSync (paid tier)
  ambient-recorder falls back to CloudKit write when loopback unavailable
  Gate: BehavioralSummary arrives via CloudKit → WAL → cognitive_snapshots
        updated correctly with original timestamp ordering

Phase 12 — Hard Sources (Apple Notes)
Phase 13 — Google Health Transport
  GoogleHealthTransport — OAuth, REST polling, Health Connect change feed
  Maps Google Health metrics → HealthMetric → RawPayload::HealthKitSample
  Gate: HRV from Google Health appears in cognitive_snapshots

Phase 14 — Distribution
```

---

## What Does NOT Change from CLAUDE.md or CLAUDE.md-1

- tsink storage config, metric names, retention policy — unchanged
- Rig integration and ReasoningBackend — unchanged
- SystemLoad enum and LoadBroadcaster — unchanged
- SpotlightExporter — unchanged
- LicenseGate and paid tier features from CLAUDE.md — unchanged
- Battery gate requirement (< 200mW) — unchanged
- Privacy rules for ActiveApp — unchanged
- All objc2 usage conventions — unchanged
- CozoDB schema from Amendment 1 (cognitive_snapshots added here)
- All QS types from Amendment 2 — unchanged
- FeedbackEvent and feedback loop from Amendment 3 — unchanged
- Onboarding crate and CapabilityGate from Amendment 4 — unchanged
- All coding conventions in CLAUDE.md and CLAUDE.md-1 remain in force
