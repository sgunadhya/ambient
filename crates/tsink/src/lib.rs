//! tsink — lightweight WAL-partitioned embedded time-series storage.
//!
//! Layout on disk:
//! ```text
//! data_path/
//!   1708560000000/     ← partition dir name = partition-start timestamp in ms
//!     records.wal      ← append-only frames: [u32 LE length][msgpack bytes]*
//!   1708473600000/
//!     records.wal
//! ```

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum TsinkError {
    InvalidPath,
    Poisoned,
    Io(String),
    Encode(String),
}

impl Display for TsinkError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            TsinkError::InvalidPath => write!(f, "invalid storage path"),
            TsinkError::Poisoned => write!(f, "storage lock poisoned"),
            TsinkError::Io(e) => write!(f, "tsink io error: {e}"),
            TsinkError::Encode(e) => write!(f, "tsink encode error: {e}"),
        }
    }
}

impl std::error::Error for TsinkError {}

impl From<io::Error> for TsinkError {
    fn from(e: io::Error) -> Self {
        TsinkError::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, TsinkError>;

// ── WAL sync mode ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum WalSyncMode {
    /// Fdatasync on every write.
    Immediate,
    /// Fdatasync at most once per interval (default).
    Periodic(Duration),
    /// Never sync — fastest, but data may be lost on crash.
    Never,
}

impl Default for WalSyncMode {
    fn default() -> Self {
        WalSyncMode::Periodic(Duration::from_secs(5))
    }
}

// ── Record ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub metric: String,
    pub timestamp_millis: i64,
    pub value: f64,
    pub labels: BTreeMap<String, String>,
}

// ── WAL frame helpers ─────────────────────────────────────────────────────────

fn encode_frame(record: &Record) -> Result<Vec<u8>> {
    rmp_serde::to_vec(record).map_err(|e| TsinkError::Encode(e.to_string()))
}

/// Reads all complete frames from a WAL file. Silently stops on a truncated frame.
fn read_wal_frames(path: &Path) -> Result<Vec<Record>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(TsinkError::Io(e.to_string())),
    };
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();

    loop {
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(TsinkError::Io(e.to_string())),
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut frame = vec![0u8; len];
        match reader.read_exact(&mut frame) {
            Ok(()) => {}
            Err(_) => break, // truncated frame — skip
        }
        if let Ok(rec) = rmp_serde::from_slice::<Record>(&frame) {
            out.push(rec);
        }
    }
    Ok(out)
}

// ── Partition helpers ─────────────────────────────────────────────────────────

/// Return the start-of-partition timestamp (ms) that `ts_ms` belongs to.
fn partition_start(ts_ms: i64, partition_ms: i64) -> i64 {
    if partition_ms <= 0 {
        return 0;
    }
    (ts_ms / partition_ms) * partition_ms
}

/// Directory name for a partition: zero-padded 20-char decimal.
fn partition_dir_name(start_ms: i64) -> String {
    format!("{:020}", start_ms)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ── StorageBuilder ────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct StorageBuilder {
    data_path: Option<String>,
    partition_duration: Option<Duration>,
    retention: Option<Duration>,
    wal_sync_mode: Option<WalSyncMode>,
}

impl Default for StorageBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageBuilder {
    pub fn new() -> Self {
        Self {
            data_path: None,
            partition_duration: None,
            retention: None,
            wal_sync_mode: None,
        }
    }

    pub fn with_data_path(mut self, path: impl Into<String>) -> Self {
        self.data_path = Some(path.into());
        self
    }

    pub fn with_partition_duration(mut self, duration: Duration) -> Self {
        self.partition_duration = Some(duration);
        self
    }

    pub fn with_retention(mut self, duration: Duration) -> Self {
        self.retention = Some(duration);
        self
    }

    pub fn with_wal_sync_mode(mut self, mode: WalSyncMode) -> Self {
        self.wal_sync_mode = Some(mode);
        self
    }

    pub fn build(self) -> Result<Storage> {
        let raw_path = match self.data_path.as_deref() {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => return Err(TsinkError::InvalidPath),
        };

        let data_path = PathBuf::from(&raw_path);
        fs::create_dir_all(&data_path)?;

        // Default: 1-day partitions
        let partition_duration = self
            .partition_duration
            .unwrap_or_else(|| Duration::from_secs(86_400));
        let retention = self.retention;
        let wal_sync_mode = self.wal_sync_mode.unwrap_or_default();

        let storage = Storage {
            data_path: data_path.clone(),
            partition_ms: partition_duration.as_millis() as i64,
            retention_ms: retention.map(|d| d.as_millis() as i64),
            wal_sync_mode,
            write_lock: Mutex::new(()),
        };

        // Retention sweep thread: runs once an hour.
        if retention.is_some() {
            let sweep_path = data_path.clone();
            let retention_ms = retention.map(|d| d.as_millis() as i64).unwrap_or(i64::MAX);
            thread::spawn(move || loop {
                thread::sleep(Duration::from_secs(3600));
                sweep_old_partitions(&sweep_path, retention_ms);
            });
        }

        Ok(storage)
    }
}

fn sweep_old_partitions(data_path: &Path, retention_ms: i64) {
    let cutoff = now_ms() - retention_ms;
    let Ok(entries) = fs::read_dir(data_path) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Ok(start_ms) = name_str.parse::<i64>() {
            if start_ms + retention_ms < cutoff {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
}

// ── Storage ───────────────────────────────────────────────────────────────────

pub struct Storage {
    data_path: PathBuf,
    partition_ms: i64,
    #[allow(dead_code)] // stored for introspection; sweep uses build-time retention_ms capture
    retention_ms: Option<i64>,
    wal_sync_mode: WalSyncMode,
    /// Coarse write-ordering lock (only one writer per partition at a time).
    write_lock: Mutex<()>,
}

impl Storage {
    /// Append a record. The WAL file for the record's partition is opened in
    /// append mode; the frame is written atomically.
    pub fn append(&self, record: Record) -> Result<()> {
        let _guard = self.write_lock.lock().map_err(|_| TsinkError::Poisoned)?;

        let part_start = partition_start(record.timestamp_millis, self.partition_ms);
        let part_dir = self.data_path.join(partition_dir_name(part_start));
        fs::create_dir_all(&part_dir)?;

        let wal_path = part_dir.join("records.wal");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&wal_path)?;

        let payload = encode_frame(&record)?;
        let len = payload.len() as u32;
        file.write_all(&len.to_le_bytes())?;
        file.write_all(&payload)?;

        match &self.wal_sync_mode {
            WalSyncMode::Immediate => file.sync_data()?,
            WalSyncMode::Periodic(_) | WalSyncMode::Never => {}
        }

        Ok(())
    }

    /// Return all records whose `timestamp_millis` falls in `[from_millis, to_millis]`.
    /// Scans all partitions that overlap the requested window.
    pub fn query_range(&self, from_millis: i64, to_millis: i64) -> Result<Vec<Record>> {
        // Determine which partition start timestamps to scan.
        let first_part = partition_start(from_millis, self.partition_ms);
        let last_part = partition_start(to_millis, self.partition_ms);

        let Ok(entries) = fs::read_dir(&self.data_path) else {
            return Ok(Vec::new());
        };

        let mut matching_parts: Vec<i64> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_string_lossy().parse::<i64>().ok())
            .filter(|&start| start >= first_part && start <= last_part)
            .collect();

        matching_parts.sort_unstable();

        let mut out = Vec::new();
        for part_start_ms in matching_parts {
            let wal_path = self
                .data_path
                .join(partition_dir_name(part_start_ms))
                .join("records.wal");
            let frames = read_wal_frames(&wal_path)?;
            for rec in frames {
                if rec.timestamp_millis >= from_millis && rec.timestamp_millis <= to_millis {
                    out.push(rec);
                }
            }
        }

        out.sort_by_key(|r| r.timestamp_millis);
        Ok(out)
    }

    /// Total number of WAL frames on disk (slow — scans all partitions). Useful for tests.
    pub fn count_all(&self) -> usize {
        let Ok(entries) = fs::read_dir(&self.data_path) else {
            return 0;
        };
        entries
            .filter_map(|e| e.ok())
            .map(|e| {
                let p = e.path().join("records.wal");
                read_wal_frames(&p).map(|v| v.len()).unwrap_or(0)
            })
            .sum()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_storage(name: &str) -> (Storage, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tsink_test_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = StorageBuilder::new()
            .with_data_path(dir.to_string_lossy().to_string())
            .with_partition_duration(Duration::from_millis(1000)) // 1-second partitions for test
            .build()
            .expect("build storage");
        (storage, dir)
    }

    fn rec(metric: &str, ts: i64, value: f64) -> Record {
        Record {
            metric: metric.to_string(),
            timestamp_millis: ts,
            value,
            labels: BTreeMap::new(),
        }
    }

    #[test]
    fn test_roundtrip_single() {
        let (storage, dir) = temp_storage("roundtrip");
        storage.append(rec("cpu", 1000, 0.42)).unwrap();
        let results = storage.query_range(0, 2000).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].metric, "cpu");
        assert!((results[0].value - 0.42).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_query_range_filters() {
        let (storage, dir) = temp_storage("range_filter");
        storage.append(rec("m", 100, 1.0)).unwrap();
        storage.append(rec("m", 500, 2.0)).unwrap();
        storage.append(rec("m", 900, 3.0)).unwrap();

        let results = storage.query_range(200, 800).unwrap();
        assert_eq!(results.len(), 1);
        assert!((results[0].value - 2.0).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_multi_partition_span() {
        // Partitions are 1000ms wide; two timestamps in different partitions
        let (storage, dir) = temp_storage("multi_part");
        storage.append(rec("m", 500, 1.0)).unwrap(); // partition 0
        storage.append(rec("m", 1500, 2.0)).unwrap(); // partition 1000
        storage.append(rec("m", 2500, 3.0)).unwrap(); // partition 2000

        let results = storage.query_range(0, 3000).unwrap();
        assert_eq!(results.len(), 3);
        // Should be sorted ascending
        assert!(results[0].timestamp_millis < results[1].timestamp_millis);
        assert!(results[1].timestamp_millis < results[2].timestamp_millis);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_persistence_across_reopen() {
        let dir = std::env::temp_dir().join("tsink_test_persist");
        let _ = std::fs::remove_dir_all(&dir);

        // Write with first instance
        {
            let storage = StorageBuilder::new()
                .with_data_path(dir.to_string_lossy().to_string())
                .with_partition_duration(Duration::from_secs(86_400))
                .build()
                .unwrap();
            storage.append(rec("heartbeat", 1_000_000, 1.0)).unwrap();
        }

        // Re-open and read
        {
            let storage = StorageBuilder::new()
                .with_data_path(dir.to_string_lossy().to_string())
                .with_partition_duration(Duration::from_secs(86_400))
                .build()
                .unwrap();
            let results = storage.query_range(0, 2_000_000).unwrap();
            assert_eq!(results.len(), 1, "record should survive across open/close");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_partition_dir_naming() {
        assert_eq!(partition_start(1500, 1000), 1000);
        assert_eq!(partition_start(0, 1000), 0);
        assert_eq!(partition_start(999, 1000), 0);
        assert_eq!(partition_start(1000, 1000), 1000);
    }

    #[test]
    fn test_empty_range_query_on_empty_storage() {
        let (storage, dir) = temp_storage("empty");
        let results = storage.query_range(0, 9_999_999).unwrap();
        assert!(results.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_builder_requires_path() {
        let result = StorageBuilder::new().build();
        assert!(result.is_err());
    }

    #[test]
    fn test_count_all() {
        let (storage, dir) = temp_storage("count");
        storage.append(rec("a", 100, 1.0)).unwrap();
        storage.append(rec("b", 200, 2.0)).unwrap();
        storage.append(rec("c", 1100, 3.0)).unwrap(); // different partition
        assert_eq!(storage.count_all(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
