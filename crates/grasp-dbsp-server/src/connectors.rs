//! Input connectors: what they report, and the read position a checkpoint
//! keeps for them.
//!
//! This module compiles without the `kafka` feature, so that `/stats`,
//! `/metadata` and checkpoints need no conditional code: a build without
//! connectors simply has none of them.
//!
//! **The read position is saved with the checkpoint, and nowhere else.** A
//! reader assigns its partitions itself and commits nothing to Kafka, as
//! Feldera's does (`adapters/src/transport/kafka/ft.rs:94-150`). The position
//! is taken on the circuit thread beside `prepare`, from the messages that
//! thread has pushed, so it describes exactly the records the circuit's state
//! holds: a resumed reader neither replays one nor skips one.

use serde::{Deserialize, Serialize};
use serde_json::{Value as J, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The file, inside the checkpoint's own directory, beside the materialized
/// views.
pub const FILE: &str = "connectors.json";

/// Where one connector stands in its topic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub topic: String,
    /// Per partition, the offset of the next message to read.
    pub partitions: BTreeMap<i32, i64>,
}

/// Every connector's position, by endpoint name.
pub type Positions = BTreeMap<String, Position>;

/// Counters a reader updates and `/stats` reads. Feldera's
/// `InputEndpointMetrics`, less what this server does not measure.
#[derive(Debug, Default)]
pub struct Metrics {
    pub total_records: AtomicU64,
    pub num_parse_errors: AtomicU64,
    pub num_transport_errors: AtomicU64,
    /// Why the reader stopped, if it did. The pipeline carries on without it.
    pub fatal_error: Mutex<Option<String>>,
}

/// One running connector, as the HTTP layer sees it.
#[derive(Debug, Clone)]
pub struct Connector {
    pub endpoint: String,
    pub table: String,
    pub topic: String,
    pub metrics: Arc<Metrics>,
}

impl Connector {
    /// Feldera's `InputEndpointStatus`, in its field names.
    pub fn stats(&self) -> J {
        let m = &self.metrics;
        json!({
            "endpoint_name": self.endpoint,
            "config": {"stream": self.table},
            "metrics": {
                "total_records": m.total_records.load(Ordering::Relaxed),
                "num_parse_errors": m.num_parse_errors.load(Ordering::Relaxed),
                "num_transport_errors": m.num_transport_errors.load(Ordering::Relaxed),
                "end_of_input": false,
            },
            "fatal_error": m.fatal_error.lock().ok().and_then(|f| f.clone()),
        })
    }
}

/// Writes every connector's position into `dir`, fsynced.
///
/// Always written, even with no connectors, so that a checkpoint that saved no
/// positions and one whose file is missing are different facts on resume.
pub fn save(dir: &Path, positions: &Positions) -> Result<(), String> {
    let path = dir.join(FILE);
    let text = serde_json::to_vec_pretty(positions)
        .map_err(|e| format!("encoding connector positions: {e}"))?;
    let mut file =
        std::fs::File::create(&path).map_err(|e| format!("writing `{}`: {e}", path.display()))?;
    file.write_all(&text)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("writing `{}`: {e}", path.display()))
}

/// The positions saved with checkpoint `uuid`, or `None` for a checkpoint
/// taken before this server had connectors.
pub fn load(root: &Path, uuid: &str) -> Result<Option<Positions>, String> {
    let path = root.join(uuid).join(FILE);
    match std::fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("reading `{}`: {e}", path.display())),
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("reading `{}`: {e}", path.display())),
    }
}
