use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use ambient_core::{CoreError, Result};

const STATE_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CloudKitTokenState {
    pub version: u8,
    pub change_token: Option<String>,
    pub events_ingested: u64,
    pub last_event_at: Option<DateTime<Utc>>,
}

impl CloudKitTokenState {
    pub fn new(
        change_token: Option<String>,
        events_ingested: u64,
        last_event_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            version: STATE_VERSION,
            change_token,
            events_ingested,
            last_event_at,
        }
    }
}

pub fn encode(state: &CloudKitTokenState) -> Result<Vec<u8>> {
    bincode::serialize(state).map_err(|e| CoreError::Internal(format!("cloudkit state encode failed: {e}")))
}

pub fn decode(bytes: &[u8]) -> Result<CloudKitTokenState> {
    let state: CloudKitTokenState = bincode::deserialize(bytes).map_err(|e| {
        CoreError::InvalidInput(format!("cloudkit state decode failed, resetting token: {e}"))
    })?;
    if state.version != STATE_VERSION {
        return Err(CoreError::InvalidInput(format!(
            "cloudkit state version mismatch: expected {}, got {}",
            STATE_VERSION, state.version
        )));
    }
    Ok(state)
}
