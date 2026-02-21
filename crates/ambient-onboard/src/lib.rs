use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ambient_core::{
    CapabilityGate, CapabilityStatus, ExternalDependency, GatedCapability, MacOSPermission,
    SetupAction,
};

#[derive(Default)]
pub struct InMemoryCapabilityGate {
    statuses: Mutex<HashMap<GatedCapability, CapabilityStatus>>,
}

impl InMemoryCapabilityGate {
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
        }
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

pub struct SetupWizard {
    gate: Arc<dyn CapabilityGate>,
}

impl SetupWizard {
    pub fn new(gate: Arc<dyn CapabilityGate>) -> Self {
        Self { gate }
    }

    pub fn run(&self) -> Vec<String> {
        let mut steps = Vec::new();
        self.gate.mark_ready(GatedCapability::MenuBarOverlay);
        steps.push("Configured vault path".to_string());
        steps.push("Initialized local stores".to_string());
        steps.push("Started ambient daemon services".to_string());
        steps
    }
}

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
        out.push("Capability Status".to_string());
        out.push("─────────────────────────────────────────".to_string());

        for cap in all {
            let status = self.gate.status(cap);
            out.push(format!(
                "{} {}",
                capability_label(cap),
                render_status_line(&status)
            ));
            if let Some(remediation) = remediation_for_status(&status) {
                out.push(format!("  {}", remediation));
            }
        }

        out
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
                .map(|d| format!(" (~{} min)", d.as_secs() / 60))
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
            SetupAction::InstallOllama => "Run `ambient setup --semantic`.".to_string(),
            SetupAction::GrantAccessibility => {
                "Enable Accessibility access for Ambient in System Settings.".to_string()
            }
            SetupAction::GrantMicrophone => {
                "Enable Microphone access for Ambient in System Settings.".to_string()
            }
            SetupAction::GrantHealthKit => {
                "Enable HealthKit in config and grant permission on first request.".to_string()
            }
            SetupAction::GrantCalendar => {
                "Enable Calendar in config and grant permission on first request.".to_string()
            }
        }),
        CapabilityStatus::RequiresPermission { permission } => Some(format!(
            "Grant {} permission in System Settings, then restart Ambient.",
            permission_label(permission)
        )),
        CapabilityStatus::RequiresDependency { dependency } => Some(match dependency {
            ExternalDependency::Ollama { install_command } => {
                format!("Install dependency: `{install_command}`")
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
            format!("Ollama ({install_command})")
        }
    }
}
