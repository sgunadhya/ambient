use std::sync::Arc;

use ambient_core::{
    EvalContext, IntentScorer, KnowledgeStore, ProceduralRule, PulseMatcher, PulseSnapshot, Result,
    RuleExecutor, ThresholdPolicy,
};

/// Orchestrates Rule Evaluation using interchangeable components.
pub struct RuleEngine {
    matcher: Arc<dyn PulseMatcher>,
    scorer: Arc<dyn IntentScorer>,
    threshold: Arc<dyn ThresholdPolicy>,
    executor: Arc<dyn RuleExecutor>,
}

impl RuleEngine {
    pub fn new(
        matcher: Arc<dyn PulseMatcher>,
        scorer: Arc<dyn IntentScorer>,
        threshold: Arc<dyn ThresholdPolicy>,
        executor: Arc<dyn RuleExecutor>,
    ) -> Self {
        Self {
            matcher,
            scorer,
            threshold,
            executor,
        }
    }

    /// Evaluates rules against a given pulse snapshot.
    /// Returns a list of (rule, score) pairs that met the threshold.
    pub fn evaluate(
        &self,
        store: &dyn KnowledgeStore,
        pulse: &PulseSnapshot,
        current_lens: &[f32],
    ) -> Result<Vec<(ProceduralRule, f32)>> {
        let mut passed = Vec::new();
        let candidates = self.matcher.candidates(store, pulse)?;

        for rule in candidates {
            let score = self.scorer.score(current_lens, &rule);
            let ctx = EvalContext {
                rule: rule.clone(),
                snapshot: pulse.clone(),
            };

            if score >= self.threshold.threshold(&rule, &ctx) {
                passed.push((rule, score));
            }
        }

        Ok(passed)
    }

    pub fn execute(&self, rule: &ProceduralRule) -> Result<()> {
        self.executor.execute(rule)
    }
}

// =====================================
// MVP Implementations (Status Quo)
// =====================================

/// MVP Pulse Matcher: Requires exact dominant app match.
pub struct MvpPulseMatcher;

impl PulseMatcher for MvpPulseMatcher {
    fn candidates(
        &self,
        store: &dyn KnowledgeStore,
        pulse: &PulseSnapshot,
    ) -> Result<Vec<ProceduralRule>> {
        let rules = store.get_all_rules()?;
        let mut matched = Vec::new();

        for rule in rules {
            if rule.pulse_mask.is_empty() {
                matched.push(rule);
            } else if let Some(app) = &pulse.current.dominant_app {
                if app == &rule.pulse_mask {
                    matched.push(rule);
                }
            }
        }

        Ok(matched)
    }
}

/// MVP Intent Scorer: Baseline 0.8 if matched.
pub struct MvpIntentScorer;

impl IntentScorer for MvpIntentScorer {
    fn score(&self, _current_lens: &[f32], rule: &ProceduralRule) -> f32 {
        if rule.lens_intent.is_empty() {
            1.0
        } else {
            0.8
        }
    }
}

/// MVP Threshold Policy: Exact static threshold.
pub struct MvpThresholdPolicy;

impl ThresholdPolicy for MvpThresholdPolicy {
    fn threshold(&self, rule: &ProceduralRule, _ctx: &EvalContext) -> f32 {
        rule.threshold
    }
}

/// No-op Executor.
pub struct NoopRuleExecutor;

impl RuleExecutor for NoopRuleExecutor {
    fn execute(&self, _rule: &ProceduralRule) -> Result<()> {
        Ok(())
    }
}

// =====================================
// Spec Implementations (Future Vision)
// =====================================

/// Spec Pulse Matcher: Bitmask-based subset matching.
pub struct SpecPulseMatcher;

impl PulseMatcher for SpecPulseMatcher {
    fn candidates(
        &self,
        store: &dyn KnowledgeStore,
        _pulse: &PulseSnapshot,
    ) -> Result<Vec<ProceduralRule>> {
        // Implementation out of scope for MVP wiring, fallback to get all rules.
        store.get_all_rules()
    }
}

/// Spec Intent Scorer: Cosine Similarity.
pub struct SpecIntentScorer;

impl IntentScorer for SpecIntentScorer {
    fn score(&self, current_lens: &[f32], rule: &ProceduralRule) -> f32 {
        if rule.lens_intent.is_empty() || current_lens.is_empty() {
            return 1.0;
        }

        let dot_product: f32 = current_lens
            .iter()
            .zip(&rule.lens_intent)
            .map(|(a, b)| a * b)
            .sum();
        let norm_a: f32 = current_lens.iter().map(|a| a * a).sum::<f32>().sqrt();
        let norm_b: f32 = rule.lens_intent.iter().map(|b| b * b).sum::<f32>().sqrt();

        if norm_a == 0.0 || norm_b == 0.0 {
            0.0
        } else {
            dot_product / (norm_a * norm_b)
        }
    }
}

/// Spec Threshold: Dynamic.
pub struct SpecThresholdPolicy;

impl ThresholdPolicy for SpecThresholdPolicy {
    fn threshold(&self, rule: &ProceduralRule, _ctx: &EvalContext) -> f32 {
        // Could lower threshold in high flow, raise during meetings, etc.
        rule.threshold
    }
}
