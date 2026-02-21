use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use ambient_core::{
    ConsumerId, CoreError, EventLogEntry, Offset, PulseEvent, RawEvent, Result, StreamProvider,
};
use chrono::Utc;
use sqlite::{Connection, ConnectionThreadSafe, State};
use tokio::sync::watch;

pub struct SqliteStreamProvider {
    conn: Mutex<ConnectionThreadSafe>,
    subscribers: Mutex<HashMap<ConsumerId, watch::Sender<Offset>>>,
}

impl SqliteStreamProvider {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| CoreError::Internal(format!("failed creating stream dir: {e}")))?;
        }

        let conn = Connection::open_thread_safe(path)
            .map_err(|e| CoreError::Internal(format!("failed opening stream db: {e}")))?;

        conn.execute("PRAGMA journal_mode=WAL")
            .map_err(|e| CoreError::Internal(format!("failed enabling WAL mode: {e}")))?;

        conn.execute(
            r#"
CREATE TABLE IF NOT EXISTS event_log (
    offset      INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type  TEXT    NOT NULL,
    transport   TEXT,
    source_id   TEXT    NOT NULL,
    record_id   TEXT,
    timestamp   REAL    NOT NULL,
    arrived_at  REAL    NOT NULL,
    payload     BLOB    NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS event_log_record_id
    ON event_log (record_id)
    WHERE record_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS event_log_timestamp ON event_log (timestamp);
CREATE INDEX IF NOT EXISTS event_log_source    ON event_log (source_id, offset);

CREATE TABLE IF NOT EXISTS consumer_offsets (
    consumer_id TEXT    PRIMARY KEY,
    last_offset INTEGER NOT NULL,
    updated_at  REAL    NOT NULL
);
"#,
        )
        .map_err(|e| CoreError::Internal(format!("failed initializing stream schema: {e}")))?;

        Ok(Self {
            conn: Mutex::new(conn),
            subscribers: Mutex::new(HashMap::new()),
        })
    }

    fn append_entry(
        &self,
        entry: &EventLogEntry,
        event_type: &str,
        source_id: &str,
        transport: Option<&str>,
        record_id: Option<&str>,
        timestamp: f64,
    ) -> Result<Offset> {
        let payload = serde_json::to_vec(entry)
            .map_err(|e| CoreError::Internal(format!("failed encoding event log entry: {e}")))?;
        let arrived_at = Utc::now().timestamp_millis() as f64 / 1000.0;

        let conn = self
            .conn
            .lock()
            .map_err(|_| CoreError::Internal("stream conn lock poisoned".to_string()))?;

        let mut statement = conn
            .prepare(
                "INSERT OR IGNORE INTO event_log (event_type, transport, source_id, record_id, timestamp, arrived_at, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .map_err(|e| CoreError::Internal(format!("failed preparing append statement: {e}")))?;
        statement
            .bind((1, event_type))
            .and_then(|_| statement.bind((2, transport)))
            .and_then(|_| statement.bind((3, source_id)))
            .and_then(|_| statement.bind((4, record_id)))
            .and_then(|_| statement.bind((5, timestamp)))
            .and_then(|_| statement.bind((6, arrived_at)))
            .and_then(|_| statement.bind((7, payload.as_slice())))
            .map_err(|e| CoreError::Internal(format!("failed binding append params: {e}")))?;
        let _ = statement
            .next()
            .map_err(|e| CoreError::Internal(format!("failed executing append: {e}")))?;

        let changes = conn.change_count();
        let offset = if changes == 0 {
            let rec = record_id.ok_or_else(|| {
                CoreError::Internal("record_id required for deduplicated append".to_string())
            })?;
            let mut lookup = conn
                .prepare(
                    "SELECT offset FROM event_log WHERE record_id = ?1 ORDER BY offset DESC LIMIT 1",
                )
                .map_err(|e| CoreError::Internal(format!("failed preparing dedup lookup: {e}")))?;
            lookup
                .bind((1, rec))
                .map_err(|e| CoreError::Internal(format!("failed binding dedup lookup: {e}")))?;
            match lookup
                .next()
                .map_err(|e| CoreError::Internal(format!("failed querying dedup lookup: {e}")))?
            {
                State::Row => lookup
                    .read::<i64, _>(0)
                    .map_err(|e| CoreError::Internal(format!("failed reading dedup offset: {e}")))?
                    as Offset,
                State::Done => {
                    return Err(CoreError::Internal(
                        "dedup lookup expected existing record_id".to_string(),
                    ));
                }
            }
        } else {
            let mut last = conn
                .prepare("SELECT last_insert_rowid()")
                .map_err(|e| CoreError::Internal(format!("failed preparing last row id query: {e}")))?;
            match last
                .next()
                .map_err(|e| CoreError::Internal(format!("failed querying last row id: {e}")))?
            {
                State::Row => last
                    .read::<i64, _>(0)
                    .map_err(|e| CoreError::Internal(format!("failed reading last row id: {e}")))?
                    as Offset,
                State::Done => {
                    return Err(CoreError::Internal(
                        "last_insert_rowid returned no rows".to_string(),
                    ));
                }
            }
        };

        if changes > 0 {
            self.notify_subscribers(offset);
        }
        Ok(offset)
    }

    fn notify_subscribers(&self, offset: Offset) {
        let subscribers = match self.subscribers.lock() {
            Ok(guard) => guard.values().cloned().collect::<Vec<_>>(),
            Err(_) => return,
        };

        for tx in subscribers {
            let _ = tx.send(offset);
        }
    }

    #[cfg(test)]
    fn journal_mode(&self) -> Result<String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CoreError::Internal("stream conn lock poisoned".to_string()))?;
        let mut stmt = conn
            .prepare("PRAGMA journal_mode")
            .map_err(|e| CoreError::Internal(format!("failed preparing journal mode query: {e}")))?;
        match stmt
            .next()
            .map_err(|e| CoreError::Internal(format!("failed querying journal mode: {e}")))?
        {
            State::Row => stmt
                .read::<String, _>(0)
                .map_err(|e| CoreError::Internal(format!("failed reading journal mode: {e}"))),
            State::Done => Err(CoreError::Internal(
                "journal mode query returned no rows".to_string(),
            )),
        }
    }
}

impl StreamProvider for SqliteStreamProvider {
    fn append_raw(&self, event: RawEvent) -> Result<Offset> {
        let timestamp = event.timestamp.timestamp_millis() as f64 / 1000.0;
        let source_id = event.source.0.clone();
        let entry = EventLogEntry::Raw(event);
        self.append_entry(&entry, "raw", &source_id, None, None, timestamp)
    }

    fn append_pulse(&self, event: PulseEvent) -> Result<Offset> {
        let timestamp = event.timestamp.timestamp_millis() as f64 / 1000.0;
        let entry = EventLogEntry::Pulse(event);
        self.append_entry(&entry, "pulse", "pulse", None, None, timestamp)
    }

    fn append_raw_from(&self, event: RawEvent, transport: &str, record_id: &str) -> Result<Offset> {
        let timestamp = event.timestamp.timestamp_millis() as f64 / 1000.0;
        let source_id = event.source.0.clone();
        let entry = EventLogEntry::Raw(event);
        self.append_entry(
            &entry,
            "raw",
            &source_id,
            Some(transport),
            Some(record_id),
            timestamp,
        )
    }

    fn read(
        &self,
        _consumer: &ConsumerId,
        from: Offset,
        limit: usize,
    ) -> Result<Vec<(Offset, EventLogEntry)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CoreError::Internal("stream conn lock poisoned".to_string()))?;
        let mut statement = conn
            .prepare(
                "SELECT offset, payload FROM event_log WHERE offset > ?1 ORDER BY offset ASC LIMIT ?2",
            )
            .map_err(|e| CoreError::Internal(format!("failed preparing read: {e}")))?;
        statement
            .bind((1, from as i64))
            .and_then(|_| statement.bind((2, limit as i64)))
            .map_err(|e| CoreError::Internal(format!("failed binding read params: {e}")))?;

        let mut out = Vec::new();
        loop {
            match statement
                .next()
                .map_err(|e| CoreError::Internal(format!("failed querying read: {e}")))?
            {
                State::Done => break,
                State::Row => {
                    let offset = statement
                        .read::<i64, _>(0)
                        .map_err(|e| CoreError::Internal(format!("failed reading offset: {e}")))?;
                    let payload = statement
                        .read::<Vec<u8>, _>(1)
                        .map_err(|e| CoreError::Internal(format!("failed reading payload: {e}")))?;
                    let entry: EventLogEntry = serde_json::from_slice(&payload).map_err(|e| {
                        CoreError::Internal(format!("failed decoding event payload: {e}"))
                    })?;
                    out.push((offset as Offset, entry));
                }
            }
        }

        Ok(out)
    }

    fn commit(&self, consumer: &ConsumerId, offset: Offset) -> Result<()> {
        let updated_at = Utc::now().timestamp_millis() as f64 / 1000.0;
        let conn = self
            .conn
            .lock()
            .map_err(|_| CoreError::Internal("stream conn lock poisoned".to_string()))?;
        let mut statement = conn
            .prepare(
                "INSERT INTO consumer_offsets (consumer_id, last_offset, updated_at) VALUES (?1, ?2, ?3) ON CONFLICT(consumer_id) DO UPDATE SET last_offset = excluded.last_offset, updated_at = excluded.updated_at",
            )
            .map_err(|e| CoreError::Internal(format!("failed preparing commit statement: {e}")))?;
        statement
            .bind((1, consumer.as_str()))
            .and_then(|_| statement.bind((2, offset as i64)))
            .and_then(|_| statement.bind((3, updated_at)))
            .map_err(|e| CoreError::Internal(format!("failed binding commit params: {e}")))?;
        let _ = statement
            .next()
            .map_err(|e| CoreError::Internal(format!("failed executing commit: {e}")))?;
        Ok(())
    }

    fn last_committed(&self, consumer: &ConsumerId) -> Result<Option<Offset>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CoreError::Internal("stream conn lock poisoned".to_string()))?;
        let mut statement = conn
            .prepare("SELECT last_offset FROM consumer_offsets WHERE consumer_id = ?1")
            .map_err(|e| CoreError::Internal(format!("failed preparing last_committed query: {e}")))?;
        statement
            .bind((1, consumer.as_str()))
            .map_err(|e| CoreError::Internal(format!("failed binding last_committed: {e}")))?;
        match statement
            .next()
            .map_err(|e| CoreError::Internal(format!("failed querying last_committed: {e}")))?
        {
            State::Done => Ok(None),
            State::Row => {
                let offset = statement
                    .read::<i64, _>(0)
                    .map_err(|e| CoreError::Internal(format!("failed reading committed offset: {e}")))?;
                Ok(Some(offset as Offset))
            }
        }
    }

    fn subscribe(&self, consumer: &ConsumerId, tx: watch::Sender<Offset>) -> Result<()> {
        self.subscribers
            .lock()
            .map_err(|_| CoreError::Internal("subscribers lock poisoned".to_string()))?
            .insert(consumer.clone(), tx);
        Ok(())
    }
}

#[cfg(test)]
pub mod memory {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use ambient_core::{
        ConsumerId, CoreError, EventLogEntry, Offset, PulseEvent, RawEvent, Result, StreamProvider,
    };
    use tokio::sync::watch;

    #[derive(Default)]
    pub struct InMemoryStreamProvider {
        entries: Mutex<Vec<(Offset, EventLogEntry)>>,
        record_ids: Mutex<HashMap<String, Offset>>,
        committed: Mutex<HashMap<ConsumerId, Offset>>,
        subscribers: Mutex<HashMap<ConsumerId, watch::Sender<Offset>>>,
    }

    impl StreamProvider for InMemoryStreamProvider {
        fn append_raw(&self, event: RawEvent) -> Result<Offset> {
            let mut guard = self
                .entries
                .lock()
                .map_err(|_| CoreError::Internal("entries lock poisoned".to_string()))?;
            let offset = (guard.len() as Offset) + 1;
            guard.push((offset, EventLogEntry::Raw(event)));
            drop(guard);
            self.notify(offset);
            Ok(offset)
        }

        fn append_pulse(&self, event: PulseEvent) -> Result<Offset> {
            let mut guard = self
                .entries
                .lock()
                .map_err(|_| CoreError::Internal("entries lock poisoned".to_string()))?;
            let offset = (guard.len() as Offset) + 1;
            guard.push((offset, EventLogEntry::Pulse(event)));
            drop(guard);
            self.notify(offset);
            Ok(offset)
        }

        fn append_raw_from(
            &self,
            event: RawEvent,
            _transport: &str,
            record_id: &str,
        ) -> Result<Offset> {
            if let Some(offset) = self
                .record_ids
                .lock()
                .map_err(|_| CoreError::Internal("record_ids lock poisoned".to_string()))?
                .get(record_id)
                .copied()
            {
                return Ok(offset);
            }
            let offset = self.append_raw(event)?;
            self.record_ids
                .lock()
                .map_err(|_| CoreError::Internal("record_ids lock poisoned".to_string()))?
                .insert(record_id.to_string(), offset);
            Ok(offset)
        }

        fn read(
            &self,
            _consumer: &ConsumerId,
            from: Offset,
            limit: usize,
        ) -> Result<Vec<(Offset, EventLogEntry)>> {
            let entries = self
                .entries
                .lock()
                .map_err(|_| CoreError::Internal("entries lock poisoned".to_string()))?;
            Ok(entries
                .iter()
                .filter(|(offset, _)| *offset > from)
                .take(limit)
                .cloned()
                .collect())
        }

        fn commit(&self, consumer: &ConsumerId, offset: Offset) -> Result<()> {
            self.committed
                .lock()
                .map_err(|_| CoreError::Internal("committed lock poisoned".to_string()))?
                .insert(consumer.clone(), offset);
            Ok(())
        }

        fn last_committed(&self, consumer: &ConsumerId) -> Result<Option<Offset>> {
            Ok(self
                .committed
                .lock()
                .map_err(|_| CoreError::Internal("committed lock poisoned".to_string()))?
                .get(consumer)
                .copied())
        }

        fn subscribe(&self, consumer: &ConsumerId, tx: watch::Sender<Offset>) -> Result<()> {
            self.subscribers
                .lock()
                .map_err(|_| CoreError::Internal("subscribers lock poisoned".to_string()))?
                .insert(consumer.clone(), tx);
            Ok(())
        }
    }

    impl InMemoryStreamProvider {
        fn notify(&self, offset: Offset) {
            let subscribers = match self.subscribers.lock() {
                Ok(guard) => guard.values().cloned().collect::<Vec<_>>(),
                Err(_) => return,
            };
            for tx in subscribers {
                let _ = tx.send(offset);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ambient_core::{RawEvent, RawPayload, SourceId, StreamProvider};
    use chrono::Utc;
    use tempfile::TempDir;

    use super::SqliteStreamProvider;

    fn temp_db_path(tmp: &TempDir) -> PathBuf {
        tmp.path().join("stream.db")
    }

    #[test]
    fn stream_roundtrip_commit_and_resume() {
        let tmp = TempDir::new().expect("tmpdir");
        let provider = SqliteStreamProvider::open(&temp_db_path(&tmp)).expect("open provider");

        let e1 = RawEvent {
            source: SourceId::new("obsidian"),
            timestamp: Utc::now(),
            payload: RawPayload::PlainText {
                content: "one".to_string(),
                path: PathBuf::from("one.txt"),
            },
        };
        let e2 = RawEvent {
            source: SourceId::new("obsidian"),
            timestamp: Utc::now(),
            payload: RawPayload::PlainText {
                content: "two".to_string(),
                path: PathBuf::from("two.txt"),
            },
        };

        let o1 = provider.append_raw(e1).expect("append1");
        let o2 = provider.append_raw(e2).expect("append2");
        assert!(o2 > o1);

        let consumer = "normalizer".to_string();
        let batch = provider.read(&consumer, 0, 100).expect("read");
        assert_eq!(batch.len(), 2);

        provider.commit(&consumer, batch[0].0).expect("commit");
        let last = provider.last_committed(&consumer).expect("last committed");
        assert_eq!(last, Some(batch[0].0));

        let resumed = provider.read(&consumer, last.unwrap_or(0), 100).expect("resume read");
        assert_eq!(resumed.len(), 1);
    }

    #[test]
    fn dedup_by_record_id_keeps_single_row() {
        let tmp = TempDir::new().expect("tmpdir");
        let provider = SqliteStreamProvider::open(&temp_db_path(&tmp)).expect("open provider");

        let event = RawEvent {
            source: SourceId::new("spotlight"),
            timestamp: Utc::now(),
            payload: RawPayload::PlainText {
                content: "same".to_string(),
                path: PathBuf::from("same.txt"),
            },
        };

        let a = provider
            .append_raw_from(event.clone(), "bonjour", "peer-1/42")
            .expect("append a");
        let b = provider
            .append_raw_from(event, "cloudkit", "peer-1/42")
            .expect("append b");
        assert_eq!(a, b);

        let consumer = "normalizer".to_string();
        let batch = provider.read(&consumer, 0, 100).expect("read");
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn sqlite_uses_wal_mode() {
        let tmp = TempDir::new().expect("tmpdir");
        let provider = SqliteStreamProvider::open(&temp_db_path(&tmp)).expect("open provider");
        let mode = provider.journal_mode().expect("journal mode");
        assert_eq!(mode.to_lowercase(), "wal");
    }
}
