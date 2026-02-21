use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ambient_core::{
    CapabilityGate, CapabilityStatus, ExternalDependency, GatedCapability, MacOSPermission,
    SetupAction,
};

// ── CapabilityGate implementation ─────────────────────────────────────────────

#[derive(Default)]
pub struct InMemoryCapabilityGate {
    statuses: Mutex<HashMap<GatedCapability, CapabilityStatus>>,
}

impl InMemoryCapabilityGate {
    /// Seed all capabilities with their initial default statuses.
    /// Should be called once after construction.
    pub fn seed_defaults(&self) {
        if let Ok(mut guard) = self.statuses.lock() {
            guard.insert(
                GatedCapability::SemanticSearch,
                CapabilityStatus::RequiresDependency {
                    dependency: ExternalDependency::Ollama {
                        install_command: "brew install ollama && ollama pull nomic-embed-text"
                            .to_string(),
                    },
                },
            );
            guard.insert(
                GatedCapability::CognitiveBadges,
                CapabilityStatus::Pending {
                    reason: "collecting baseline pulse context".to_string(),
                    estimated_eta: Some(Duration::from_secs(2 * 24 * 3600)),
                },
            );
            guard.insert(
                GatedCapability::PatternReports,
                CapabilityStatus::Pending {
                    reason: "requires 7 days of pulse data".to_string(),
                    estimated_eta: Some(Duration::from_secs(7 * 24 * 3600)),
                },
            );
            guard.insert(
                GatedCapability::HealthCorrelations,
                CapabilityStatus::RequiresPermission {
                    permission: MacOSPermission::HealthKit,
                },
            );
            guard.insert(
                GatedCapability::CalendarContext,
                CapabilityStatus::RequiresPermission {
                    permission: MacOSPermission::Calendar,
                },
            );
            guard.insert(GatedCapability::MenuBarOverlay, CapabilityStatus::Ready);
        }
    }

    /// Promote SemanticSearch to Ready (called after ollama probe succeeds).
    pub fn mark_ollama_ready(&self) {
        self.mark_ready(GatedCapability::SemanticSearch);
    }
}

impl CapabilityGate for InMemoryCapabilityGate {
    fn status(&self, capability: GatedCapability) -> CapabilityStatus {
        self.statuses
            .lock()
            .ok()
            .and_then(|map| map.get(&capability).cloned())
            .unwrap_or(CapabilityStatus::RequiresSetup {
                action: SetupAction::ConfigureVaultPath,
            })
    }

    fn mark_ready(&self, capability: GatedCapability) {
        if let Ok(mut guard) = self.statuses.lock() {
            guard.insert(capability, CapabilityStatus::Ready);
        }
    }
}

// ── Vault detection ───────────────────────────────────────────────────────────

/// Candidate directories that Obsidian typically uses by default.
const OBSIDIAN_CANDIDATE_DIRS: &[&str] = &["Documents/Obsidian", "Obsidian", "vault", "notes"];

/// Heuristic: a directory counts as an Obsidian vault if it contains a
/// `.obsidian/` sub-directory or at least one `.md` file.
pub fn looks_like_obsidian_vault(path: &Path) -> bool {
    if path.join(".obsidian").is_dir() {
        return true;
    }
    std::fs::read_dir(path)
        .ok()
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .any(|e| e.path().extension().map(|ext| ext == "md").unwrap_or(false))
        })
        .unwrap_or(false)
}

/// Return all Obsidian vaults found under $HOME, in priority order.
pub fn detect_obsidian_vaults() -> Vec<PathBuf> {
    let Ok(home) = std::env::var("HOME") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    let mut found = Vec::new();

    for candidate in OBSIDIAN_CANDIDATE_DIRS {
        let path = home.join(candidate);
        if path.is_dir() && looks_like_obsidian_vault(&path) {
            found.push(path);
        }
    }

    // Also scan ~/Documents for any sub-directory with .obsidian/
    let docs = home.join("Documents");
    if let Ok(entries) = std::fs::read_dir(&docs) {
        for entry in entries.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.is_dir() && p.join(".obsidian").is_dir() && !found.contains(&p) {
                found.push(p);
            }
        }
    }

    found
}

/// Count `.md` files immediately under a directory (non-recursive estimate).
pub fn count_md_files(path: &Path) -> usize {
    std::fs::read_dir(path)
        .ok()
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map(|ext| ext == "md").unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
}

// ── Ollama probe ──────────────────────────────────────────────────────────────

/// Non-blocking HTTP probe: returns `true` when ollama is reachable.
pub fn probe_ollama(base_url: &str) -> bool {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    // Use reqwest in blocking mode with a tight timeout.
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()
        .and_then(|c| c.get(&url).send().ok())
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

// ── SetupWizard ───────────────────────────────────────────────────────────────

/// Interactive terminal setup wizard per NEXT.md Amendment 4.
///
/// Returns a list of status/output lines suitable for printing to stdout.
/// Progresses through steps:
///   1. Detect or ask for vault path
///   2. Probe ollama availability
///   3. Confirm daemon services
pub struct SetupWizard {
    gate: Arc<dyn CapabilityGate>,
    /// Override path (used in tests or when called with `--vault`).
    vault_override: Option<PathBuf>,
    /// Override ollama base URL.
    ollama_url: String,
}

impl SetupWizard {
    pub fn new(gate: Arc<dyn CapabilityGate>) -> Self {
        Self {
            gate,
            vault_override: None,
            ollama_url: "http://localhost:11434".to_string(),
        }
    }

    pub fn with_vault(mut self, path: PathBuf) -> Self {
        self.vault_override = Some(path);
        self
    }

    pub fn with_ollama_url(mut self, url: String) -> Self {
        self.ollama_url = url;
        self
    }

    /// Run the setup wizard. Returns printable output lines.
    pub fn run(&self) -> Vec<String> {
        let mut out = vec![
            String::new(),
            "🌱 Ambient — Personal Cognitive Intelligence".to_string(),
            String::new(),
            "Step 1/3: Knowledge Sources".to_string(),
        ];

        let vault = self.step_detect_vault(&mut out);

        // Step 2 — Semantic Search via ollama
        out.push(String::new());
        out.push("Step 2/3: Local Search".to_string());
        self.step_check_ollama(&mut out);

        // Step 3 — Confirm daemon is configured
        out.push(String::new());
        out.push("Step 3/3: Starting Ambient".to_string());
        self.step_finalize(&mut out, vault.as_deref());

        out.push(String::new());
        out.push("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━".to_string());
        out.push("  Ready. Run `ambient watch` to start the daemon.".to_string());
        out.push("  Try: `ambient query \"my best ideas\"`".to_string());
        out.push("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━".to_string());
        out.push(String::new());

        // Mark vault as ready
        self.gate.mark_ready(GatedCapability::MenuBarOverlay);

        out
    }

    fn step_detect_vault(&self, out: &mut Vec<String>) -> Option<PathBuf> {
        if let Some(ref path) = self.vault_override {
            let note_count = count_md_files(path);
            out.push(format!(
                "  ✓ Using vault: {} ({} notes)",
                path.display(),
                note_count
            ));
            return Some(path.clone());
        }

        out.push("  Scanning for Obsidian vaults...".to_string());
        let vaults = detect_obsidian_vaults();

        if let Some(vault) = vaults.first() {
            let note_count = count_md_files(vault);
            out.push(format!(
                "  ✓ Found: {} ({} notes)",
                vault.display(),
                note_count
            ));
            Some(vault.clone())
        } else {
            out.push("  ⚠ No Obsidian vault detected.".to_string());
            out.push(
                "  → Set obsidian_vault in ~/.ambient/config.toml to enable ingestion.".to_string(),
            );
            None
        }
    }

    fn step_check_ollama(&self, out: &mut Vec<String>) {
        out.push("  Checking ollama availability...".to_string());
        if probe_ollama(&self.ollama_url) {
            out.push("  ✓ ollama is running — semantic search available".to_string());
            out.push("  ✓ Full-text search ready (always available)".to_string());
            self.gate.mark_ready(GatedCapability::SemanticSearch);
        } else {
            out.push("  ✓ Full-text search ready".to_string());
            out.push(String::new());
            out.push("  Semantic search requires ollama (currently not running).".to_string());
            out.push(
                "  → Install: brew install ollama && ollama pull nomic-embed-text".to_string(),
            );
            out.push(
                "  → Then run `ambient watch` — semantic search enables automatically.".to_string(),
            );
        }
    }

    fn step_finalize(&self, out: &mut Vec<String>, vault: Option<&Path>) {
        if vault.is_some() {
            out.push("  ✓ Vault path configured".to_string());
        }
        out.push("  ✓ Ambient is configured and ready".to_string());
        out.push("  → Run `ambient watch` to start the background daemon".to_string());
        out.push("  → Run `ambient doctor` to check capability status".to_string());
    }
}

// ── HealthCheck / ambient doctor ─────────────────────────────────────────────

/// Produces `ambient doctor` output: capability status + remediation hints.
pub struct HealthCheck {
    gate: Arc<dyn CapabilityGate>,
}

impl HealthCheck {
    pub fn new(gate: Arc<dyn CapabilityGate>) -> Self {
        Self { gate }
    }

    pub fn status_lines(&self) -> Vec<String> {
        let all = [
            GatedCapability::SemanticSearch,
            GatedCapability::CognitiveBadges,
            GatedCapability::PatternReports,
            GatedCapability::HealthCorrelations,
            GatedCapability::CalendarContext,
            GatedCapability::MenuBarOverlay,
        ];

        let mut out = Vec::new();
        out.push(String::new());
        out.push("Capability Status".to_string());
        out.push("─────────────────────────────────────────".to_string());

        for cap in all {
            let status = self.gate.status(cap);
            let icon = status_icon(&status);
            out.push(format!(
                "{}  {:<28} {}",
                icon,
                capability_label(cap),
                render_status_line(&status)
            ));
            if let Some(remediation) = remediation_for_status(&status) {
                out.push(format!("   {}", remediation));
            }
        }

        out.push(String::new());
        out
    }
}

// ── GuidedQuery ───────────────────────────────────────────────────────────────

/// Template queries tried in priority order after setup completes.
/// Per NEXT.md Amendment 4 §Guided First Query.
pub const GUIDED_QUERY_TEMPLATES: &[&str] = &[
    "ideas I keep coming back to",
    "things I want to remember",
    "projects I'm working on",
    "notes worth revisiting",
];

/// Select the best guided query template to show after first setup.
/// In production this would actually call the QueryEngine; here we return
/// the first template (callers drive the actual search).
pub fn guided_query_template() -> &'static str {
    GUIDED_QUERY_TEMPLATES[0]
}

// ── Progressive unlock notifications ─────────────────────────────────────────

/// Human-readable unlock message for each capability.
pub fn unlock_notification(cap: GatedCapability) -> Option<&'static str> {
    match cap {
        GatedCapability::SemanticSearch => {
            Some("Semantic search is ready — try searching by meaning, not just keywords.")
        }
        GatedCapability::CognitiveBadges => {
            Some("Your focus patterns are emerging — results now show cognitive context.")
        }
        GatedCapability::PatternReports => Some("Your first weekly insight report is ready."),
        GatedCapability::HealthCorrelations => {
            Some("30 days of data — here's what we've learned about your best thinking conditions.")
        }
        GatedCapability::CalendarContext => {
            Some("Calendar context enabled — queries now reflect your meeting and focus schedule.")
        }
        GatedCapability::MenuBarOverlay => None,
    }
}

// ── Rendering helpers ─────────────────────────────────────────────────────────

fn status_icon(status: &CapabilityStatus) -> &'static str {
    match status {
        CapabilityStatus::Ready => "✓",
        CapabilityStatus::Pending { .. } => "⏳",
        CapabilityStatus::RequiresSetup { .. } => "⚠",
        CapabilityStatus::RequiresPermission { .. } => "✗",
        CapabilityStatus::RequiresDependency { .. } => "✗",
    }
}

fn capability_label(cap: GatedCapability) -> &'static str {
    match cap {
        GatedCapability::SemanticSearch => "Semantic search",
        GatedCapability::CognitiveBadges => "Cognitive badges",
        GatedCapability::PatternReports => "Pattern reports",
        GatedCapability::HealthCorrelations => "Health correlations",
        GatedCapability::CalendarContext => "Calendar context",
        GatedCapability::MenuBarOverlay => "Menu bar overlay",
    }
}

fn render_status_line(status: &CapabilityStatus) -> String {
    match status {
        CapabilityStatus::Ready => "Ready".to_string(),
        CapabilityStatus::Pending {
            reason,
            estimated_eta,
        } => {
            let eta = estimated_eta
                .map(|d| {
                    let hours = d.as_secs() / 3600;
                    if hours >= 24 {
                        format!(" (~{} days)", hours / 24)
                    } else {
                        format!(" (~{} min)", d.as_secs() / 60)
                    }
                })
                .unwrap_or_default();
            format!("Pending — {reason}{eta}")
        }
        CapabilityStatus::RequiresSetup { action } => {
            format!("Requires setup — {}", setup_action_label(action))
        }
        CapabilityStatus::RequiresPermission { permission } => {
            format!("Requires permission — {}", permission_label(permission))
        }
        CapabilityStatus::RequiresDependency { dependency } => {
            format!("Requires dependency — {}", dependency_label(dependency))
        }
    }
}

fn remediation_for_status(status: &CapabilityStatus) -> Option<String> {
    match status {
        CapabilityStatus::Ready => None,
        CapabilityStatus::Pending { .. } => {
            Some("Leave Ambient running to complete initialization.".to_string())
        }
        CapabilityStatus::RequiresSetup { action } => Some(match action {
            SetupAction::ConfigureVaultPath => {
                "Run `ambient setup` and choose a valid Obsidian vault path.".to_string()
            }
            SetupAction::InstallOllama => {
                "Run: brew install ollama && ollama pull nomic-embed-text".to_string()
            }
            SetupAction::GrantAccessibility => {
                "Enable Accessibility access for Ambient in System Settings → Privacy.".to_string()
            }
            SetupAction::GrantMicrophone => {
                "Enable Microphone access for Ambient in System Settings → Privacy.".to_string()
            }
            SetupAction::GrantHealthKit => {
                "Enable [sources] healthkit = true in config, then grant HealthKit permission."
                    .to_string()
            }
            SetupAction::GrantCalendar => {
                "Enable [sources] calendar = true in config, then grant Calendar permission."
                    .to_string()
            }
        }),
        CapabilityStatus::RequiresPermission { permission } => Some(format!(
            "Grant {} in System Settings → Privacy, then run `ambient watch`.",
            permission_label(permission)
        )),
        CapabilityStatus::RequiresDependency { dependency } => Some(match dependency {
            ExternalDependency::Ollama { install_command } => {
                format!("Install: {install_command}")
            }
        }),
    }
}

fn setup_action_label(action: &SetupAction) -> &'static str {
    match action {
        SetupAction::ConfigureVaultPath => "configure vault path",
        SetupAction::InstallOllama => "install ollama",
        SetupAction::GrantAccessibility => "grant accessibility",
        SetupAction::GrantMicrophone => "grant microphone",
        SetupAction::GrantHealthKit => "grant healthkit",
        SetupAction::GrantCalendar => "grant calendar",
    }
}

fn permission_label(permission: &MacOSPermission) -> &'static str {
    match permission {
        MacOSPermission::Accessibility => "Accessibility",
        MacOSPermission::Microphone => "Microphone",
        MacOSPermission::HealthKit => "HealthKit",
        MacOSPermission::Calendar => "Calendar",
    }
}

fn dependency_label(dep: &ExternalDependency) -> String {
    match dep {
        ExternalDependency::Ollama { install_command } => {
            format!("Ollama — run: {install_command}")
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_gate() -> Arc<InMemoryCapabilityGate> {
        let gate = Arc::new(InMemoryCapabilityGate::default());
        gate.seed_defaults();
        gate
    }

    #[test]
    fn test_capability_gate_defaults() {
        let gate = make_gate();
        // SemanticSearch starts as RequiresDependency
        assert!(matches!(
            gate.status(GatedCapability::SemanticSearch),
            CapabilityStatus::RequiresDependency { .. }
        ));
        // MenuBarOverlay marked ready in seed_defaults
        assert!(matches!(
            gate.status(GatedCapability::MenuBarOverlay),
            CapabilityStatus::Ready
        ));
    }

    #[test]
    fn test_mark_ready_promotes_status() {
        let gate = make_gate();
        gate.mark_ready(GatedCapability::SemanticSearch);
        assert!(matches!(
            gate.status(GatedCapability::SemanticSearch),
            CapabilityStatus::Ready
        ));
    }

    #[test]
    fn test_setup_wizard_run_returns_lines() {
        let gate = make_gate();
        let wizard = SetupWizard::new(gate);
        let lines = wizard.run();
        assert!(!lines.is_empty(), "wizard should return output lines");
        let joined = lines.join("\n");
        assert!(joined.contains("Step 1/3"), "should include step 1");
        assert!(joined.contains("Step 2/3"), "should include step 2");
        assert!(joined.contains("Step 3/3"), "should include step 3");
    }

    #[test]
    fn test_setup_wizard_with_vault_override() {
        let gate = make_gate();
        let tmp = std::env::temp_dir().join("ambient_test_vault");
        let _ = std::fs::create_dir_all(&tmp);
        // Create a dummy .md file
        let _ = std::fs::write(tmp.join("test.md"), "# Test");

        let wizard = SetupWizard::new(gate.clone()).with_vault(tmp.clone());
        let lines = wizard.run();
        let joined = lines.join("\n");
        assert!(
            joined.contains("Using vault"),
            "should report the provided vault path"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_health_check_status_lines() {
        let gate = make_gate();
        let hc = HealthCheck::new(gate);
        let lines = hc.status_lines();
        assert!(!lines.is_empty());
        let joined = lines.join("\n");
        assert!(joined.contains("Semantic search"));
        assert!(joined.contains("Menu bar overlay"));
    }

    #[test]
    fn test_looks_like_obsidian_vault_with_obsidian_dir() {
        let tmp = std::env::temp_dir().join("ambient_vault_test_obsidian");
        let _ = std::fs::create_dir_all(tmp.join(".obsidian"));
        assert!(looks_like_obsidian_vault(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_guided_query_template_is_nonempty() {
        assert!(!guided_query_template().is_empty());
    }

    #[test]
    fn test_unlock_notification_semantic_search() {
        assert!(unlock_notification(GatedCapability::SemanticSearch).is_some());
        assert!(unlock_notification(GatedCapability::MenuBarOverlay).is_none());
    }
}
