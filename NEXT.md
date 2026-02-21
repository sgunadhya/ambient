# CLAUDE.md-1 — Amendment Layer

This document amends CLAUDE.md. Where this document conflicts with CLAUDE.md,
this document takes precedence. All other CLAUDE.md decisions remain in force.

Read CLAUDE.md first, then read this document in full before writing any code.

---

## Amendment 1 — CozoDB Replaces LadybugDB

### Decision

LadybugDB is replaced by CozoDB as the knowledge graph store.
tsink is unchanged — it remains the pulse telemetry store.
The dual-store architecture is preserved but for the right reason:
CozoDB handles graph + vector + temporal knowledge.
tsink handles high-frequency behavioral telemetry.
These are genuinely different data shapes requiring different storage engines.

### Rationale

CozoDB provides three capabilities that LadybugDB cannot:

1. **Native time travel** — the knowledge graph is versioned at the
   transaction level. Queries like "how has my thinking about X evolved
   over time?" are native Datalog against a single store, not application
   code joining multiple query results.

2. **Vector search inside Datalog** — HNSW indices integrate seamlessly
   with recursive graph traversal in a single query. The semantic +
   structural search that currently requires two separate query paths
   collapses into one composable Datalog statement.

3. **Datalog query language** — the PatternDetector becomes composable
   Datalog rules rather than application code. Each pattern is a rule.
   Rules compose. The query planner optimizes across them. The
   unit_with_context query becomes a single statement rather than
   multiple Cypher queries stitched together in Rust.

CozoDB is embedded, written in Rust, runs in-process, requires no setup.
The KnowledgeStore trait boundary in ambient-core is unchanged — this is
an ambient-store implementation detail invisible to all other crates.

### Crate Changes

ambient-store/Cargo.toml:
  Remove: ladybug (or equivalent LadybugDB crate)
  Add:    cozo (CozoDB Rust crate, RocksDB backend for persistence)

No other crate changes. ambient-core, ambient-query, ambient-cli,
ambient-menubar all depend on the KnowledgeStore trait only.

### Schema (Datalog Relations)

Replace the Cypher DDL in CLAUDE.md with the following CozoDB schema:

```datalog
# Core knowledge relation — Validity type enables native time travel
:create notes {
    id: String                  =>
    source: String,
    title: String,
    content: String,
    hash: String,
    embedding: <F32; 768>?,
    observed_at: Float
}

# Directional links between notes (from wikilinks, explicit refs)
:create links {
    from_id: String,
    to_id: String               =>
    link_type: String
}

# Concept nodes (extracted topics, tags, named entities)
:create concepts {
    id: String                  =>
    label: String
}

# Note → Concept membership
:create concept_mentions {
    note_id: String,
    concept_id: String          =>
}

# Pattern insights from PatternDetector (stored back into graph)
:create pattern_results {
    id: String                  =>
    pattern_type: String,
    summary: String,
    unit_ids: [String],
    detected_at: Float,
    feedback: String?           # "useful" | "noise" | null
}

# Feedback events (see Amendment 3)
:create feedback_events {
    id: String                  =>
    timestamp: Float,
    signal_type: String,
    unit_id: String?,
    query_text: String?,
    action: String?,
    pattern_id: String?
}
```

### HNSW Vector Index

Create after schema initialization:

```datalog
::hnsw create notes:embedding_idx {
    dim: 768,
    dtype: F32,
    fields: [embedding],
    distance: Cosine,
    ef_construction: 200,
    m: 16
}
```

### Key Queries in Datalog

**upsert (idempotent on hash):**
```datalog
?[id, source, title, content, hash, embedding, observed_at] <-
    [[$id, $source, $title, $content, $hash, $embedding, $observed_at]]
:put notes { id => source, title, content, hash, embedding, observed_at }
```

**search_fulltext:**
```datalog
?[id, title, content, score] :=
    ~notes:fts{ id, title, content | query: $query, k: $k, score_field: score }
:order -score
:limit $k
```

**search_semantic:**
```datalog
?[id, title, content, dist] :=
    ~notes:embedding_idx{ id | query: $vec, k: $k, ef: 50, bind_distance: dist },
    *notes{ id, title, content }
:order dist
:limit $k
```

**related (graph traversal via links):**
```datalog
# depth-2 traversal
connected[a, b] := *links{ from_id: a, to_id: b }
connected[a, b] := connected[a, mid], *links{ from_id: mid, to_id: b }

?[id, title] :=
    connected[$root_id, id],
    *notes{ id, title }
:limit $limit
```

**temporal evolution query (native time travel):**
```datalog
# How has content about a concept evolved over time
?[id, title, observed_at] :=
    *notes{ id, title, observed_at },
    *concept_mentions{ note_id: id, concept_id: $concept_id }
:order observed_at
```

### unit_with_context Query

The graph side becomes a single Datalog query.
The pulse side remains a tsink query. Application code derives CognitiveState.

```
CozoDB Datalog:
    fetch note by id
    fetch related notes via links traversal
    fetch concept_mentions for note
    → KnowledgeUnit + graph context

tsink:
    pulse_window(observed_at - window, observed_at + window)
    → Vec<PulseEvent>

Application (ambient-store):
    derive CognitiveState from Vec<PulseEvent>
    return (KnowledgeUnit, Vec<PulseEvent>, CognitiveState)
```

### FTS Index

CozoDB v0.7 includes full-text search. Create FTS index after schema:

```datalog
::fts create notes:fts {
    fields: [title, content],
    filters: {language: English}
}
```

### Storage Path

Replace ~/.ambient/graph (LadybugDB) with ~/.ambient/cozo (CozoDB RocksDB).

---

## Amendment 2 — Quantified Self Layer

### Decision

Ambient expands from a knowledge management tool to a personal cognitive
intelligence platform by adding physiological and calendar signals.
This is additive — no existing crossing point types are modified.

### New RawPayload Variants

Add to the RawPayload enum in ambient-core:

```rust
pub enum RawPayload {
    // Existing variants unchanged
    Markdown { content: String, path: PathBuf },
    AppleNote { protobuf_blob: Bytes, note_id: String, modified_at: DateTime<Utc> },
    SpotlightItem { text_content: String, content_type: String,
                    display_name: String, file_url: String,
                    last_modified: DateTime<Utc> },
    PlainText { content: String, path: PathBuf },

    // New QS variants
    HealthKitSample {
        metric: HealthMetric,
        value: f64,
        unit: String,
        recorded_at: DateTime<Utc>,
    },
    CalendarEvent {
        title: String,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        duration_minutes: u32,
        attendee_count: u32,       // count only, never names
        is_focus_block: bool,
        calendar_name: String,
    },
    SelfReport {
        energy: u8,                // 1-5
        mood: u8,                  // 1-5
        note: Option<String>,
        reported_at: DateTime<Utc>,
    },
}

pub enum HealthMetric {
    HRV,
    RestingHeartRate,
    SleepDuration,
    SleepQuality,           // 0.0-1.0 score derived from Apple Health
    StepCount,
    ActiveCalories,
    RespiratoryRate,
}
```

### New PulseSignal Variants

Add to the PulseSignal enum in ambient-core:

```rust
pub enum PulseSignal {
    // Existing variants unchanged
    ContextSwitchRate { switches_per_minute: f32 },
    ActiveApp { bundle_id: String, window_title: Option<String> },
    AudioInputActive { active: bool },
    TimeContext { hour_of_day: u8, day_of_week: u8, is_weekend: bool },

    // New QS variants
    CalendarContext {
        in_meeting: bool,
        in_focus_block: bool,
        minutes_until_next_event: Option<u32>,
        current_event_duration_minutes: Option<u32>,
    },
    EnergyLevel { score: u8 },   // 1-5, from SelfReport
    MoodLevel { score: u8 },     // 1-5, from SelfReport
    HRVScore { value: f32 },     // from HealthKit, sampled at start of day
    SleepQuality { score: f32 }, // 0.0-1.0, from HealthKit, sampled at start of day
}
```

### New Source Adapters (ambient-watcher)

**HealthKitAdapter:**
- macOS HealthKit API via Swift FFI or objc2
- Entitlement: com.apple.developer.healthkit (read-only)
- Query on startup: fetch last 90 days of HRV, resting HR, sleep
- Subscribe to HKObserverQuery for ongoing updates
- Emit RawPayload::HealthKitSample on each new sample
- Privacy: never store raw HealthKit data — normalize to
  RawPayload::HealthKitSample immediately, discard raw record
- Gated: config [sources] healthkit = false by default
  Request entitlement only when user explicitly enables

**CalendarAdapter:**
- macOS EventKit API via objc2
- Entitlement: com.apple.security.personal-information.calendars
- On startup: fetch events for next 7 days + past 30 days
- EKEventStore change notification for ongoing updates
- Emit RawPayload::CalendarEvent
- Privacy: attendee_count only, never attendee names or emails
- Detect focus blocks: events matching configurable title patterns
  e.g. ["Deep Work", "Focus", "No Meetings", "Blocked"]
  configurable in ~/.ambient/config.toml

**CalendarContextSampler (PulseSampler, not SourceAdapter):**
- 60s tick
- Query EKEventStore for current and upcoming events
- Emit PulseSignal::CalendarContext
- LoadAware: continue on Conservative, pause on Minimal

**SelfReportCollector:**
- Not a sampler — receives SelfReport via CLI or HTTP API
- ambient checkin command: prompts energy (1-5) and mood (1-5)
- POST /checkin endpoint for OpenClaw or other external triggers
- Emits both RawPayload::SelfReport and PulseSignal::EnergyLevel +
  PulseSignal::MoodLevel
- Reminder: optional cron in ~/.ambient/config.toml
  e.g. checkin_reminder = "09:00"  (fires macOS notification)

### New Normalizers (ambient-normalizer)

**HealthKitNormalizer:**
- can_handle: RawPayload::HealthKitSample
- title: "{metric} reading — {recorded_at date}"
- content: human-readable description e.g. "HRV: 52ms"
- metadata["metric"], metadata["value"], metadata["unit"]
- content_hash: blake3 of metric + recorded_at (deduplicate resync)
- Stored in CozoDB as a note — queryable alongside knowledge units

**CalendarNormalizer:**
- can_handle: RawPayload::CalendarEvent
- title: event title
- content: structured summary (duration, attendee count, focus block flag)
- metadata["is_focus_block"], metadata["duration_minutes"],
  metadata["attendee_count"], metadata["calendar_name"]
- content_hash: blake3 of title + start timestamp

**SelfReportNormalizer:**
- can_handle: RawPayload::SelfReport
- title: "Check-in — {date}"
- content: "Energy: {energy}/5, Mood: {mood}/5. {note}"
- metadata["energy"], metadata["mood"]

### CognitiveState Extension

Add fields to CognitiveState in ambient-core:

```rust
pub struct CognitiveState {
    // Existing fields unchanged
    pub was_in_flow: bool,
    pub was_on_call: bool,
    pub dominant_app: Option<String>,
    pub time_of_day: Option<u8>,

    // New QS fields — None if data not available
    pub was_in_meeting: bool,
    pub was_in_focus_block: bool,
    pub energy_level: Option<u8>,
    pub mood_level: Option<u8>,
    pub hrv_score: Option<f32>,
    pub sleep_quality: Option<f32>,
    pub minutes_since_last_meeting: Option<u32>,
}
```

### New Config Keys

```toml
[sources]
healthkit = false         # opt-in, requires entitlement
calendar = false          # opt-in, requires EventKit permission
self_reports = true       # enabled by default, no permission needed

[calendar]
focus_block_patterns = ["Deep Work", "Focus", "No Meetings", "Blocked"]

[checkin]
reminder_time = ""        # empty = no reminder, e.g. "09:00" to enable
```

### New tsink Metric Names

```
calendar_context         labels: { in_meeting, in_focus_block,
                                   minutes_until_next }
energy_level             labels: {}
mood_level               labels: {}
hrv_score                labels: {}
sleep_quality            labels: {}
```

### New Prescriptive Trigger Conditions

Add to TriggerCondition enum in ambient-core:

```rust
pub enum TriggerCondition {
    // Existing conditions unchanged
    NewNoteFromSource { source_id: SourceId },
    ContentMatches { pattern: String },
    CognitiveStateIs { state: CognitiveStateFilter },
    ConceptMentioned { concept: String },

    // New QS conditions
    OptimalWindowDetected,   // HRV in top quartile + focus block + no meeting soon
    PostMeetingRecovery {    // N minutes after meeting ends
        minutes_after: u32
    },
    KnowledgeGapDetected {   // frequently retrieved note is stale
        staleness_days: u32,
        retrieval_count_threshold: u32,
    },
    EnergyThreshold {
        above: Option<u8>,
        below: Option<u8>,
    },
}
```

### New Monetization Gate

Add to paid tier:

```
Health Correlations:
  - HealthKit integration (HRV, sleep, heart rate)
  - Calendar integration (focus blocks, meeting load)
  - Optimal window detection triggers
  - QS pattern reports (sleep vs cognitive output, meeting load vs note quality)
```

---

## Amendment 3 — Feedback Loop

### Decision

Add a feedback loop so the system learns which surfaced knowledge was
actually useful. FeedbackEvent is stored in CozoDB (not tsink) as it is
structured event data with relational queries rather than telemetry.

### New Types in ambient-core

```rust
pub struct FeedbackEvent {
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub signal: FeedbackSignal,
}

pub enum FeedbackSignal {
    QueryResultActedOn {
        query_text: String,
        unit_id: Uuid,
        action: ResultAction,
        ms_to_action: u32,      // latency between result shown and action taken
    },
    QueryResultDismissed {
        query_text: String,
        unit_id: Uuid,
    },
    TriggerAcknowledged {
        trigger_id: String,
        unit_id: Uuid,
    },
    TriggerDismissed {
        trigger_id: String,
        unit_id: Uuid,
    },
    PatternMarkedUseful { pattern_id: Uuid },
    PatternMarkedNoise  { pattern_id: Uuid },
}

pub enum ResultAction {
    OpenedSource,
    OpenedDetail,
    CopiedContent,
}
```

### Storage

FeedbackEvent is stored in the CozoDB feedback_events relation (schema
defined in Amendment 1). Not tsink — feedback is structured relational
data with cross-unit aggregation queries, not a telemetry stream.

### KnowledgeStore Trait Extension

Add to KnowledgeStore trait in ambient-core:

```rust
fn record_feedback(&self, event: FeedbackEvent) -> Result<()>;
fn feedback_score(&self, unit_id: Uuid) -> Result<f32>;
    // returns click-through rate: acted_on / (acted_on + dismissed)
    // returns 0.5 (neutral) when no feedback history exists
fn pattern_feedback(&self, pattern_id: Uuid) -> Result<Option<String>>;
    // returns "useful" | "noise" | None
```

### QueryResult Extension

Add to QueryResult in ambient-core:

```rust
pub struct QueryResult {
    // Existing fields unchanged
    pub unit: KnowledgeUnit,
    pub score: f32,
    pub pulse_context: Option<Vec<PulseEvent>>,
    pub cognitive_state: Option<CognitiveState>,

    // New feedback field
    pub historical_feedback_score: f32,
    // 0.0-1.0: 0.5 = no history, >0.5 = historically useful,
    // <0.5 = historically dismissed
}
```

### QueryEngine Reranking

AmbientQueryEngine reranks results using a combined score:

```rust
let reranked_score = (semantic_score * 0.7) + (feedback_score * 0.3);
```

This weight (0.7 / 0.3) is configurable in ~/.ambient/config.toml:

```toml
[query]
semantic_weight  = 0.7
feedback_weight  = 0.3
```

### Feedback Recording in ambient-menubar

After displaying results, ambient-menubar records:
- User opens source file → FeedbackSignal::QueryResultActedOn
- User presses Esc without acting → FeedbackSignal::QueryResultDismissed
  (only after results were shown for > 2 seconds to avoid accidental dismissal)
- Record ms_to_action for acted-on results (latency is a quality signal)

### Feedback Recording in ambient-cli

POST /query response includes result UUIDs.
Client can POST /feedback with { unit_id, action } to record feedback.
ambient query --feedback flag: interactive mode that asks "useful? [y/N]"
after showing results.

### Pattern Feedback in Weekly Report

Each pattern in the HTML weekly report includes:
- [✓ Useful] and [✗ Noise] buttons
- Click writes FeedbackSignal::PatternMarkedUseful or PatternMarkedNoise
  to CozoDB via the HTTP API
- PatternDetector reads pattern_feedback before re-running patterns —
  patterns consistently marked noise are suppressed

### Implicit Behavioral Feedback

ambient-watcher observes post-query behavior via existing signals:
- If a query is followed within 60s by a file open in ActiveAppSampler
  matching a returned result's source path → implicit positive signal
  (weight: 0.5 of explicit action, stored as QueryResultActedOn with
  action: OpenedSource and a synthetic flag)
- If a query is followed within 5s by a new ContextSwitchRate spike
  → implicit negative signal (user switched away immediately)
  (weight: 0.3 of explicit dismiss, stored as QueryResultDismissed
  with a synthetic flag)

These implicit signals compound the explicit feedback without requiring
user action. The synthetic flag distinguishes them from explicit signals
in analytics queries.

---

## Amendment 4 — Onboarding Crate

### Decision

Add ambient-onboard as an 11th crate. Onboarding is a first-class
architectural concern, not a startup script in ambient-cli.

### New Crate: ambient-onboard

Dependencies: ambient-core, ambient-store, ambient-query, clap, tokio

```
ambient-onboard/
  src/
    lib.rs
    setup_wizard.rs       # terminal wizard, step verification
    capability_gate.rs    # tracks which capabilities are ready
    unlock_notifier.rs    # in-app notifications for capability unlocks
    guided_query.rs       # first-query templates, success-biased search
    health_check.rs       # ongoing system health monitoring
    first_run.rs          # first launch detection, progressive unlock
```

### New Types in ambient-core

```rust
pub enum CapabilityStatus {
    Ready,
    Pending {
        reason: String,
        estimated_eta: Option<Duration>,
    },
    RequiresSetup { action: SetupAction },
    RequiresPermission { permission: MacOSPermission },
    RequiresDependency { dependency: ExternalDependency },
}

pub enum SetupAction {
    ConfigureVaultPath,
    InstallOllama,
    GrantAccessibility,
    GrantMicrophone,
    GrantHealthKit,
    GrantCalendar,
}

pub enum MacOSPermission {
    Accessibility,
    Microphone,
    HealthKit,
    Calendar,
}

pub enum ExternalDependency {
    Ollama { install_command: String },
}

pub enum GatedCapability {
    SemanticSearch,
    CognitiveBadges,
    PatternReports,
    HealthCorrelations,
    CalendarContext,
    MenuBarOverlay,         // License gate (paid)
}
```

### CapabilityGate Trait

Add to ambient-core:

```rust
pub trait CapabilityGate: Send + Sync {
    fn status(&self, capability: GatedCapability) -> CapabilityStatus;
    fn mark_ready(&self, capability: GatedCapability);
}
```

Every feature that has an onboarding dependency checks its gate before
executing. On CapabilityStatus::Pending or RequiresSetup, return a
structured response rather than empty results or a silent failure.

Example — QueryEngine when semantic search is pending:

```rust
match self.gate.status(GatedCapability::SemanticSearch) {
    CapabilityStatus::Ready => { /* semantic search */ },
    CapabilityStatus::Pending { reason, estimated_eta } => {
        // fall back to FTS, annotate response
        result.capability_note = Some(format!(
            "Semantic search is being prepared ({reason}). \
             Showing full-text results. ETA: {eta}",
            eta = estimated_eta.map(|d| format!("{:.0}min", d.as_secs_f32() / 60.0))
                               .unwrap_or("unknown".into())
        ));
    },
    CapabilityStatus::RequiresDependency { dependency } => {
        // FTS fallback + explicit prompt
    },
    _ => { /* FTS fallback */ }
}
```

### Setup Wizard (ambient setup command)

```
$ ambient setup

🌱 Ambient — Personal Cognitive Intelligence

Step 1/3: Knowledge Sources
  Scanning for Obsidian vaults...
  ✓ Found: ~/Documents/Obsidian (3,847 notes)
  Use this vault? [Y/n]

  Apple Notes: enable? [y/N]
  → Skipped. Enable later: [sources] apple_notes = true

Step 2/3: Local Search
  Indexing vault... ████████████████ done (23s, 3,847 notes)
  ✓ Full-text search ready

  Semantic search requires ollama.
  Install ollama for richer results? [y/N]
  → Skipped. Run 'brew install ollama && ollama pull nomic-embed-text'
    then 'ambient setup --semantic' to enable.

Step 3/3: Starting Ambient
  ✓ Daemon started (PID 48291)
  ✓ Menu bar icon added 🌱

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  Ready. Press Cmd+Shift+Space to search.
  Try: 'ambient query "my best ideas"'
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

Rules:
- Only one required input: vault path (auto-detected if possible)
- All permissions requested lazily and contextually, never upfront
- ollama is optional at setup time — system works in FTS-only mode
- Every step verifies success before proceeding
- Graceful handling of every failure mode with clear remediation text
- Completes in under 2 minutes on the happy path

### Progressive Capability Unlock

After setup completes, capabilities unlock in this sequence:

```
Immediate:   Full-text search (always available after indexing)
~5 minutes:  Semantic search (after ollama pulls model + embeds vault)
             Notification: "Semantic search is ready — try searching
                           by meaning, not just keywords"
Day 1-3:     Cognitive state badges (after pulse baseline forms)
             Notification: "Your focus patterns are emerging —
                           results now show cognitive context"
Day 7:       First pattern report
             Notification: "Your first weekly insight report is ready"
Day 30:      Full QS correlations (after HRV/sleep baseline)
             Notification: "30 days of data — here's what we've learned
                           about your best thinking conditions"
```

Each unlock notification includes a guided query that demonstrates the
new capability concretely. Not "semantic search is enabled" — "Try:
ambient query 'notes I wrote when I understood something deeply'"

### Guided First Query

After setup wizard completes, guided_query.rs selects a query template
that will return interesting results from any vault:

Priority order (try each, use first that returns 3+ results):
1. "ideas I keep coming back to"
2. "things I want to remember"
3. "projects I'm working on"
4. Most-linked note in the graph (always works if vault has wikilinks)
5. Most recent 3 notes (always works)

Print results with an explanation of what the system found and why.
This is the user's first experience of Ambient's intelligence — it must
succeed and feel non-trivial.

### Ambient Onboard Wiring in ambient-cli

Add ambient setup command that runs SetupWizard.
Add ambient doctor command that runs HealthCheck and prints capability
statuses for all GatedCapabilitys with remediation instructions.

```
$ ambient doctor

Capability Status
─────────────────────────────────────────
✓ Full-text search       Ready
✓ Graph traversal        Ready
⏳ Semantic search       Pending — ollama embedding in progress
                         ETA: ~4 minutes (847 notes remaining)
⚠ Cognitive badges       Pending — pulse baseline needs 2 more days
✗ Health correlations    Requires setup — enable in config + grant HealthKit
✗ Calendar context       Requires setup — enable in config + grant Calendar
● Menu bar overlay       Ready (paid tier active)

Run 'ambient setup --semantic' to install ollama.
Run 'ambient setup --health' to enable HealthKit integration.
```

---

## Amendment 5 — Revised Crate Structure

Replace the 10-crate structure in CLAUDE.md with this 11-crate structure:

```
ambient-core/         All crossing point types, all traits, Mock* impls
                      Includes: FeedbackEvent, FeedbackSignal, ResultAction
                                CapabilityStatus, GatedCapability, SetupAction
                                HealthMetric, CognitiveState (extended)
                                new RawPayload variants, new PulseSignal variants

ambient-watcher/      SourceAdapters: Obsidian, Spotlight, AppleNotes
                      PulseSamplers: ContextSwitch, ActiveApp, AudioInput
                      QS Adapters: HealthKitAdapter, CalendarAdapter
                      QS Samplers: CalendarContextSampler
                      SystemLoad sampler + LoadBroadcaster

ambient-normalizer/   Markdown, AppleNotes, Spotlight, PlainText normalizers
                      QS Normalizers: HealthKit, Calendar, SelfReport

ambient-store/        CozoDB (replaces LadybugDB) + tsink
                      Unified KnowledgeStore impl
                      record_feedback, feedback_score, pattern_feedback

ambient-spotlight/    Inbound NSMetadataQuery, Outbound CSSearchableIndex

ambient-reasoning/    Rig + ollama backend, EmbeddingQueue, LoadAware

ambient-patterns/     PatternDetector (Datalog rules via CozoDB)
                      QS pattern rules: sleep-vs-output, meeting-load-vs-quality
                      Gap detection, decay alerts

ambient-triggers/     TriggerEngine, new QS TriggerCondition variants
                      OptimalWindowDetected, PostMeetingRecovery,
                      KnowledgeGapDetected, EnergyThreshold

ambient-query/        AmbientQueryEngine with feedback-weighted reranking
                      CapabilityGate-aware degradation chain

ambient-menubar/      Cmd+Shift+Space overlay, cognitive state badges
                      Feedback recording (acted-on / dismissed)

ambient-onboard/      SetupWizard, CapabilityGate impl, UnlockNotifier
                      GuidedQuery, HealthCheck, ambient setup + doctor commands

ambient-cli/          Daemon entry point, axum HTTP server
                      SelfReportCollector, /checkin endpoint
                      ambient checkin, query --feedback commands
```

---

## Amendment 6 — Revised Build Phase Order

The phase order in CLAUDE.md is amended. New phases:

```
Phase 1 — Spine (unchanged from CLAUDE.md)
  Scaffold, ambient-core, ObsidianAdapter → MarkdownNormalizer → CozoDB
  CLI: ambient watch
  Gate: cargo test --workspace

Phase 2 — Pulse (amended: CozoDB schema includes feedback_events)
  ContextSwitchSampler, ActiveAppSampler, AudioInputSampler
  tsink integration, unit_with_context, CognitiveState
  CLI: ambient status
  Gate: battery test < 200mW

Phase 3 — Intelligence (unchanged from CLAUDE.md)
  Rig + ollama, EmbeddingQueue, CozoDB HNSW vector index
  QueryEngine, CLI: ambient query, HTTP /query
  Gate: semantic search + graceful FTS degradation

Phase 4 — Onboarding (new, before Pattern/Trigger)
  ambient-onboard crate, SetupWizard, CapabilityGate
  ambient setup wizard, ambient doctor
  Progressive unlock notifications
  Gate: end-to-end setup completes in < 2 minutes on clean machine

Phase 5 — Feedback Loop (new)
  FeedbackEvent in CozoDB, record_feedback, feedback_score
  QueryEngine reranking with feedback weight
  ambient-menubar feedback recording
  Implicit behavioral feedback via ActiveAppSampler correlation
  Gate: feedback reranking changes result order after 20 explicit signals

Phase 6 — Pattern and Trigger (was Phase 4)
  PatternDetector as Datalog rules in CozoDB
  TriggerEngine, macOS notifications
  Weekly HTML pattern report with feedback buttons
  Gate: pattern fires trigger → notification delivered

Phase 7 — Quantified Self (new)
  HealthKitAdapter, CalendarAdapter, CalendarContextSampler
  SelfReportCollector, /checkin endpoint, ambient checkin
  QS normalizers, extended CognitiveState
  QS pattern rules in PatternDetector
  QS trigger conditions: OptimalWindowDetected, PostMeetingRecovery
  Gate: unit_with_context returns HRV and sleep fields

Phase 8 — Spotlight (was Phase 5, unchanged)
Phase 9 — Menu Bar (was Phase 6, unchanged)
Phase 10 — Hard Sources (Apple Notes, was Phase 7)
Phase 11 — Distribution (was Phase 8)
```

---

## Coding Conventions Additions

Add to the coding conventions in CLAUDE.md:

- All CozoDB queries are strings — validate query structure in unit tests
  by running them against an in-memory CozoDB instance, not just compiling
- FeedbackEvent recording never blocks the UI thread — fire-and-forget
  via a dedicated tokio task, same pattern as SpotlightExporter
- CapabilityGate checks are mandatory before every gated feature call —
  no silent empty results, always return CapabilityStatus to caller
- SelfReport never stores mood/energy data in CozoDB without explicit
  user action — no passive inference of emotional state
- HealthKit data is normalized immediately on receipt — raw HealthKit
  records are never stored, only the normalized HealthKitSample
- Calendar attendee names and emails are never stored — attendee_count
  only, enforced at the CalendarNormalizer level

---

## What Does NOT Change from CLAUDE.md

- All crossing point types for existing variants (RawPayload, PulseSignal,
  KnowledgeUnit, QueryRequest, QueryResult) are additive only
- tsink is unchanged — storage config, metric names, retention policy
- Rig integration and ReasoningBackend — unchanged
- SystemLoad enum and LoadBroadcaster — unchanged
- SpotlightExporter — unchanged
- LicenseGate and paid tier features (menu bar overlay, hosted inference)
- Session prompts 1, 2, 3 — unchanged (scaffold, ambient-core, Phase 1)
- Session prompt 4 (Phase 2 Pulse) — unchanged
- Battery gate requirement — unchanged
- Privacy rules for ActiveAppSampler and keystroke sampling removal — unchanged
- All objc2 usage conventions — unchanged
- ambient-menubar and ambient-cli dependency rules — unchanged
