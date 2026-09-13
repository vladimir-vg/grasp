//! What a checkpoint has to record beyond the circuit's own state.
//!
//! `dbsp` checkpoints operator state and nothing else. Everything an embedder
//! needs to know *about* a checkpoint — what it was written by, and whether
//! this process may read it — is the embedder's own problem, which is why
//! Feldera writes a `state.json` of its own into the same directory
//! (`adapters/src/controller/checkpoint.rs:34`). This is that file, and it
//! lives in this crate rather than in the server because the rules it enforces
//! are properties of the runtime: another host embedding `grasp-dbsp` should
//! get the same refusals without reimplementing them.
//!
//! **Three things `dbsp` does not check**, each of which corrupts silently
//! rather than failing, and each of which is one field here:
//!
//! 1. **The worker count.** Every operator's state file is named
//!    `{worker_index}-…` (`dbsp/src/circuit/circuit_builder.rs:992-1003`) and
//!    every batch file `w{n}-…` (`dbsp/src/storage/file/writer.rs:1161`).
//!    Restore at a different count and the surviving shards still hold data
//!    placed by the old count's hash, with no error anywhere. Feldera avoids
//!    this by taking the worker count *from* the checkpoint and ignoring what
//!    the operator asked for (`adapters/src/controller.rs:5863-5866`); this
//!    refuses instead, because silently overriding a number someone wrote in a
//!    file is its own surprise.
//! 2. **The archived value format.** See [`crate::value::DynValue::format_digest`].
//! 3. **Which program wrote it.** `dbsp` has a fingerprint, but it is FNV over
//!    node *type names* in traversal order
//!    (`dbsp/src/circuit/circuit_builder.rs:8788`), so two different programs
//!    built from the same operators collide. This crate can do better: content
//!    ids are derived from the program itself, so hashing them identifies the
//!    computation and not merely its shape.

use crate::diag::{Diagnostic, Pass};
use crate::value::DynValue;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use xxhash_rust::xxh3::Xxh3Default;

/// The file this module writes, inside `dbsp`'s own checkpoint directory.
pub const MANIFEST: &str = "grasp.json";

/// A digest of the computation a circuit performs.
///
/// The content ids are already a hash of each node's operator, its arguments
/// and its inputs, computed from the program rather than from anything
/// positional (`crate::typecheck::content_ids`). Hashing them in plan order
/// therefore identifies the whole program.
///
/// The selected outputs go in too, and they are not a detail: selecting a node
/// adds an accumulator and a sink to the circuit, so two runs of the same
/// program observing different nodes build different graphs — and `dbsp`
/// restores by position, so the difference matters.
pub fn program_digest(ids: &[String], outputs: &[String]) -> u64 {
    use std::hash::Hasher;
    let mut h = Xxh3Default::new();
    h.write_u64(ids.len() as u64);
    for id in ids {
        h.write(id.as_bytes());
        h.write_u8(0xff);
    }
    // Sorted, because the order outputs were named in is the caller's business
    // and does not change the circuit — only the set does.
    let mut sorted: Vec<&String> = outputs.iter().collect();
    sorted.sort();
    h.write_u64(sorted.len() as u64);
    for name in sorted {
        h.write(name.as_bytes());
        h.write_u8(0xfe);
    }
    h.finish()
}

/// What this crate records about a checkpoint it wrote.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    /// `dbsp`'s checkpoint uuid, which is also the directory name.
    pub uuid: String,
    /// The worker count the circuit ran at.
    pub workers: usize,
    /// [`DynValue::format_digest`] at the time of writing.
    pub format_digest: u64,
    /// [`program_digest`] for the program and output selection.
    pub program_digest: u64,
    /// Completed transactions at the time of writing. Informational.
    pub steps: u64,
    /// Seconds since the Unix epoch. Informational, and it is what makes a
    /// directory listing legible to a person.
    pub created: u64,
    /// The next offset for every partition of every partitioned input, so that
    /// a restored circuit continues numbering where this one stopped rather
    /// than reissuing offsets its history already holds.
    ///
    /// `None` in a manifest written before offsets existed. It is optional
    /// rather than defaulted to empty because an empty map means "no partition
    /// has seen a record", and that manifest does not know.
    #[serde(default)]
    pub offsets: Option<BTreeMap<String, BTreeMap<i64, i64>>>,
}

impl Manifest {
    pub fn new(
        uuid: String,
        workers: usize,
        program_digest: u64,
        steps: u64,
        offsets: BTreeMap<String, BTreeMap<i64, i64>>,
    ) -> Manifest {
        Manifest {
            uuid,
            workers,
            format_digest: DynValue::format_digest(),
            program_digest,
            steps,
            offsets: Some(offsets),
            created: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }

    /// `<storage root>/<uuid>/grasp.json`, beside the `CHECKPOINT` marker
    /// `dbsp` writes and the `state.json` Feldera writes.
    pub fn path(root: &Path, uuid: &str) -> PathBuf {
        root.join(uuid).join(MANIFEST)
    }

    pub fn write(&self, root: &Path) -> Result<(), Vec<Diagnostic>> {
        let path = Manifest::path(root, &self.uuid);
        let text = serde_json::to_string_pretty(self).map_err(|e| {
            vec![Diagnostic::error(
                Pass::Config,
                None,
                format!("encoding the checkpoint manifest: {e}"),
            )]
        })?;
        std::fs::write(&path, text).map_err(|e| {
            vec![Diagnostic::error(
                Pass::Config,
                None,
                format!("writing `{}`: {e}", path.display()),
            )]
        })
    }

    /// Reads the manifest for a checkpoint, or says why it cannot be trusted.
    ///
    /// A missing manifest is refused rather than waved through. A checkpoint
    /// directory this crate did not write is one it cannot vouch for: none of
    /// the three checks below can be made, and `dbsp` will not make them.
    pub fn read(root: &Path, uuid: &str) -> Result<Manifest, Vec<Diagnostic>> {
        let path = Manifest::path(root, uuid);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            vec![Diagnostic::error(
                Pass::Config,
                None,
                format!(
                    "reading `{}`: {e}. Every checkpoint this runtime takes has one \
                     beside `dbsp`'s own state, and a checkpoint without one cannot be \
                     checked for the worker count, the value format or the program it \
                     was written by — so it is refused rather than restored blind.",
                    path.display()
                ),
            )]
        })?;
        serde_json::from_str(&text).map_err(|e| {
            vec![Diagnostic::error(
                Pass::Config,
                None,
                format!("reading `{}`: {e}", path.display()),
            )]
        })
    }

    /// Whether this checkpoint may be restored into the circuit described.
    ///
    /// One diagnostic per mismatch rather than the first: a checkpoint from an
    /// older build can easily differ in more than one way, and being told about
    /// them one restart at a time is the worse experience.
    pub fn check(&self, workers: usize, program_digest: u64) -> Result<(), Vec<Diagnostic>> {
        let mut diags = Vec::new();

        if self.workers != workers {
            diags.push(Diagnostic::error(
                Pass::Config,
                None,
                format!(
                    "this checkpoint was written at {} worker(s) and this circuit has {}. \
                     A checkpoint is sharded by worker — every state file is named for the \
                     worker that wrote it — so restoring it at another count would hand \
                     each worker rows that a different one is now responsible for. Set \
                     `workers: {}` to restore it.",
                    self.workers, workers, self.workers
                ),
            ));
        }

        if self.format_digest != DynValue::format_digest() {
            diags.push(Diagnostic::error(
                Pass::Config,
                None,
                "this checkpoint was written by a build whose value format differs from \
                 this one's: the set or the order of `DynValue`'s variants changed, and \
                 that order is the discriminant every stored value carries. Nothing would \
                 report a mismatch while reading — a value would simply come back as a \
                 different variant — so it is refused here instead."
                    .to_string(),
            ));
        }

        if self.program_digest != program_digest {
            diags.push(Diagnostic::error(
                Pass::Config,
                None,
                "this checkpoint was written by a different program, or by the same one \
                 observing different outputs. State is restored into the circuit by \
                 position, so a program that does not match is not a partial restore but \
                 a wrong one."
                    .to_string(),
            ));
        }

        if diags.is_empty() { Ok(()) } else { Err(diags) }
    }
}

/// The newest checkpoint in a storage directory that this crate can vouch for.
///
/// Read from `dbsp`'s catalog without building a circuit, so that a host can
/// resolve "the latest" before anything is started. Checkpoints with no
/// manifest are skipped rather than returned: one is what a crash between
/// `dbsp`'s commit and the manifest write leaves behind, and choosing it would
/// turn a restart into a refusal.
pub fn latest(
    storage: &dbsp::circuit::CircuitStorageConfig,
) -> Result<Option<String>, Vec<Diagnostic>> {
    let root = Path::new(&storage.config.path);
    let all = dbsp::circuit::checkpointer::Checkpointer::read_checkpoints(&*storage.backend)
        .map_err(|e| {
            vec![Diagnostic::error(
                Pass::Config,
                None,
                format!(
                    "reading the checkpoint catalog in `{}`: {e}",
                    root.display()
                ),
            )]
        })?;
    Ok(all
        .iter()
        .rev()
        .map(|c| c.uuid.to_string())
        .find(|uuid| Manifest::path(root, uuid).is_file()))
}
