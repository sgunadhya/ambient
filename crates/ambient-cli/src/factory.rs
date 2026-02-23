use std::sync::Arc;

use ambient_core::{
    CandidateSelector, Clusterer, DiscoveryTrigger, IntentScorer, PulseMatcher, ReasoningEngine,
    Result, RuleExecutor, RulePublisher, RuleSynthesizer, ThresholdPolicy,
};
use ambient_patterns::{
    MvpCandidateSelector, MvpClusterer, MvpDiscoveryTrigger, MvpIntentScorer, MvpPulseMatcher,
    MvpRulePublisher, MvpRuleSynthesizer, MvpThresholdPolicy, NoopRuleExecutor, NoticerEngine,
    RuleEngine, SpecCandidateSelector, SpecIntentScorer, SpecPulseMatcher, SpecThresholdPolicy,
};

use crate::AmbientConfig;

pub fn build_rule_engine(config: &AmbientConfig) -> Result<Arc<RuleEngine>> {
    let matcher: Arc<dyn PulseMatcher> = match config.rules_matcher.as_str() {
        "exact_bitmask" | "mvp" => Arc::new(MvpPulseMatcher),
        "spec_bitmask" => Arc::new(SpecPulseMatcher),
        _ => Arc::new(MvpPulseMatcher),
    };

    let scorer: Arc<dyn IntentScorer> = match config.rules_scorer.as_str() {
        "cosine" | "mvp" => Arc::new(MvpIntentScorer),
        "spec_cosine" => Arc::new(SpecIntentScorer),
        _ => Arc::new(MvpIntentScorer),
    };

    let threshold: Arc<dyn ThresholdPolicy> = match config.rules_threshold.as_str() {
        "dynamic" | "mvp" => Arc::new(MvpThresholdPolicy),
        "spec_dynamic" => Arc::new(SpecThresholdPolicy),
        _ => Arc::new(MvpThresholdPolicy),
    };

    let executor: Arc<dyn RuleExecutor> = match config.rules_executor.as_str() {
        "noop" => Arc::new(NoopRuleExecutor),
        _ => Arc::new(NoopRuleExecutor),
    };

    Ok(Arc::new(RuleEngine::new(
        matcher, scorer, threshold, executor,
    )))
}

pub fn build_noticer_engine(
    config: &AmbientConfig,
    reasoning: Arc<dyn ReasoningEngine>,
) -> Result<Arc<NoticerEngine>> {
    let trigger: Arc<dyn DiscoveryTrigger> = match config.noticer_trigger.as_str() {
        "events_or_24h" | "mvp" => Arc::new(MvpDiscoveryTrigger),
        _ => Arc::new(MvpDiscoveryTrigger),
    };

    let selector: Arc<dyn CandidateSelector> = match config.noticer_selector.as_str() {
        "mvp_lens" | "mvp" => Arc::new(MvpCandidateSelector),
        "spec_candidate" => Arc::new(SpecCandidateSelector),
        _ => Arc::new(MvpCandidateSelector),
    };

    let clusterer: Arc<dyn Clusterer> = match config.noticer_clusterer.as_str() {
        "mvp_dbscan" | "pca32_dbscan" | "mvp" => Arc::new(MvpClusterer),
        _ => Arc::new(MvpClusterer),
    };

    let synthesizer: Arc<dyn RuleSynthesizer> = match config.noticer_synthesizer.as_str() {
        "llm_json" | "mvp" => Arc::new(MvpRuleSynthesizer::new(reasoning)),
        _ => Arc::new(MvpRuleSynthesizer::new(reasoning)),
    };

    let publisher: Arc<dyn RulePublisher> = match config.noticer_publisher.as_str() {
        "store_rule" | "mvp" => Arc::new(MvpRulePublisher),
        _ => Arc::new(MvpRulePublisher),
    };

    Ok(Arc::new(NoticerEngine::new(
        trigger,
        selector,
        clusterer,
        synthesizer,
        publisher,
    )))
}
