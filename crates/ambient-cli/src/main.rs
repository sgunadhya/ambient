use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{env, net::SocketAddr, path::PathBuf};

use ambient_cli::{
    open_default_stream_provider, run_doctor, run_http_server, run_setup, AmbientConfig,
    DaemonStatusProbe, HealthResponse, HttpAppState, PulseConsumer, TransportRegistry,
};
use ambient_core::{
    GatedFeature, License, LicenseError, LicenseGate, LoadAware, QueryResult, SpotlightExporter,
    SystemLoad, TransportState,
};
use ambient_menubar::{
    parse_ambient_deep_link, register_url_handler, start_native_runtime, AmbientDeepLink,
    MenubarController,
};
use ambient_normalizer::{default_dispatch, NormalizerConsumer};
use ambient_onboard::InMemoryCapabilityGate;
use ambient_patterns::PatternScheduler;
use ambient_query::build_runtime_components_with_weights;
use ambient_reasoning::{LensConsumer, RigReasoningEngine};
use ambient_spotlight::CoreSpotlightExporter;
use ambient_transport_local::{DefaultLoadPolicyProvider, LoadBroadcaster, SystemLoadSampler};
use ambient_triggers::TriggerEngine;
use clap::{Parser, Subcommand};
use reqwest::blocking::Client;
use serde::Serialize;
use std::io::{self, Write};
use uuid::Uuid;

struct LocalLicenseGate {
    license: License,
}

impl LocalLicenseGate {
    fn from_env() -> Self {
        // Default: local daemon instances are Pro — license is enforced on
        // hosted/remote instances. Set AMBIENT_LICENSE=free to test gated paths.
        match std::env::var("AMBIENT_LICENSE").ok().as_deref() {
            Some("free") => Self {
                license: License::Free,
            },
            _ => Self {
                license: License::Pro {
                    expires_at: chrono::Utc::now() + chrono::Duration::days(3650),
                },
            },
        }
    }
}

impl LicenseGate for LocalLicenseGate {
    fn is_pro(&self) -> bool {
        matches!(self.license, License::Pro { .. })
    }

    fn check(&self, feature: GatedFeature) -> std::result::Result<(), LicenseError> {
        match self.license {
            License::Free => Err(LicenseError::ProRequired { feature }),
            License::Pro { expires_at } if chrono::Utc::now() > expires_at => {
                Err(LicenseError::Expired { expires_at })
            }
            License::Pro { .. } => Ok(()),
        }
    }
}

struct RuntimeStatusProbe {
    reasoning: Arc<RigReasoningEngine>,
    load_tracker: Arc<RuntimeLoadTracker>,
    upserted_units: Arc<AtomicU64>,
    active_sources: Vec<String>,
    active_samplers: Vec<String>,
    transport_registry: Arc<TransportRegistry>,
}

impl DaemonStatusProbe for RuntimeStatusProbe {
    fn reasoning_available(&self) -> bool {
        self.reasoning.reasoning_available()
    }

    fn load(&self) -> String {
        self.load_tracker.current()
    }

    fn queue_depth(&self) -> usize {
        0
    }

    fn upserted_units(&self) -> u64 {
        self.upserted_units.load(Ordering::Relaxed)
    }

    fn active_sources(&self) -> Vec<String> {
        self.active_sources.clone()
    }

    fn active_samplers(&self) -> Vec<String> {
        self.active_samplers.clone()
    }

    fn transport_statuses(&self) -> Vec<ambient_core::TransportStatus> {
        self.transport_registry.status_all()
    }
}

struct RuntimeLoadTracker {
    state: std::sync::Mutex<SystemLoad>,
}

struct DaemonPidGuard {
    path: PathBuf,
}

impl DaemonPidGuard {
    fn create(path: PathBuf) -> Result<Self, String> {
        write_daemon_pid(&path)?;
        Ok(Self { path })
    }
}

impl Drop for DaemonPidGuard {
    fn drop(&mut self) {
        let _ = remove_daemon_pid(&self.path);
    }
}

impl RuntimeLoadTracker {
    fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(SystemLoad::Unconstrained),
        }
    }

    fn current(&self) -> String {
        match self
            .state
            .lock()
            .map(|s| *s)
            .unwrap_or(SystemLoad::Conservative)
        {
            SystemLoad::Unconstrained => "unconstrained".to_string(),
            SystemLoad::Conservative => "conservative".to_string(),
            SystemLoad::Minimal => "minimal".to_string(),
        }
    }
}

impl LoadAware for RuntimeLoadTracker {
    fn on_load_change(&self, load: SystemLoad) {
        if let Ok(mut guard) = self.state.lock() {
            *guard = load;
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "ambient")]
#[command(about = "Ambient CLI", version)]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:7474")]
    server: String,
    #[arg(long, env = "AMBIENT_TOKEN")]
    token: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Watch,
    Query {
        text: String,
        #[arg(long)]
        pulse: bool,
        #[arg(long)]
        feedback: bool,
    },
    Ask {
        question: String,
    },
    Predict {
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
    Unit {
        id: Uuid,
        #[arg(long, default_value_t = 3600)]
        window: u64,
    },
    Related {
        id: Uuid,
        #[arg(long, default_value_t = 2)]
        depth: usize,
    },
    Status,
    Setup,
    Doctor,
    Checkin {
        #[arg(long)]
        energy: u8,
        #[arg(long)]
        mood: u8,
        #[arg(long)]
        note: Option<String>,
    },
    Feedback {
        #[arg(long)]
        query: String,
        #[arg(long)]
        unit_id: Uuid,
        #[arg(long)]
        useful: bool,
        #[arg(long)]
        ms_to_action: Option<u32>,
    },
    AskFeedback {
        #[arg(long)]
        query: String,
        #[arg(long)]
        unit_id: Uuid,
        #[arg(long)]
        useful: bool,
        #[arg(long)]
        ms_to_action: Option<u32>,
    },
    Export {
        #[arg(long, default_value = "json")]
        format: String,
    },
    OpenUrl {
        url: String,
    },
    CloudkitPush {
        #[arg(long, default_value = "cloudkit")]
        transport: String,
        payload_file: PathBuf,
    },
    Reindex,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Watch => {
            let config_path = default_config_path()?;
            let mut config =
                AmbientConfig::ensure_default(&config_path).map_err(|e| e.to_string())?;

            // Gap 4: first-run vault check — guide user through setup if no vault configured.
            if config.obsidian_vault.is_empty()
                || !std::path::Path::new(&config.obsidian_vault).exists()
            {
                eprintln!("⚠  No Obsidian vault configured. Running setup wizard...");
                eprintln!();
                let gate = Arc::new(InMemoryCapabilityGate::default());
                gate.seed_defaults();
                for line in run_setup(gate) {
                    println!("{line}");
                }
                eprintln!();

                // Re-read config after setup
                config = AmbientConfig::ensure_default(&config_path).map_err(|e| e.to_string())?;

                if config.obsidian_vault.is_empty()
                    || !std::path::Path::new(&config.obsidian_vault).exists()
                {
                    eprintln!("Setup did not configure a vault. Exiting.");
                    return Ok(());
                }
                eprintln!("Setup complete! Starting daemon...");
            }

            let daemon_pid_path = default_daemon_pid_path()?;
            let _pid_guard = DaemonPidGuard::create(daemon_pid_path)?;

            let backend = match config.reasoning_type.as_str() {
                "openai" => ambient_core::ReasoningBackend::OpenAI {
                    base_url: config.reasoning_url.clone(),
                    api_key: config.reasoning_api_key.clone(),
                },
                _ => ambient_core::ReasoningBackend::Local {
                    ollama_base_url: config.reasoning_url.clone(),
                },
            };

            let reasoning = Arc::new(
                RigReasoningEngine::new(backend)
                    .with_embedding_model(config.embedding_model.clone())
                    .with_completion_model(config.completion_model.clone()),
            );
            reasoning.start_ollama_probe();

            let (engine, store, index_store) = build_runtime_components_with_weights(
                config.semantic_weight,
                config.feedback_weight,
                Some(reasoning.clone()),
            )
            .map_err(|e| e.to_string())?;

            let dispatch = Arc::new(default_dispatch());
            let spotlight_exporter = Arc::new(CoreSpotlightExporter::default());
            let stream_provider = open_default_stream_provider().map_err(|e| e.to_string())?;

            let lens_consumer = Arc::new(
                LensConsumer::new(
                    stream_provider.clone(),
                    store.clone(),
                    index_store.clone(),
                    reasoning.clone(),
                    "lens_indexer".to_string(),
                )
                .map_err(|e| e.to_string())?,
            );
            lens_consumer.clone().start();

            let broadcaster = Arc::new(LoadBroadcaster::default());
            broadcaster.register(lens_consumer.clone() as Arc<dyn LoadAware>);
            let load_tracker = Arc::new(RuntimeLoadTracker::new());
            broadcaster.register(load_tracker.clone() as Arc<dyn LoadAware>);
            let system_load = Arc::new(SystemLoadSampler::new(
                Arc::new(DefaultLoadPolicyProvider),
                broadcaster.clone(),
            ));
            system_load
                .start()
                .map_err(|e: ambient_core::CoreError| e.to_string())?;

            let transport_registry =
                Arc::new(TransportRegistry::from_config(&config, broadcaster.clone()));
            let trigger_path = default_triggers_path()?;
            let triggers = Arc::new(
                TriggerEngine::load_from_path(store.clone(), &trigger_path)
                    .unwrap_or_else(|_| TriggerEngine::new(store.clone(), Vec::new())),
            );
            let _ = triggers.start_watch(trigger_path);
            let report_dir = default_report_dir()?;
            PatternScheduler::new(
                store.clone(),
                index_store.clone(),
                reasoning.clone(),
                report_dir,
            )
            .start();

            let feedback_recorder = Arc::new(ambient_patterns::ImplicitFeedbackRecorder::new(
                store.clone(),
            ));

            let mut active_sources = vec!["obsidian".to_string()];
            if config.spotlight {
                active_sources.push("spotlight".to_string());
            }

            let consume_provider = stream_provider.clone();
            let ingest_store = store.clone();
            let ingest_dispatch = dispatch.clone();
            let ingest_exporter = spotlight_exporter.clone();
            let ingest_triggers = triggers.clone();
            let upserted_units = Arc::new(AtomicU64::new(0));
            let ingest_upserted_units = upserted_units.clone();
            std::thread::spawn(move || {
                let consumer = match NormalizerConsumer::new(
                    consume_provider,
                    ingest_store,
                    ingest_dispatch,
                    "normalizer".to_string(),
                ) {
                    Ok(consumer) => consumer,
                    Err(_) => return,
                };

                loop {
                    let units = match consumer.poll_once(100) {
                        Ok(units) => units,
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(250));
                            continue;
                        }
                    };
                    if units.is_empty() {
                        std::thread::sleep(Duration::from_millis(250));
                        continue;
                    }

                    for unit in units {
                        ingest_upserted_units.fetch_add(1, Ordering::Relaxed);
                        let _ = ingest_exporter.export(&unit);
                        ingest_triggers.clone().on_upsert_background(unit);
                    }
                }
            });

            let mut active_samplers = Vec::new();
            if config.context_switches {
                active_samplers.push("context_switches".to_string());
            }
            if config.active_app_titles {
                active_samplers.push("active_app_titles".to_string());
            }
            if config.audio_input {
                active_samplers.push("audio_input".to_string());
            }
            let pulse_provider = stream_provider.clone();
            let pulse_store = store.clone();
            let feedback_recorder_pulse = feedback_recorder.clone();
            std::thread::spawn(move || {
                let consumer = match PulseConsumer::new(
                    pulse_provider,
                    pulse_store,
                    "pulse".to_string(),
                    Some(feedback_recorder_pulse),
                ) {
                    Ok(c) => c,
                    Err(_) => return,
                };
                loop {
                    let count = consumer.poll_once(200).unwrap_or(0);
                    if count == 0 {
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            });

            let addr: SocketAddr = format!("127.0.0.1:{}", config.http_port)
                .parse()
                .map_err(|e| format!("invalid server addr: {e}"))?;

            let menubar_gate = Arc::new(LocalLicenseGate::from_env());
            let menubar_controller = Arc::new(MenubarController::new(engine.clone(), menubar_gate));
            // Gap 2: register ambient:// URL scheme handler
            register_url_handler(menubar_controller.clone());
            let _menubar_runtime = if menubar_controller.startup_check() {
                start_native_runtime(menubar_controller).ok()
            } else {
                None
            };

            println!("ambient daemon listening on http://{addr}");
            let runtime = tokio::runtime::Runtime::new()
                .map_err(|e| format!("failed to create tokio runtime: {e}"))?;
            {
                let _guard = runtime.enter();
                transport_registry
                    .start_all(stream_provider)
                    .map_err(|e| format!("failed to start transport registry: {e}"))?;
            }
            let run_result = runtime.block_on(run_http_server(
                addr,
                HttpAppState {
                    engine,
                    store,
                    config: Arc::new(config.clone()),
                    auth_token: config.auth_token.clone(),
                    status_probe: Some(Arc::new(RuntimeStatusProbe {
                        reasoning,
                        load_tracker,
                        upserted_units,
                        active_sources,
                        active_samplers,
                        transport_registry: transport_registry.clone(),
                    })),
                    deep_link_focus: Arc::new(std::sync::Mutex::new(None)),
                    transport_registry: Some(transport_registry.clone()),
                    feedback_recorder: Some(feedback_recorder),
                },
            ));
            let stop_result = transport_registry.stop_all().map_err(|e| e.to_string());
            run_result.map_err(|e| e.to_string()).and(stop_result)
        }
        Commands::Query {
            text,
            pulse,
            feedback,
        } => {
            let body = serde_json::json!({
                "text": text,
                "k": 10,
                "include_pulse_context": pulse,
                "context_window_secs": 120
            });
            if feedback {
                query_with_feedback(&cli.server, cli.token.as_deref(), &text, body)
            } else {
                post_json(&cli.server, cli.token.as_deref(), "/query", &body)
            }
        }
        Commands::Ask { question } => {
            let body = serde_json::json!({
                "question": question,
                "k": 5
            });
            post_json(&cli.server, cli.token.as_deref(), "/ask", &body)
        }
        Commands::Predict { limit } => {
            let body = serde_json::json!({
                "limit": limit
            });
            post_json(&cli.server, cli.token.as_deref(), "/predict", &body)
        }
        Commands::Unit { id, window } => get(
            &cli.server,
            cli.token.as_deref(),
            &format!("/unit/{id}?context_window_secs={window}"),
        ),
        Commands::Related { id, depth } => get(
            &cli.server,
            cli.token.as_deref(),
            &format!("/related/{id}?depth={depth}"),
        ),
        Commands::Status => get(&cli.server, cli.token.as_deref(), "/health"),
        Commands::Setup => {
            let gate = Arc::new(InMemoryCapabilityGate::default());
            gate.seed_defaults();
            for line in run_setup(gate) {
                println!("{line}");
            }
            Ok(())
        }
        Commands::Doctor => {
            if let Some(lines) = fetch_transport_doctor(&cli.server, cli.token.as_deref())? {
                for line in lines {
                    println!("{line}");
                }
            } else {
                let gate = Arc::new(InMemoryCapabilityGate::default());
                gate.seed_defaults();
                for line in run_doctor(gate) {
                    println!("{line}");
                }
            }
            Ok(())
        }
        Commands::Checkin { energy, mood, note } => {
            let body = serde_json::json!({
                "energy": energy,
                "mood": mood,
                "note": note
            });
            post_json(&cli.server, cli.token.as_deref(), "/checkin", &body)
        }
        Commands::Feedback {
            query,
            unit_id,
            useful,
            ms_to_action,
        } => {
            let body = serde_json::json!({
                "query_text": query,
                "unit_id": unit_id,
                "useful": useful,
                "ms_to_action": ms_to_action
            });
            post_json(&cli.server, cli.token.as_deref(), "/feedback", &body)
        }
        Commands::AskFeedback {
            query,
            unit_id,
            useful,
            ms_to_action,
        } => {
            let body = serde_json::json!({
                "query_text": query,
                "unit_id": unit_id,
                "useful": useful,
                "ms_to_action": ms_to_action
            });
            post_json(&cli.server, cli.token.as_deref(), "/oracle-feedback", &body)
        }
        Commands::Export { format } => export_data(&cli.server, cli.token.as_deref(), &format),
        Commands::OpenUrl { url } => open_url(&cli.server, cli.token.as_deref(), &url),
        Commands::CloudkitPush {
            transport,
            payload_file,
        } => {
            let payload = std::fs::read_to_string(&payload_file).map_err(|e| {
                format!(
                    "failed reading payload file {}: {e}",
                    payload_file.display()
                )
            })?;
            post_raw_json(
                &cli.server,
                cli.token.as_deref(),
                &format!("/transport/{transport}/push"),
                payload,
            )
        }
        Commands::Reindex => {
            let stream_provider = open_default_stream_provider().map_err(|e| e.to_string())?;
            stream_provider
                .commit(&"lens_indexer".to_string(), 0)
                .map_err(|e: ambient_core::CoreError| e.to_string())?;
            println!("Lens indexer offset reset to 0. Restart the daemon to trigger re-indexing.");
            Ok(())
        }
    }
}

fn default_config_path() -> Result<PathBuf, String> {
    let home = env::var("HOME").map_err(|e| format!("failed to resolve HOME: {e}"))?;
    Ok(PathBuf::from(home).join(".ambient").join("config.toml"))
}

fn default_triggers_path() -> Result<PathBuf, String> {
    let home = env::var("HOME").map_err(|e| format!("failed to resolve HOME: {e}"))?;
    Ok(PathBuf::from(home).join(".ambient").join("triggers.json"))
}

fn default_report_dir() -> Result<PathBuf, String> {
    let home = env::var("HOME").map_err(|e| format!("failed to resolve HOME: {e}"))?;
    Ok(PathBuf::from(home).join(".ambient").join("reports"))
}

fn default_daemon_pid_path() -> Result<PathBuf, String> {
    let home = env::var("HOME").map_err(|e| format!("failed to resolve HOME: {e}"))?;
    Ok(PathBuf::from(home).join(".ambient").join("daemon.pid"))
}

fn write_daemon_pid(path: &PathBuf) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create daemon pid dir: {e}"))?;
    }
    let pid = std::process::id();
    std::fs::write(path, pid.to_string()).map_err(|e| format!("failed to write daemon pid: {e}"))
}

fn remove_daemon_pid(path: &PathBuf) -> Result<(), String> {
    if path.exists() {
        std::fs::remove_file(path).map_err(|e| format!("failed to remove daemon pid: {e}"))?;
    }
    Ok(())
}

fn get(server: &str, token: Option<&str>, path: &str) -> Result<(), String> {
    let url = format!("{}{}", server.trim_end_matches('/'), path);
    let client = Client::new();
    let mut req = client.get(url);
    if let Some(token) = token {
        req = req.header("X-Ambient-Token", token);
    }
    let response = req.send().map_err(|e| format!("request failed: {e}"))?;
    print_response(response)
}

fn post_json<T: Serialize>(
    server: &str,
    token: Option<&str>,
    path: &str,
    payload: &T,
) -> Result<(), String> {
    let url = format!("{}{}", server.trim_end_matches('/'), path);
    let client = Client::new();
    let mut req = client.post(url).json(payload);
    if let Some(token) = token {
        req = req.header("X-Ambient-Token", token);
    }
    let response = req.send().map_err(|e| format!("request failed: {e}"))?;
    print_response(response)
}

fn post_raw_json(
    server: &str,
    token: Option<&str>,
    path: &str,
    payload: String,
) -> Result<(), String> {
    let url = format!("{}{}", server.trim_end_matches('/'), path);
    let client = Client::new();
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .body(payload);
    if let Some(token) = token {
        req = req.header("X-Ambient-Token", token);
    }
    let response = req.send().map_err(|e| format!("request failed: {e}"))?;
    print_response(response)
}

fn print_response(response: reqwest::blocking::Response) -> Result<(), String> {
    let status = response.status();
    let body = response
        .text()
        .map_err(|e| format!("failed reading response: {e}"))?;
    if status.is_success() {
        println!("{body}");
        Ok(())
    } else {
        Err(format!("HTTP {}: {}", status.as_u16(), body))
    }
}

fn query_with_feedback(
    server: &str,
    token: Option<&str>,
    query_text: &str,
    body: serde_json::Value,
) -> Result<(), String> {
    let url = format!("{}{}", server.trim_end_matches('/'), "/query");
    let client = Client::new();
    let mut req = client.post(url).json(&body);
    if let Some(token) = token {
        req = req.header("X-Ambient-Token", token);
    }
    let response = req.send().map_err(|e| format!("request failed: {e}"))?;

    let status = response.status();
    let raw = response
        .text()
        .map_err(|e| format!("failed reading response: {e}"))?;
    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status.as_u16(), raw));
    }

    println!("{raw}");
    let parsed: Vec<QueryResult> =
        serde_json::from_str(&raw).map_err(|e| format!("invalid query response: {e}"))?;
    let Some(first) = parsed.first() else {
        return Ok(());
    };

    print!("useful? [y/N]: ");
    io::stdout()
        .flush()
        .map_err(|e| format!("stdout flush failed: {e}"))?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|e| format!("stdin read failed: {e}"))?;
    let useful = matches!(input.trim(), "y" | "Y" | "yes" | "YES");

    let feedback = serde_json::json!({
        "query_text": query_text,
        "unit_id": first.unit.id,
        "useful": useful,
        "ms_to_action": null
    });
    post_json(server, token, "/feedback", &feedback)
}

fn export_data(server: &str, token: Option<&str>, format: &str) -> Result<(), String> {
    if format != "json" {
        return Err(format!("unsupported export format: {format}"));
    }
    let body = serde_json::json!({
        "text": "",
        "k": 10000,
        "include_pulse_context": true,
        "context_window_secs": 120
    });
    let url = format!("{}{}", server.trim_end_matches('/'), "/query");
    let client = Client::new();
    let mut req = client.post(url).json(&body);
    if let Some(token) = token {
        req = req.header("X-Ambient-Token", token);
    }
    let response = req.send().map_err(|e| format!("request failed: {e}"))?;
    print_response(response)
}

fn open_url(server: &str, token: Option<&str>, url: &str) -> Result<(), String> {
    match parse_ambient_deep_link(url).map_err(|e| e.to_string())? {
        AmbientDeepLink::Unit(id) => {
            let route = format!("/open/unit/{id}");
            post_json(server, token, &route, &serde_json::json!({}))?;
            get(server, token, &format!("/unit/{id}"))
        }
    }
}

fn fetch_transport_doctor(
    server: &str,
    token: Option<&str>,
) -> Result<Option<Vec<String>>, String> {
    let url = format!("{}/health", server.trim_end_matches('/'));
    let client = Client::new();
    let mut req = client.get(url);
    if let Some(token) = token {
        req = req.header("X-Ambient-Token", token);
    }

    let Ok(resp) = req.send() else {
        return Ok(None);
    };
    if !resp.status().is_success() {
        return Ok(None);
    }
    let body = resp
        .text()
        .map_err(|e| format!("failed reading /health response: {e}"))?;
    let parsed: HealthResponse =
        serde_json::from_str(&body).map_err(|e| format!("invalid /health payload: {e}"))?;

    let mut out = Vec::new();
    out.push("Transport Status".to_string());
    out.push("────────────────────────────────────────────────────────────".to_string());
    if parsed.transport_statuses.is_empty() {
        out.push("No transport status reported".to_string());
        return Ok(Some(out));
    }

    for status in parsed.transport_statuses {
        let state = match status.state {
            TransportState::Active => "Active".to_string(),
            TransportState::Inactive => "Inactive".to_string(),
            TransportState::Degraded { reason } => format!("Degraded ({reason})"),
            TransportState::RequiresSetup { action } => format!("Requires setup ({action:?})"),
        };
        out.push(format!(
            "{}  {} events={} peers={}",
            status.transport_id,
            state,
            status.events_ingested,
            status.peers.len()
        ));
    }
    Ok(Some(out))
}
