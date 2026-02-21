use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug)]
pub enum TsinkError {
    InvalidPath,
    Poisoned,
}

impl Display for TsinkError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            TsinkError::InvalidPath => write!(f, "invalid storage path"),
            TsinkError::Poisoned => write!(f, "storage lock poisoned"),
        }
    }
}

impl std::error::Error for TsinkError {}

pub type Result<T> = std::result::Result<T, TsinkError>;

#[derive(Debug, Clone)]
pub enum WalSyncMode {
    Periodic(Duration),
}

#[derive(Debug, Clone)]
pub struct Record {
    pub metric: String,
    pub timestamp_millis: i64,
    pub value: f64,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct StorageBuilder {
    data_path: Option<String>,
    partition_duration: Option<Duration>,
    retention: Option<Duration>,
    wal_sync_mode: Option<WalSyncMode>,
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
        if self
            .data_path
            .as_deref()
            .map(str::is_empty)
            .unwrap_or(true)
        {
            return Err(TsinkError::InvalidPath);
        }

        let _ = self.partition_duration;
        let _ = self.retention;
        let _ = self.wal_sync_mode;

        Ok(Storage {
            records: Mutex::new(Vec::new()),
        })
    }
}

#[derive(Debug, Default)]
pub struct Storage {
    records: Mutex<Vec<Record>>,
}

impl Storage {
    pub fn append(&self, record: Record) -> Result<()> {
        self.records
            .lock()
            .map_err(|_| TsinkError::Poisoned)?
            .push(record);
        Ok(())
    }

    pub fn query_range(&self, from_millis: i64, to_millis: i64) -> Result<Vec<Record>> {
        let mut items: Vec<Record> = self
            .records
            .lock()
            .map_err(|_| TsinkError::Poisoned)?
            .iter()
            .filter(|r| r.timestamp_millis >= from_millis && r.timestamp_millis <= to_millis)
            .cloned()
            .collect();
        items.sort_by_key(|r| r.timestamp_millis);
        Ok(items)
    }
}
