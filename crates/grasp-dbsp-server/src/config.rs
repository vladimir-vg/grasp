//! The pipeline configuration file, spelled the way Feldera spells it.
//!
//! This server is a drop-in for Feldera's, which means the configuration is too
//! — an operator who knows `workers:` and `storage_config:` should not have to
//! learn a second vocabulary for the same two ideas. So the keys here are
//! Feldera's keys, and the storage half is Feldera's own *types*, deserialized
//! straight out of `feldera-types` by way of `dbsp`'s re-export.
//!
//! Feldera's `PipelineConfig` flattens its `RuntimeConfig`
//! (`feldera-types/src/config.rs:101-103`), which is why `workers` and
//! `storage` are top-level keys there and top-level keys here rather than
//! nested under something.
//!
//! **Two departures, both deliberate.**
//!
//! *Unknown keys are refused.* Feldera accepts and ignores them. A
//! configuration file is the artifact a person is most likely to get wrong, and
//! a silently ignored key is a setting that did not take effect and said
//! nothing — the same argument the fixture loader makes about
//! `deny_unknown_fields` ("without it a misspelled key in a fixture is silently
//! ignored and the case quietly asserts nothing"). Feldera keys this server has
//! no counterpart for are refused *by name*, with a sentence saying why, rather
//! than through serde's "unknown field, expected one of …".
//!
//! *Storage is off unless asked for.* Feldera's `RuntimeConfig::default` sets
//! `storage: Some(StorageOptions::default())` (`config.rs:1180-1184`) because
//! its manager always supplies a directory to go with it. There is no manager
//! here, and inventing a temporary directory would mean a restart silently lost
//! whatever had spilled — so a file that says nothing about storage gets none.

use grasp_dbsp_runner::diag::{Diagnostic, Pass, Span};
use grasp_dbsp_runner::lower::RunnerConfig;
use serde::Deserialize;
use std::num::NonZeroUsize;
use std::path::Path;

/// What a configuration file may say.
///
/// Every field is optional except by convention: a file whose only content is
/// `{}` is a valid single-worker in-memory pipeline named `grasp`, which is
/// what a first experiment wants.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineConfig {
    /// The name in `/v0/pipelines/{name}/…`.
    ///
    /// Feldera's `PipelineConfig::name` (`config.rs:119`). A request naming any
    /// other pipeline gets a 404, which is what it would get from a manager
    /// that had never heard of it.
    #[serde(default = "default_name")]
    pub name: String,

    /// `dbsp` worker threads. Feldera's `RuntimeConfig::workers`
    /// (`config.rs:861`).
    ///
    /// **Feldera's default is 8 and this one is 1**, which is a real
    /// divergence and not an oversight. `RunnerConfig`'s own reasoning is that
    /// a count nobody chose should be the one that needs no invariant to be
    /// right: more than one worker rests on the hashing invariants in
    /// `mapping.md`, and a default that quietly engages them would make a
    /// placement bug somebody else's mystery. Feldera picked 8 for production
    /// clusters; a file that says nothing here is not a production cluster.
    #[serde(default = "default_workers")]
    pub workers: u16,

    /// A memory budget for the whole process, in megabytes. Feldera's
    /// `RuntimeConfig::max_rss_mb` (`config.rs:883`).
    ///
    /// It is what makes spilling adapt rather than wait for a fixed threshold.
    /// Per *process*, not per pipeline.
    #[serde(default)]
    pub max_rss_mb: Option<u64>,

    /// How storage is used. Feldera's `RuntimeConfig::storage`
    /// (`config.rs:927`), and its type verbatim.
    #[serde(default)]
    pub storage: Option<dbsp::circuit::StorageOptions>,

    /// Where storage lives. Feldera's `PipelineConfig::storage_config`
    /// (`config.rs:133`), and its type verbatim.
    ///
    /// The split between this and [`Self::storage`] is Feldera's, and worth
    /// keeping: one is where the bytes go and the other is when they go there.
    /// Feldera's rule that neither works without the other (`config.rs:127-133`)
    /// is enforced here rather than defaulted around.
    #[serde(default)]
    pub storage_config: Option<dbsp::circuit::StorageConfig>,

    /// Views whose full contents this server keeps, so that
    /// `POST /egress/{view}?send_snapshot=true` has something to send.
    ///
    /// **Feldera has no such key**, and the reason is worth stating: it learns
    /// materialization from SQL, `CREATE MATERIALIZED VIEW`. grasp-dbsp has no
    /// such declaration, and more to the point this runner has no way to read a
    /// relation's contents at all — `step` returns deltas and nothing else. So
    /// a snapshot is a fold this server keeps in memory, which costs a second
    /// copy of the relation outside the circuit's own budget. That cost is why
    /// it is opt-in and named per view rather than on for everything.
    #[serde(default)]
    pub materialized: Vec<String>,
}

fn default_name() -> String {
    "grasp".to_string()
}

fn default_workers() -> u16 {
    1
}

impl PipelineConfig {
    /// Reads a configuration file, or says what is wrong with it.
    pub fn read(path: &Path) -> Result<PipelineConfig, Vec<Diagnostic>> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            vec![Diagnostic::error(
                Pass::Config,
                None,
                format!("reading `{}`: {e}", path.display()),
            )]
        })?;
        Self::parse(&text)
    }

    /// The same, from text already in hand — which is what the tests use.
    pub fn parse(text: &str) -> Result<PipelineConfig, Vec<Diagnostic>> {
        // Classified first, so that a key Feldera has and this server does not
        // is answered with a sentence about that key rather than with serde's
        // list of the ones it expected.
        let mapping: serde_yaml::Mapping = match serde_yaml::from_str(text) {
            Ok(m) => m,
            // Not a mapping at all: let the typed parse produce the message,
            // since "expected a map" is already what it says.
            Err(_) => serde_yaml::Mapping::new(),
        };
        let mut diags = Vec::new();
        for key in mapping.keys() {
            let Some(key) = key.as_str() else { continue };
            if let Some(why) = unsupported(key) {
                diags.push(Diagnostic::error(Pass::Config, locate(text, key), why));
            }
        }
        if !diags.is_empty() {
            return Err(diags);
        }

        let config: PipelineConfig = serde_yaml::from_str(text).map_err(|e| {
            let span = e.location().map(|l| Span::new(l.line(), l.column(), 0));
            // `serde_yaml` appends "at line N column M" to its message, which
            // the span already says and `render` already prints.
            let message = e.to_string();
            let message = match (span.is_some(), message.rfind(" at line ")) {
                (true, Some(i)) => message[..i].to_string(),
                _ => message,
            };
            vec![Diagnostic::error(Pass::Config, span, message)]
        })?;
        config.check()?;
        Ok(config)
    }

    /// The rules a well-formed file still has to satisfy.
    fn check(&self) -> Result<(), Vec<Diagnostic>> {
        let mut diags = Vec::new();

        if self.workers == 0 {
            diags.push(Diagnostic::error(
                Pass::Config,
                None,
                "`workers` is zero: a circuit runs on at least one worker thread. \
                 Leave the key out for one."
                    .to_string(),
            ));
        }

        // Feldera's own rule (`feldera-types/src/config.rs:127-133`): the two
        // halves are meaningless apart. Defaulting the missing half would be
        // worse than refusing — a path this server invented is a path an
        // operator does not know to clean up, and options with nowhere to write
        // are a setting that did nothing.
        match (&self.storage, &self.storage_config) {
            (Some(_), None) => diags.push(Diagnostic::error(
                Pass::Config,
                None,
                "`storage` says how to use storage and `storage_config` says where it \
                 lives; this file has the first and not the second. Add \
                 `storage_config: {path: <directory>}`."
                    .to_string(),
            )),
            (None, Some(_)) => diags.push(Diagnostic::error(
                Pass::Config,
                None,
                "`storage_config` names a directory but `storage` is absent, so nothing \
                 would ever be written to it. Add `storage: {}` for the defaults."
                    .to_string(),
            )),
            _ => {}
        }

        if diags.is_empty() { Ok(()) } else { Err(diags) }
    }

    /// The keys that name something in the program, checked against it.
    ///
    /// Separate from [`Self::parse`] because it needs the program, and a
    /// configuration is readable — and worth reporting on — before there is a
    /// compiled plan to read it against. A view named here that the program
    /// does not have is the mistake this catches, and it is an easy one: a
    /// renamed node leaves the configuration behind.
    pub fn check_against(
        &self,
        plan: &grasp_dbsp_runner::typecheck::Plan,
    ) -> Result<(), Vec<Diagnostic>> {
        let views = plan.views();
        let diags: Vec<Diagnostic> = self
            .materialized
            .iter()
            .filter(|name| !views.contains(&name.as_str()))
            .map(|name| {
                Diagnostic::error(
                    Pass::Config,
                    None,
                    format!(
                        "`materialized` names `{name}`, which is not a view of this program. \
                         Its views are: {}.",
                        if views.is_empty() {
                            "none".to_string()
                        } else {
                            views.join(", ")
                        }
                    ),
                )
            })
            .collect();
        if diags.is_empty() { Ok(()) } else { Err(diags) }
    }

    /// The worker count, storage and memory budget, as the runner wants them.
    ///
    /// Opening the storage backend is the fallible part, and it happens here —
    /// before a port is bound — so that a locked directory is a diagnostic on
    /// stderr rather than a server that appears to start and then cannot run.
    pub fn runner(&self) -> Result<RunnerConfig, Vec<Diagnostic>> {
        let storage = match (&self.storage_config, &self.storage) {
            (Some(where_), Some(how)) => Some(
                dbsp::circuit::CircuitStorageConfig::for_config(where_.clone(), how.clone())
                    .map_err(|e| {
                        vec![Diagnostic::error(
                            Pass::Config,
                            None,
                            format!(
                                "opening the storage directory `{}`: {e}. `dbsp` takes an \
                                 exclusive lock on `<path>/feldera.pidlock` and waits sixty \
                                 seconds for it, so another pipeline using this directory \
                                 looks like this.",
                                where_.path
                            ),
                        )]
                    })?,
            ),
            _ => None,
        };

        Ok(RunnerConfig {
            workers: NonZeroUsize::new(self.workers as usize).unwrap_or(NonZeroUsize::MIN),
            storage,
            max_rss_bytes: self.max_rss_mb.map(|mb| mb.saturating_mul(1024 * 1024)),
        })
    }
}

/// Why a Feldera key is not accepted here, if it is one.
///
/// Every entry is a key a real Feldera configuration file may contain, so a
/// file copied from a Feldera deployment gets told what to remove and why,
/// rather than being told the key is unknown — which would be untrue, since it
/// is a key Feldera knows and this server does not implement.
fn unsupported(key: &str) -> Option<String> {
    let why = match key {
        "inputs" | "outputs" => {
            "configures Feldera connectors — Kafka, files, object stores. This server has \
             exactly one input transport, `POST /ingress/{table}`, and one output transport, \
             `POST /egress/{view}`, so there is nothing for a connector configuration to \
             configure"
        }
        "fault_tolerance" | "checkpoint_during_suspend" => {
            "needs checkpoints, and this runner starts a circuit from nothing. `Runner::build` \
             refuses a storage configuration naming an initial checkpoint for the same reason: \
             restoring one would pin the worker count it was written at and freeze `DynValue`'s \
             archived variant order"
        }
        "hosts" | "multihost" => {
            "is a multi-host `dbsp` layout. This runner names a worker count and nothing else"
        }
        "clock_resolution_usecs" | "clock_timezone_offset" => {
            "paces Feldera's clock for SQL's `NOW()`. This language has no clock: a timestamp \
             is a value a program is given, never one it reads"
        }
        "min_batch_size_records" | "max_buffering_delay_usecs" => {
            "tunes how Feldera's controller batches input before stepping. This server steps \
             once per drain of its command queue, so a burst of requests is already one \
             transaction, and an explicit transaction is the knob for grouping more"
        }
        "program_ir"
        | "given_name"
        | "secrets_dir"
        | "resources"
        | "pin_cpus"
        | "env"
        | "dev_tweaks"
        | "logging"
        | "tracing"
        | "tracing_endpoint_jaeger"
        | "cpu_profiler"
        | "http_workers"
        | "io_workers"
        | "datafusion_memory_mb"
        | "provisioning_timeout_secs"
        | "max_parallel_connector_init"
        | "init_containers"
        | "pipeline_template_configmap" => {
            "is a Feldera pipeline-manager setting with no counterpart in a server that runs \
             one program in one process"
        }
        _ => return None,
    };
    Some(format!("`{key}` {why}. Remove the key."))
}

/// Where a top-level key sits in the file.
///
/// `serde_yaml` gives a location for a parse *error* and nothing for a key that
/// parsed fine, so this finds the first line that starts with the key. A
/// configuration file is small and its top-level keys are unindented, which is
/// what makes a scan good enough — and a span slightly off is still better than
/// no span, because it puts the reader on the right line.
fn locate(text: &str, key: &str) -> Option<Span> {
    text.lines().enumerate().find_map(|(i, line)| {
        let trimmed = line.trim_start();
        (trimmed.starts_with(key) && trimmed[key.len()..].starts_with(':'))
            .then(|| Span::new(i + 1, line.len() - trimmed.len() + 1, key.len()))
    })
}
