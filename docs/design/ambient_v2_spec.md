# Ambient V2: Resident Cognition Specification 🧠

## Vision
Ambient V2 is an **Evolutionary Refinement** of the existing system. It promotes the current **StreamProvider (WAL)** from a sync-mechanism to the definitive source of truth, allowing independent **Lenses** to consume and project knowledge into **CozoDB** as a structured memory cache.

## 1. Foundation Mapping: Reuse vs. Evolution
V2 does not reimplement the core; it redistributes logic across existing foundations:

| Foundation | V1 Role | V2 Role (Evolution) |
| :--- | :--- | :--- |
| **StreamProvider** | Background Sync | **Absolute Source of Truth (WAL)**. All indexing starts here. |
| **tsink** | Pulse Storage | **Ephemeral Context Store**. Optimized for Harmonic L4-Temporal analysis. |
| **CozoStore** | Primary Unit Store | **Knowledge Cache (Relational Graph)**. Stores projections and joins. |
| **ReasoningEngine** | Single-Head Embedder | **Lens Router**. Dispatches to Power-5 specialized models. |

## 2. Sensory Layer: The Power-5 Lenses
In V2, core data structures are **pure capture objects**. They contain no vector data. Embeddings are stored in a disjoint **LensMap** within CozoDB.

| Lens | Implementation | Storage |
| :--- | :--- | :--- |
| **L1: Semantic** | Nomic-v2 / Gemma-300M | `<F32;768> HNSW` in `lens_map` |
| **L2: Technical** | Jina-Code-v2 (AST-Aware) | `<F32;768> HNSW` in `lens_map` |
| **L3: Anchor** | CozoDB FTS (built-in) | FTS index on `notes.content` (no `lens_map` row) |
| **L4: Temporal** | Scoring function (see §2.1) | `temporal_profile` table (see §2.1) |
| **L5: Social** | GTE-Small (entities) + Graph (links) | `<F32;384> HNSW` for similarity + `note_links` edges for traversal |

### 2.1 L4 Temporal — Concrete Schema
L4 is **not** a vector embedding. It is a per-unit scoring function backed by a stored profile.

**`temporal_profile` table** (Cozo): `{unit_id, hour_peak: Int, day_mask: Int, recency_weight: Float}`
- `hour_peak`: The hour-of-day (0–23) at which this unit's source type is most commonly modified.
- `day_mask`: A 7-bit bitmask (Mon=bit0) of which weekdays the unit has been accessed.
- `recency_weight`: `exp(-λ * days_since_observed)` where λ = 0.05 for notes, 0.3 for pulses.

**At query time (Rust)**: `l4_score(unit) = recency_weight * cos_similarity(hour_of_query, hour_peak)`. This is a scalar computed in Rust and used to re-rank L1/L2 results — it does not participate in HNSW.

### 2.2 L5 Social — Two-Path Query Model
L5 uses two **separate** storage primitives that answer different questions:
- **Similarity path** (`lens_map`, GTE-Small `<F32;384>` HNSW): Answers "Which units mention entities *like* this person?"
- **Traversal path** (`note_links` edges, Cozo graph): Answers "Which units are *directly linked* to this unit via a wiki-link or mention?"

The Datalog query uses the **similarity path** for initial candidate discovery and the **traversal path** to expand top-K results by 1 hop:
```datalog
# Find notes semantically related to entity X, expanded by graph
?[unit_id, score] :=
  *lens_map:hnsw_social{vec: $v_entity, k: 30} -> [unit_id, d_social],
  score = 1.0 / (1.0 + d_social)
```
The graph hop is performed in Rust on the returned IDs: `store.related(unit_id, depth=1)`.

## 3. Storage Layer: The Datalog Hub (CozoDB)
CozoDB performs **Relational Joins** to reconnect raw data with its multi-lens vectors.

### The Hybrid Query Pattern
Retrieval is performed by joining L1 and L2 lenses with an L4 temporal re-rank:
```datalog
# Hybrid: code + semantic, filtered to recent
?[unit_id, d] :=
  *lens_map:hnsw_technical{vec: $v_code, k: 50} -> [unit_id, d]
```
L4 re-rank and L5 graph expansion are applied to results in Rust after Datalog retrieval.

## 4. Learning Layer: The Noticer (Procedural Memory)
The Noticer is a **Log-Consumer** that discovers correlations between behavioral "Pulses" and semantic "Lenses" to synthesize **Production Rules**.

### 4.1 Discovery Inputs
The Noticer operates on a **Synchronicity Window**:
1.  **PulseHistory (Behavioral)**: App switches, flow state, meeting status from the WAL.
2.  **LensMap (Intent)**: The vector profile of the document/code the user is currently focused on.
3.  **OracleFeedback (Intelligence)**: `OracleResultActedOn` events (highest weight signal).

### 4.2 Algorithm: Discovery & Clustering
The Noticer uses **Adaptive Discovery** with a two-stage clustering strategy.

- **Trigger**: Discovery runs when **(New Events > 500) OR (Time since last run > 24h)**. The OR ensures the Noticer still fires during early bootstrapping when event count is below 500.

- **Stage 1 — Candidate Selection (HNSW pre-filter)**: For each recent `PulseEvent`, query the L1 `lens_map` HNSW to find the 50 nearest lens vectors. This reduces the working set from all events to O(recent_pulses × 50) candidate pairs — without any dimensionality reduction.

- **Stage 2 — Cluster Discovery (DBSCAN in 32D)**:
  1. Project candidate L1 vectors from 768D → 32D using PCA (fitted once on the first 1,000 units, cached to disk).
  2. Run DBSCAN (`epsilon = 0.25`, `min_samples = 5`) in the 32D space.
  3. For each cluster, compute the **centroid** in the *original 768D space* (not 32D) for use as `lens_intent` in the rule.

> [!IMPORTANT]
> The PCA projection is used **only** for clustering geometry. The stored `lens_intent` centroid is always in the original 768D space to ensure cosine similarity at rule-fire time is accurate.

- **Rule Generation**: Clusters with `min_samples >= 5` are promoted to "Candidate Rules" for user approval.

### 4.3 Hybrid Rule Execution
To maintain performance, rule evaluation is split between **Datalog (Exact)** and **Rust (Approximate)**.

1.  **Datalog Segment**: A query retrieves all potentially active rules based on the **Pulse Bitmask** (exact match).
2.  **Rust Segment**: For the returned candidate rules, the system computes `cosine_similarity(current_lens, rule.lens_intent)`.
3.  **Firing**: If similarity > `dynamic_threshold`, the `action_payload` is executed.

**Datalog Predicate**: `procedural_rule(id, lens_intent, pulse_mask, threshold, action_payload)`

## 5. The Feedback Loop: Closing the Circuit
The system is "Closed-Loop." The Oracle (LLM) is not a dead-end; it is a signal generator.

1.  **Action**: User invokes `/ask` -> Oracle generates answer -> User **copies** or **clicks a link** in the answer.
2.  **Signal**: Ambient appends an `OracleResultActedOn` event to the WAL.
3.  **Consumption**: The Noticer sees this high-quality event. It links the **Current Pulse** + **Current Lenses** + **The Oracle's Success**.
4.  **Optimization**: This is how Ambient "learns" that its own suggestions were correct, further refining the "Synchronicity Peaks" for future rules.

## 6. Reasoning Layer: The Gated Oracle
Ambient uses a gated LLM strategy to balance intelligence and power consumption.

- **Fast Path**: Deterministic Datalog rules (Section 4.3) trigger instant UI updates with zero LLM overhead.
- **Deep Path**: When the user initiates an `/ask` query, Ambient:
  1. Retrieves top-10 fused units via Datalog.
  2. Wakes up a quantized LLM (Phi-4 or Gemma-2B).
  3. Synthesizes an answer.
  4. **Emits feedback event** to WAL on user action (Section 5).

## 7. Pipeline Architecture: Reorganizing the Consumers
V2 solves the bottleneck by splitting the existing `NormalizerConsumer` logic into three independent consumers running on the **existing WAL plumbing**.

### 7.1 The Ingestor (Existing Producer)
- **Status**: **REUSED**. Leverages current `Normalizer` and `StreamProvider::append`.
- **Action**: Persists raw events. No change to logic; change to priority (Guaranteed Capture).

### 7.2 The Database Committer (New Consumer A)
- **Status**: **EVOLVED**. Moves `CozoStore::upsert` logic into a WAL consumer.
- **Action**: Replays events to populate FTS and Relational tables.

### 7.3 The Multi-Lens Indexer (New Consumer B)
- **Status**: **EVOLVED**. Moves `EmbeddingQueue` logic into a multi-head WAL consumer.
- **Action**: Generates Power-5 vectors and inserts into `lens_map`.

---

> [!IMPORTANT]
> **Source of Truth Rule**: CozoDB is a *projection* of the event stream. The WAL is the *memory*. If CozoDB is deleted, the entire system can be reconstituted by replaying the WAL.

---

> [!IMPORTANT]
> **Asymmetric Design Rule**: One-time background "embedding tax" (Indexing) enables zero-waiting "local-intelligence" (Querying).
