use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ambient_core::{
    CoreError, KnowledgeStore, KnowledgeUnit, Result, SourceId, TriggerAction, TriggerCondition,
};
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;

pub enum CognitiveStateFilter {
    Flow,
    OnCall,
}

pub enum BuiltInCondition {
    NewNoteFromSource { source_id: SourceId },
    ContentMatches { pattern: String },
    ConceptMentioned { concept: String },
}

impl TriggerCondition for BuiltInCondition {
    fn evaluate(&self, unit: &KnowledgeUnit, _store: &dyn KnowledgeStore) -> Result<bool> {
        match self {
            BuiltInCondition::NewNoteFromSource { source_id } => Ok(&unit.source == source_id),
            BuiltInCondition::ContentMatches { pattern } => Ok(unit.content.contains(pattern)),
            BuiltInCondition::ConceptMentioned { concept } => Ok(unit
                .metadata
                .get("concepts")
                .and_then(|v| v.as_array())
                .is_some_and(|values| values.iter().any(|v| v.as_str() == Some(concept)))),
        }
    }
}

pub enum BuiltInAction {
    MacOSNotification {
        title: String,
        body_template: String,
    },
    Webhook {
        url: String,
        method: String,
    },
    RunScript {
        path: PathBuf,
    },
}

impl TriggerAction for BuiltInAction {
    fn fire(&self, unit: &KnowledgeUnit) -> Result<()> {
        match self {
            BuiltInAction::MacOSNotification {
                title,
                body_template,
            } => {
                #[cfg(target_os = "macos")]
                {
                    let body = body_template
                        .replace("{title}", unit.title.as_deref().unwrap_or("untitled"));
                    let script =
                        format!("display notification \"{}\" with title \"{}\"", body, title);
                    let _ = Command::new("osascript").arg("-e").arg(script).output();
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = (title, body_template, unit.id);
                }
                Ok(())
            }
            BuiltInAction::Webhook { url, method: _ } => {
                if url.is_empty() {
                    return Err(CoreError::InvalidInput(
                        "webhook url cannot be empty".to_string(),
                    ));
                }
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .map_err(|e| CoreError::Internal(format!("webhook client init failed: {e}")))?;
                let method = match self {
                    BuiltInAction::Webhook { method, .. } => method.to_uppercase(),
                    BuiltInAction::MacOSNotification { .. } | BuiltInAction::RunScript { .. } => {
                        "POST".to_string()
                    }
                };
                let request = match method.as_str() {
                    "GET" => client.get(url),
                    "POST" => client.post(url).json(&serde_json::json!({
                        "unit_id": unit.id,
                        "title": unit.title,
                        "source": unit.source.0,
                    })),
                    "PUT" => client.put(url).json(&serde_json::json!({
                        "unit_id": unit.id,
                        "title": unit.title,
                        "source": unit.source.0,
                    })),
                    "PATCH" => client.patch(url).json(&serde_json::json!({
                        "unit_id": unit.id,
                        "title": unit.title,
                        "source": unit.source.0,
                    })),
                    "DELETE" => client.delete(url),
                    _ => {
                        return Err(CoreError::InvalidInput(format!(
                            "unsupported webhook method: {}",
                            method
                        )));
                    }
                };
                let _ = request.send();
                Ok(())
            }
            BuiltInAction::RunScript { path } => {
                if !path.exists() {
                    return Err(CoreError::NotFound(format!(
                        "script not found: {}",
                        path.display()
                    )));
                }
                let output = Command::new(path)
                    .arg(unit.id.to_string())
                    .output()
                    .map_err(|e| CoreError::Internal(format!("failed to run script: {e}")))?;
                if !output.status.success() {
                    return Err(CoreError::Internal(format!(
                        "script exited with status {:?}",
                        output.status.code()
                    )));
                }
                Ok(())
            }
        }
    }
}

pub struct TriggerRule {
    pub condition: Arc<dyn TriggerCondition>,
    pub action: Arc<dyn TriggerAction>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ConditionConfig {
    NewNoteFromSource { source_id: String },
    ContentMatches { pattern: String },
    ConceptMentioned { concept: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ActionConfig {
    MacOsNotification {
        title: String,
        body_template: String,
    },
    Webhook {
        url: String,
        method: String,
    },
    RunScript {
        path: PathBuf,
    },
}

#[derive(Debug, Clone, Deserialize)]
struct RuleConfig {
    condition: ConditionConfig,
    action: ActionConfig,
}

pub struct TriggerEngine {
    store: Arc<dyn KnowledgeStore>,
    rules: Arc<Mutex<Vec<TriggerRule>>>,
}

impl TriggerEngine {
    pub fn new(store: Arc<dyn KnowledgeStore>, rules: Vec<TriggerRule>) -> Self {
        Self {
            store,
            rules: Arc::new(Mutex::new(rules)),
        }
    }

    pub fn load_from_path(store: Arc<dyn KnowledgeStore>, path: &Path) -> Result<Self> {
        let initial = load_rules(path)?;
        Ok(Self::new(store, initial))
    }

    pub fn start_watch(&self, path: PathBuf) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| CoreError::InvalidInput("trigger path has no parent".to_string()))?
            .to_path_buf();

        let rules = Arc::clone(&self.rules);
        thread::Builder::new()
            .name("trigger-rule-watch".to_string())
            .spawn(move || {
                let (notify_tx, notify_rx) = mpsc::channel();
                let mut watcher: RecommendedWatcher = match RecommendedWatcher::new(
                    move |res| {
                        let _ = notify_tx.send(res);
                    },
                    Config::default(),
                ) {
                    Ok(w) => w,
                    Err(_) => return,
                };

                if watcher.watch(&parent, RecursiveMode::NonRecursive).is_err() {
                    return;
                }

                while let Ok(res) = notify_rx.recv() {
                    let Ok(event) = res else {
                        continue;
                    };
                    match event.kind {
                        EventKind::Create(_) | EventKind::Modify(_) => {
                            if !event.paths.iter().any(|p| p == &path) {
                                continue;
                            }
                            if let Ok(new_rules) = load_rules(&path) {
                                if let Ok(mut guard) = rules.lock() {
                                    *guard = new_rules;
                                }
                            }
                        }
                        EventKind::Any
                        | EventKind::Access(_)
                        | EventKind::Remove(_)
                        | EventKind::Other => {}
                    }
                }
            })
            .map_err(|e| CoreError::Internal(format!("failed to spawn trigger watcher: {e}")))?;
        Ok(())
    }

    pub fn on_upsert(&self, unit: &KnowledgeUnit) {
        let rules = match self.rules.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        for rule in rules.iter() {
            let should_fire = rule
                .condition
                .evaluate(unit, self.store.as_ref())
                .unwrap_or(false);
            if should_fire {
                let _ = rule.action.fire(unit);
            }
        }
    }

    pub fn on_upsert_background(self: Arc<Self>, unit: KnowledgeUnit) {
        thread::spawn(move || {
            self.on_upsert(&unit);
        });
    }
}

fn load_rules(path: &Path) -> Result<Vec<TriggerRule>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let raw = fs::read_to_string(path)
        .map_err(|e| CoreError::Internal(format!("failed reading trigger file: {e}")))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }

    let parsed: Vec<RuleConfig> = serde_json::from_str(&raw)
        .map_err(|e| CoreError::InvalidInput(format!("invalid triggers.json: {e}")))?;

    let mut out = Vec::new();
    for rule in parsed {
        let condition: Arc<dyn TriggerCondition> = match rule.condition {
            ConditionConfig::NewNoteFromSource { source_id } => {
                Arc::new(BuiltInCondition::NewNoteFromSource {
                    source_id: SourceId::new(source_id),
                })
            }
            ConditionConfig::ContentMatches { pattern } => {
                Arc::new(BuiltInCondition::ContentMatches { pattern })
            }
            ConditionConfig::ConceptMentioned { concept } => {
                Arc::new(BuiltInCondition::ConceptMentioned { concept })
            }
        };

        let action: Arc<dyn TriggerAction> = match rule.action {
            ActionConfig::MacOsNotification {
                title,
                body_template,
            } => Arc::new(BuiltInAction::MacOSNotification {
                title,
                body_template,
            }),
            ActionConfig::Webhook { url, method } => {
                Arc::new(BuiltInAction::Webhook { url, method })
            }
            ActionConfig::RunScript { path } => Arc::new(BuiltInAction::RunScript { path }),
        };

        out.push(TriggerRule { condition, action });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ambient_core::{KnowledgeUnit, SourceId, TriggerAction, TriggerCondition};
    use chrono::Utc;

    use super::*;

    struct TrueCondition;
    impl TriggerCondition for TrueCondition {
        fn evaluate(&self, _unit: &KnowledgeUnit, _store: &dyn KnowledgeStore) -> Result<bool> {
            Ok(true)
        }
    }

    struct CountingAction(Arc<AtomicUsize>);
    impl TriggerAction for CountingAction {
        fn fire(&self, _unit: &KnowledgeUnit) -> Result<()> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn on_upsert_background_invokes_actions() {
        let store = Arc::new(ambient_core::mocks::MockKnowledgeStore::default());
        let count = Arc::new(AtomicUsize::new(0));
        let engine = Arc::new(TriggerEngine::new(
            store,
            vec![TriggerRule {
                condition: Arc::new(TrueCondition),
                action: Arc::new(CountingAction(Arc::clone(&count))),
            }],
        ));

        let unit = KnowledgeUnit {
            id: uuid::Uuid::new_v4(),
            source: SourceId::new("obsidian"),
            content: "x".to_string(),
            title: Some("x".to_string()),
            metadata: HashMap::new(),
            observed_at: Utc::now(),
            content_hash: [1; 32],
        };

        engine.on_upsert_background(unit);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }
}
