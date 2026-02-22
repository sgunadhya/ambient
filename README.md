<div align="center">
  <h1>🌱 Ambient</h1>
  <p><strong>A Continuous Local Cognitive Engine for macOS.</strong></p>
</div>

Most AI assistants suffer from severe amnesia. They sit passively in chat boxes, waiting for you to explicitly prompt them with your context, your state, and your intent. Traditional automations (like Apple Shortcuts or Zapier) are brittle `IF-THEN` scripts that break the second a workflow deviates.

Ambient bridges the gap. It is not a chatbot. It is a **Personal Cognitive Daemon** built in sub-millisecond Rust that sits silently on macOS, structurally modeling human memory—Sensory, Episodic, Semantic, and Procedural—to anticipate your needs without explicit commands.

---

## 🧠 The Cognitive Architecture

Ambient is designed around three distinct memory pillars, inspired by classical cognitive architectures like ACT-R and SOAR, but implemented with modern Vector DBs, Model Context Protocol (MCP), and Rust.

### 1. The Senses & Episodic Memory (`tsink`)
Instead of waiting for prompts, Ambient continuously observes your OS state. It logs window switches, calendar events, active applications, audio states, and biometric "energy levels" into an ultra-fast, local Write-Ahead Log (`tsink`). 
* It doesn't just know *what* a file is; it remembers that you were looking at a Jira ticket while on a Zoom call when your energy was low.

### 2. The Semantic Vault (`CozoDB`)
Your local markdown notes (e.g., Obsidian) and automated LLM summaries of your daily "Episodes" are embedded into a local Graph/Vector backend powered by CozoDB. Ambient creates a localized knowledge graph of your mind and workflows that never touches the cloud.

### 3. The Predictive UI ("Semantic Gravity")
Ambient rejects brittle `IF-THEN` procedural macros. Instead, it acts as an extended **Working Memory**. 
Every few seconds, Ambient evaluates your exact OS context against the vault and available tools using a "Semantic Gravity" equation (blending Vector Similarity + Time Decay + Reinforcement Learning). 

If a note or an action is highly relevant to your *exact* current moment, it surfaces dynamically to your Mac Menubar as a contextual Affordance.

> **Example:** You join a "Daily Standup" calendar meeting and open Slack. Ambient's Menubar silently glows. You click it, and the exact Notion design document you need, alongside a smart action to "Mute Spotify", are sitting right there. *It just knows.*

---

## 🛠️ Technology Stack

Ambient is built for extreme local performance, ensuring it can run constantly in the background without draining your Macbook's battery.

* **Core Engine:** Written entirely in `Rust` across heavily segmented crates (Workspace).
* **Declarative Memory:** `CozoDB` (Datalog Graph/Vector Database over SQLite) and `Rig` (LLM Embeddings/Inference).
* **Episodic Ledger:** Custom Rust Write-Ahead Log (`tsink`) chunking high-throughput OS context streams.
* **Procedural Action Layer:** First-class support for **Model Context Protocol (MCP 1.0)**. Ambient doesn't hardcode tool integrations; it leverages the open MCP ecosystem to execute local tools as the user clicks them.
* **Mac Integration:** Pure Objective-C/Swift bridge for menubar overlays, URL scheme handlers (`ambient://`), and AppleScript state extraction without electron bloat.

---

## 🚀 The Vision

We are moving from "Command-Line Interfaces" (CLIs) to "Locomotion/Context Interfaces." Ambient doesn't replace your active work; it eliminates the friction of context-switching by physically projecting the information and tools you need based on the gravity of your current environment.

It is 100% local, completely private, and purely personal.

---

### Request for Feedback

We are currently evaluating the architectural boundaries of Ambient. If this conceptual model interests you, we would love your thoughts on three specific questions:

1. **The Privacy vs. Magic Line:** Ambient relies on continuous OS telemetry to generate "Semantic Gravity". Does the value of zero-prompt, predictive affordances outweigh the friction of letting a local daemon watch your screen state 24/7?
2. **The UX Mechanism:** We chose a 'Predictive Menubar' instead of a silent 'Hot Loop' that clicks buttons for you automatically. Do you prefer an agent that asks permission by glowing in the menubar, or an agent that takes repetitive actions completely silently in the background?
3. **The Data Ingestion:** Semantic memory currently pulls from local Markdown paths. If you were to adopt this, what other passive data streams (e.g., Apple Notes, Browser History, IDE states) would be strictly mandatory for Ambient to be useful?

---
*Ambient is currently in architectural evaluation.*
