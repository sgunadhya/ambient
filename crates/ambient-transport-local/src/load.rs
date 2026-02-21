use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use ambient_core::{CoreError, LoadAware, Result, SystemLoad};

pub trait LoadPolicyProvider: Send + Sync {
    fn sample(&self) -> LoadSample;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadSample {
    pub plugged_in: bool,
    pub battery_percent: f32,
    pub cpu_30s_avg: f32,
    pub user_active_last_60s: bool,
}

pub struct DefaultLoadPolicyProvider;

impl LoadPolicyProvider for DefaultLoadPolicyProvider {
    fn sample(&self) -> LoadSample {
        LoadSample {
            plugged_in: true,
            battery_percent: 100.0,
            cpu_30s_avg: 0.0,
            user_active_last_60s: false,
        }
    }
}

#[derive(Default)]
pub struct LoadBroadcaster {
    components: Mutex<Vec<Arc<dyn LoadAware>>>,
}

impl LoadBroadcaster {
    pub fn register(&self, component: Arc<dyn LoadAware>) {
        if let Ok(mut guard) = self.components.lock() {
            guard.push(component);
        }
    }

    pub fn broadcast(&self, load: SystemLoad) {
        if let Ok(guard) = self.components.lock() {
            for component in guard.iter() {
                component.on_load_change(load);
            }
        }
    }
}

pub struct SystemLoadSampler {
    provider: Arc<dyn LoadPolicyProvider>,
    broadcaster: Arc<LoadBroadcaster>,
    tick: Duration,
    started: AtomicBool,
}

impl SystemLoadSampler {
    pub fn new(provider: Arc<dyn LoadPolicyProvider>, broadcaster: Arc<LoadBroadcaster>) -> Self {
        Self {
            provider,
            broadcaster,
            tick: Duration::from_secs(30),
            started: AtomicBool::new(false),
        }
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    pub fn start(&self) -> Result<()> {
        if self.started.swap(true, Ordering::Relaxed) {
            return Ok(());
        }

        let provider = Arc::clone(&self.provider);
        let broadcaster = Arc::clone(&self.broadcaster);
        let tick = self.tick;

        thread::Builder::new()
            .name("system-load-sampler".to_string())
            .spawn(move || {
                let mut previous: Option<SystemLoad> = None;
                loop {
                    thread::sleep(tick);
                    let load = classify_system_load(provider.sample());
                    if previous != Some(load) {
                        broadcaster.broadcast(load);
                        previous = Some(load);
                    }
                }
            })
            .map_err(|e| CoreError::Internal(format!("failed to spawn load sampler: {e}")))?;
        Ok(())
    }
}

pub fn classify_system_load(sample: LoadSample) -> SystemLoad {
    if sample.battery_percent < 20.0 {
        return SystemLoad::Minimal;
    }

    if !sample.plugged_in || sample.cpu_30s_avg > 60.0 || sample.user_active_last_60s {
        return SystemLoad::Conservative;
    }

    if sample.plugged_in && sample.cpu_30s_avg < 40.0 {
        return SystemLoad::Unconstrained;
    }

    SystemLoad::Conservative
}
