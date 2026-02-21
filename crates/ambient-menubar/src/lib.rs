use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ambient_core::{
    CoreError, GatedFeature, LicenseGate, QueryEngine, QueryRequest, QueryResult, Result,
};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmbientDeepLink {
    Unit(Uuid),
}

pub fn parse_ambient_deep_link(url: &str) -> Result<AmbientDeepLink> {
    let without_scheme = url
        .strip_prefix("ambient://")
        .ok_or_else(|| CoreError::InvalidInput("deep link must start with ambient://".to_string()))?;

    if let Some(id_str) = without_scheme.strip_prefix("unit/") {
        let id = Uuid::parse_str(id_str)
            .map_err(|e| CoreError::InvalidInput(format!("invalid unit id in deep link: {e}")))?;
        return Ok(AmbientDeepLink::Unit(id));
    }

    Err(CoreError::InvalidInput(
        "unsupported ambient deep link path".to_string(),
    ))
}

#[derive(Debug, Clone)]
pub struct OverlayState {
    pub visible: bool,
    pub query_text: String,
    pub results: Vec<QueryResult>,
    pub selected_index: usize,
    pub focused_unit: Option<Uuid>,
    pub last_query_at: Option<Instant>,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            visible: false,
            query_text: String::new(),
            results: Vec::new(),
            selected_index: 0,
            focused_unit: None,
            last_query_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayAction {
    None,
    OpenSource { unit_id: Uuid },
    OpenDetail { unit_id: Uuid },
    Dismissed,
}

pub struct MenubarController {
    engine: Arc<dyn QueryEngine>,
    gate: Arc<dyn LicenseGate>,
    state: Mutex<OverlayState>,
    debounce: Duration,
}

impl MenubarController {
    pub fn new(engine: Arc<dyn QueryEngine>, gate: Arc<dyn LicenseGate>) -> Self {
        Self {
            engine,
            gate,
            state: Mutex::new(OverlayState::default()),
            debounce: Duration::from_millis(300),
        }
    }

    pub fn with_debounce(mut self, debounce: Duration) -> Self {
        self.debounce = debounce;
        self
    }

    pub fn startup_check(&self) -> bool {
        self.gate.check(GatedFeature::MenuBarOverlay).is_ok()
    }

    pub fn on_hotkey(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.visible = true;
        }
    }

    pub fn dismiss(&self) -> OverlayAction {
        if let Ok(mut state) = self.state.lock() {
            state.visible = false;
            state.query_text.clear();
            state.results.clear();
            state.selected_index = 0;
        }
        OverlayAction::Dismissed
    }

    pub fn search(&self, text: &str) -> Result<Vec<QueryResult>> {
        self.engine.query(QueryRequest {
            text: text.to_string(),
            k: 10,
            include_pulse_context: true,
            context_window_secs: Some(120),
        })
    }

    pub fn update_query(&self, text: &str) -> Result<Vec<QueryResult>> {
        let now = Instant::now();
        let should_query = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| CoreError::Internal("overlay state lock poisoned".to_string()))?;
            state.query_text = text.to_string();
            match state.last_query_at {
                Some(last) if now.duration_since(last) < self.debounce => false,
                _ => {
                    state.last_query_at = Some(now);
                    true
                }
            }
        };

        if !should_query {
            return self.current_results();
        }

        let results = self.search(text)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| CoreError::Internal("overlay state lock poisoned".to_string()))?;
        state.results = results.clone();
        state.selected_index = 0;
        Ok(results)
    }

    pub fn open_deep_link(&self, url: &str) -> Result<()> {
        match parse_ambient_deep_link(url)? {
            AmbientDeepLink::Unit(id) => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| CoreError::Internal("overlay state lock poisoned".to_string()))?;
                state.visible = true;
                state.focused_unit = Some(id);
                Ok(())
            }
        }
    }

    pub fn focused_unit(&self) -> Option<Uuid> {
        self.state.lock().ok().and_then(|s| s.focused_unit)
    }

    pub fn current_state(&self) -> OverlayState {
        self.state
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| OverlayState::default())
    }

    pub fn current_results(&self) -> Result<Vec<QueryResult>> {
        let state = self
            .state
            .lock()
            .map_err(|_| CoreError::Internal("overlay state lock poisoned".to_string()))?;
        Ok(state.results.clone())
    }

    pub fn select_next(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.results.is_empty() {
                return;
            }
            state.selected_index = (state.selected_index + 1) % state.results.len();
        }
    }

    pub fn on_enter(&self) -> OverlayAction {
        self.with_selected_action(false)
    }

    pub fn on_cmd_enter(&self) -> OverlayAction {
        self.with_selected_action(true)
    }

    fn with_selected_action(&self, detail: bool) -> OverlayAction {
        let state = match self.state.lock() {
            Ok(s) => s,
            Err(_) => return OverlayAction::None,
        };
        let Some(selected) = state.results.get(state.selected_index) else {
            return OverlayAction::None;
        };

        if detail {
            OverlayAction::OpenDetail {
                unit_id: selected.unit.id,
            }
        } else {
            OverlayAction::OpenSource {
                unit_id: selected.unit.id,
            }
        }
    }
}

pub fn badge_for(result: &QueryResult) -> Vec<&'static str> {
    let mut badges = Vec::new();
    if let Some(state) = &result.cognitive_state {
        if state.was_in_flow {
            badges.push("🟢 Flow");
        }
        if state.was_on_call {
            badges.push("📞 On Call");
        }
        if let Some(hour) = state.time_of_day {
            if hour >= 22 || hour < 6 {
                badges.push("🌙 Late Night");
            }
            if (5..8).contains(&hour) {
                badges.push("🌅 Early Morning");
            }
        }
    }
    badges
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use ambient_core::mocks::{MockLicenseGate, MockQueryEngine};
    use ambient_core::{KnowledgeUnit, QueryResult, SourceId};
    use chrono::Utc;

    use super::{parse_ambient_deep_link, MenubarController, OverlayAction};

    #[test]
    fn parses_unit_deep_link() {
        let id = uuid::Uuid::new_v4();
        let link = format!("ambient://unit/{id}");
        let parsed = parse_ambient_deep_link(&link).expect("parse");
        assert!(matches!(parsed, super::AmbientDeepLink::Unit(x) if x == id));
    }

    #[test]
    fn enter_and_cmd_enter_emit_actions() {
        let engine = MockQueryEngine::default();
        let unit = KnowledgeUnit {
            id: uuid::Uuid::new_v4(),
            source: SourceId::new("obsidian"),
            content: "x".to_string(),
            title: Some("x".to_string()),
            metadata: HashMap::new(),
            embedding: None,
            observed_at: Utc::now(),
            content_hash: [1; 32],
        };
        engine
            .results
            .lock()
            .expect("lock")
            .push(QueryResult {
                unit: unit.clone(),
                score: 1.0,
                pulse_context: None,
                cognitive_state: None,
                historical_feedback_score: 0.5,
                capability_status: None,
            });
        let gate = MockLicenseGate {
            is_pro_user: true,
            expired_at: None,
            calls: std::sync::Mutex::new(Vec::new()),
        };

        let controller = MenubarController::new(Arc::new(engine), Arc::new(gate));
        controller.on_hotkey();
        let _ = controller.update_query("x").expect("query");

        assert_eq!(
            controller.on_enter(),
            OverlayAction::OpenSource { unit_id: unit.id }
        );
        assert_eq!(
            controller.on_cmd_enter(),
            OverlayAction::OpenDetail { unit_id: unit.id }
        );
    }
}
