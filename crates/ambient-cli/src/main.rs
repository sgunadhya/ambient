use std::process::ExitCode;
use std::sync::Arc;
use std::{env, net::SocketAddr, path::PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ambient_cli::{
    run_doctor, run_http_server, run_setup, start_pulse_pipeline, AmbientConfig, DaemonSamplers,
    DaemonStatusProbe, HttpAppState,
};
use ambient_core::{LoadAware, QueryResult, SourceAdapter, SpotlightExporter, SystemLoad};
use ambient_menubar::{parse_ambient_deep_link, AmbientDeepLink};
use ambient_normalizer::default_dispatch;
use ambient_onboard::InMemoryCapabilityGate;
use ambient_patterns::PatternScheduler;
use ambient_query::build_runtime_components_with_weights;
use ambient_reasoning::{EmbeddingQueue, RigReasoningEngine};
use ambient_spotlight::{CoreSpotlightExporter, SpotlightAdapter};
use ambient_triggers::TriggerEngine;
use ambient_watcher::{
    ActiveAppSampler, AudioInputSampler, ContextSwitchSampler, DefaultActiveAppProvider,
    DefaultLoadPolicyProvider, LoadBroadcaster, ObsidianAdapter, SystemLoadSampler,
    WatchAudioInputSource,
};
use clap::{Parser, Subcommand};
use reqwest::blocking::Client;
use serde::Serialize;
use std::io::{self, Write};
use uuid::Uuid;

struct RuntimeStatusProbe {
    reasoning: Arc<RigReasoningEngine>,
    embedding_queue: Arc<EmbeddingQueue>,
    load_tracker: Arc<RuntimeLoadTracker>,
    upserted_units: Arc<AtomicU64>,
    active_sources: Vec<String>,
    active_samplers: Vec<String>,
}

impl DaemonStatusProbe for RuntimeStatusProbe {
    fn embedding_available(&self) -> bool {
        self.reasoning.embedding_available()
    }

    fn load(&self) -> String {
        self.load_tracker.current()
    }

    fn queue_depth(&self) -> usize {
        self.embedding_queue.queue_depth()
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
}

struct RuntimeLoadTracker {
    state: std::sync::Mutex<SystemLoad>,
}

impl RuntimeLoadTracker {
    fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(SystemLoad::Unconstrained),
        }
    }

    fn current(&self) -> String {
        match self.state.lock().map(|s| *s).unwrap_or(SystemLoad::Conservative) {
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
    Export {
        #[arg(long, default_value = "json")]
        format: String,
    },
    OpenUrl {
        url: String,
    },
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
    let cli = Cli::parse();

    match cli.command {
        Commands::Watch => {
            let config_path = default_config_path()?;
            let config = AmbientConfig::ensure_default(&config_path).map_err(|e| e.to_string())?;
            let (engine, store) =
                build_runtime_components_with_weights(config.semantic_weight, config.feedback_weight)
                    .map_err(|e| e.to_string())?;

            let dispatch = Arc::new(default_dispatch());
            let spotlight_exporter = Arc::new(CoreSpotlightExporter::default());
            let reasoning = Arc::new(RigReasoningEngine::new(ambient_core::ReasoningBackend::Local {
                ollama_base_url: "http://localhost:11434".to_string(),
            }));
            reasoning.start_ollama_probe();
            let embedding_queue = Arc::new(EmbeddingQueue::new(store.clone(), reasoning.clone()));
            let trigger_path = default_triggers_path()?;
            let triggers = Arc::new(
                TriggerEngine::load_from_path(store.clone(), &trigger_path)
                    .unwrap_or_else(|_| TriggerEngine::new(store.clone(), Vec::new())),
            );
            let _ = triggers.start_watch(trigger_path);
            let report_dir = default_report_dir()?;
            PatternScheduler::new(store.clone(), report_dir).start();

            let (raw_tx, raw_rx) = std::sync::mpsc::channel();
            let mut source_handles = Vec::new();
            let mut active_sources = vec!["obsidian".to_string()];
            let obsidian = ObsidianAdapter::new(config.obsidian_vault.clone());
            source_handles.push(obsidian.watch(raw_tx.clone()).map_err(|e| e.to_string())?);

            if config.spotlight {
                let spotlight = SpotlightAdapter;
                source_handles.push(spotlight.watch(raw_tx.clone()).map_err(|e| e.to_string())?);
                active_sources.push("spotlight".to_string());
            }

            let ingest_store = store.clone();
            let ingest_dispatch = dispatch.clone();
            let ingest_exporter = spotlight_exporter.clone();
            let ingest_queue = embedding_queue.clone();
            let ingest_triggers = triggers.clone();
            let upserted_units = Arc::new(AtomicU64::new(0));
            let ingest_upserted_units = upserted_units.clone();
            std::thread::spawn(move || {
                for raw in raw_rx {
                    let Ok(unit) = ingest_dispatch.normalize(raw) else {
                        continue;
                    };
                    if ingest_store.upsert(unit.clone()).is_err() {
                        continue;
                    }
                    ingest_upserted_units.fetch_add(1, Ordering::Relaxed);
                    let _ = ingest_exporter.export(&unit);
                    let _ = ingest_queue.enqueue(unit.id, unit.content.clone());
                    ingest_triggers.clone().on_upsert_background(unit);
                }
            });

            let broadcaster = Arc::new(LoadBroadcaster::default());
            broadcaster.register(embedding_queue.clone() as Arc<dyn LoadAware>);
            let load_tracker = Arc::new(RuntimeLoadTracker::new());
            broadcaster.register(load_tracker.clone() as Arc<dyn LoadAware>);
            let system_load = Arc::new(SystemLoadSampler::new(
                Arc::new(DefaultLoadPolicyProvider),
                broadcaster.clone(),
            ));
            let active_provider = Arc::new(DefaultActiveAppProvider);
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
            let samplers = DaemonSamplers {
                context_switch: if config.context_switches {
                    Some(Arc::new(ContextSwitchSampler::new(active_provider.clone())))
                } else {
                    None
                },
                active_app: if config.active_app_titles {
                    Some(Arc::new(ActiveAppSampler::new(active_provider)))
                } else {
                    None
                },
                audio_input: if config.audio_input {
                    Some(Arc::new(AudioInputSampler::new(Arc::new(
                        WatchAudioInputSource::new(false),
                    ))))
                } else {
                    None
                },
            };
            let _pulse_handles = start_pulse_pipeline(store.clone(), samplers, system_load, broadcaster)
                .map_err(|e| e.to_string())?;

            let addr: SocketAddr = format!("127.0.0.1:{}", config.http_port)
                .parse()
                .map_err(|e| format!("invalid server addr: {e}"))?;

            println!("ambient daemon listening on http://{addr}");
            let _ = source_handles;
            let runtime = tokio::runtime::Runtime::new()
                .map_err(|e| format!("failed to create tokio runtime: {e}"))?;
            runtime
                .block_on(run_http_server(
                    addr,
                    HttpAppState {
                        engine,
                        store,
                        auth_token: config.auth_token.clone(),
                        status_probe: Some(Arc::new(RuntimeStatusProbe {
                            reasoning,
                            embedding_queue,
                            load_tracker,
                            upserted_units,
                            active_sources,
                            active_samplers,
                        })),
                        deep_link_focus: Arc::new(std::sync::Mutex::new(None)),
                    },
                ))
                .map_err(|e| e.to_string())
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
            let gate = Arc::new(InMemoryCapabilityGate::default());
            gate.seed_defaults();
            for line in run_doctor(gate) {
                println!("{line}");
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
        Commands::Export { format } => export_data(&cli.server, cli.token.as_deref(), &format),
        Commands::OpenUrl { url } => open_url(&cli.server, cli.token.as_deref(), &url),
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

fn print_response(response: reqwest::blocking::Response) -> Result<(), String> {
    let status = response.status();
    let body = response.text().map_err(|e| format!("failed reading response: {e}"))?;
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
