use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ambient_core::{
    CoreError, RawEvent, RawPayload, Result, SourceId, StreamProvider, StreamTransport,
    TransportId, TransportState, TransportStatus,
};
use chrono::Utc;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

const DEBOUNCE_WINDOW_MS: u64 = 500;

pub struct ObsidianTransport {
    vault_path: PathBuf,
}

impl ObsidianTransport {
    pub fn new(vault_path: impl Into<PathBuf>) -> Self {
        Self {
            vault_path: vault_path.into(),
        }
    }

    fn is_eligible_markdown(path: &Path) -> bool {
        if path.extension().is_none_or(|ext| ext != "md") {
            return false;
        }

        if path
            .components()
            .any(|part| part.as_os_str().to_string_lossy().starts_with('.'))
        {
            return false;
        }

        !path
            .components()
            .any(|part| part.as_os_str().to_string_lossy() == ".obsidian")
    }

    fn emit_markdown(provider: &Arc<dyn StreamProvider>, path: PathBuf) {
        let Ok(content) = fs::read_to_string(&path) else {
            return;
        };

        let event = RawEvent {
            source: SourceId::new("obsidian"),
            timestamp: Utc::now(),
            payload: RawPayload::Markdown { content, path },
        };
        let _ = provider.append_raw(event);
    }

    fn should_emit(path: &Path, recently_emitted: &mut HashMap<PathBuf, Instant>) -> bool {
        let now = Instant::now();
        if recently_emitted.get(path).is_some_and(|last| {
            now.duration_since(*last) < Duration::from_millis(DEBOUNCE_WINDOW_MS)
        }) {
            return false;
        }
        recently_emitted.insert(path.to_path_buf(), now);
        true
    }

    fn handle_notify_event(
        event: Event,
        provider: &Arc<dyn StreamProvider>,
        recently_emitted: &mut HashMap<PathBuf, Instant>,
    ) {
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                for path in event.paths {
                    if !Self::is_eligible_markdown(&path) {
                        continue;
                    }
                    if Self::should_emit(&path, recently_emitted) {
                        Self::emit_markdown(provider, path);
                    }
                }
            }
            EventKind::Any | EventKind::Access(_) | EventKind::Remove(_) | EventKind::Other => {}
        }
    }

    fn scan_markdown_files(root: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with('.'))
                    || path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy() == ".obsidian")
                {
                    continue;
                }
                Self::scan_markdown_files(&path, out);
                continue;
            }
            if Self::is_eligible_markdown(&path) {
                out.push(path);
            }
        }
    }
}

use std::sync::Arc;

impl StreamTransport for ObsidianTransport {
    fn transport_id(&self) -> TransportId {
        "obsidian".to_string()
    }

    fn start(&self, provider: Arc<dyn StreamProvider>) -> Result<tokio::task::JoinHandle<()>> {
        let vault = self.vault_path.clone();

        thread::Builder::new()
            .name("obsidian-transport".to_string())
            .spawn(move || {
                let (notify_tx, notify_rx) = mpsc::channel();
                let mut watcher: RecommendedWatcher = match RecommendedWatcher::new(
                    move |res| {
                        let _ = notify_tx.send(res);
                    },
                    Config::default(),
                ) {
                    Ok(watcher) => watcher,
                    Err(_) => return,
                };

                if watcher.watch(&vault, RecursiveMode::Recursive).is_err() {
                    return;
                }

                let mut recently_emitted: HashMap<PathBuf, Instant> = HashMap::new();
                let mut initial_files = Vec::new();
                Self::scan_markdown_files(&vault, &mut initial_files);
                for path in initial_files {
                    if Self::should_emit(&path, &mut recently_emitted) {
                        Self::emit_markdown(&provider, path);
                    }
                }
                while let Ok(res) = notify_rx.recv() {
                    let Ok(event) = res else {
                        continue;
                    };
                    Self::handle_notify_event(event, &provider, &mut recently_emitted);
                }
            })
            .map_err(|e| CoreError::Internal(format!("failed to spawn obsidian transport: {e}")))?;

        let handle = tokio::runtime::Handle::try_current()
            .map_err(|e| CoreError::Internal(format!("tokio runtime unavailable: {e}")))?
            .spawn(async {});
        Ok(handle)
    }

    fn stop(&self) -> Result<()> {
        Ok(())
    }

    fn status(&self) -> TransportStatus {
        TransportStatus {
            transport_id: self.transport_id(),
            state: TransportState::Active,
            peers: Vec::new(),
            last_event_at: None,
            events_ingested: 0,
        }
    }
}
