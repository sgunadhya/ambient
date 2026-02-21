use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use ambient_core::{RawEvent, RawPayload, SourceId};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const LOOP_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_PORT: u16 = 17474;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecorderFrame {
    record_id: String,
    event: RawEvent,
}

fn main() {
    let mut interval_start = Utc::now();
    loop {
        if daemon_is_active() {
            interval_start = Utc::now();
            thread::sleep(LOOP_INTERVAL);
            continue;
        }

        let interval_end = Utc::now();
        let event = sample_behavior(interval_start, interval_end);
        interval_start = interval_end;

        let frame = RecorderFrame {
            record_id: Uuid::new_v4().to_string(),
            event,
        };

        if send_frame_loopback(&frame).is_ok() {
            let _ = flush_buffer_to_loopback();
        } else {
            let _ = append_buffer(&frame);
        }

        thread::sleep(LOOP_INTERVAL);
    }
}

fn sample_behavior(period_start: chrono::DateTime<Utc>, period_end: chrono::DateTime<Utc>) -> RawEvent {
    RawEvent {
        source: SourceId::new("ambient-recorder"),
        timestamp: period_end,
        payload: RawPayload::BehavioralSummary {
            period_start,
            period_end,
            avg_context_switch_rate: 0.0,
            peak_context_switch_rate: 0.0,
            audio_active_seconds: 0,
            dominant_app: None,
            app_switch_count: 0,
            was_in_meeting: false,
            was_in_focus_block: false,
            device_origin: "local-recorder".to_string(),
        },
    }
}

fn ambient_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".ambient"))
}

fn daemon_pid_path() -> Option<PathBuf> {
    ambient_home().map(|p| p.join("daemon.pid"))
}

fn buffer_path() -> Option<PathBuf> {
    ambient_home().map(|p| p.join("recorder-buffer.bin"))
}

fn daemon_is_active() -> bool {
    let Some(path) = daemon_pid_path() else {
        return false;
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(pid) = raw.trim().parse::<i32>() else {
        return false;
    };
    pid > 0 && std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .ok()
        .is_some_and(|out| out.status.success())
}

fn send_frame_loopback(frame: &RecorderFrame) -> Result<(), String> {
    let mut stream = TcpStream::connect(("127.0.0.1", DEFAULT_PORT))
        .map_err(|e| format!("connect failed: {e}"))?;
    let payload = serde_json::to_vec(frame).map_err(|e| format!("encode failed: {e}"))?;
    stream
        .write_all(&payload)
        .map_err(|e| format!("write failed: {e}"))?;
    stream.write_all(b"\n").map_err(|e| format!("write failed: {e}"))?;
    Ok(())
}

fn append_buffer(frame: &RecorderFrame) -> Result<(), String> {
    let Some(path) = buffer_path() else {
        return Err("HOME not available".to_string());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create dir failed: {e}"))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open buffer failed: {e}"))?;
    let line = serde_json::to_string(frame).map_err(|e| format!("encode failed: {e}"))?;
    writeln!(file, "{line}").map_err(|e| format!("write failed: {e}"))?;
    Ok(())
}

fn flush_buffer_to_loopback() -> Result<(), String> {
    let Some(path) = buffer_path() else {
        return Err("HOME not available".to_string());
    };
    if !path.exists() {
        return Ok(());
    }

    let file = OpenOptions::new()
        .read(true)
        .open(&path)
        .map_err(|e| format!("open buffer failed: {e}"))?;
    let mut sent = 0usize;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| format!("read buffer failed: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let frame: RecorderFrame =
            serde_json::from_str(&line).map_err(|e| format!("decode buffer failed: {e}"))?;
        send_frame_loopback(&frame)?;
        sent += 1;
    }

    if sent > 0 {
        let _ = fs::remove_file(path);
    }
    Ok(())
}
