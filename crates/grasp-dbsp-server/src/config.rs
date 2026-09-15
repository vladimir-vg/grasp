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

use grasp_dbsp::diag::{Diagnostic, Pass, Span};
use grasp_dbsp::lower::RunnerConfig;
use serde::Deserialize;
use std::collections::BTreeMap;
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

    /// How many checkpoints to keep in `storage_config.path`; older ones are
    /// removed after each new one commits.
    ///
    /// **Feldera has no such key**: its manager decides what to keep. Without
    /// one here a directory grows by a checkpoint per request for ever. The
    /// default is `dbsp`'s own floor — it never removes below two
    /// (`Checkpointer::MIN_CHECKPOINT_THRESHOLD`) — so a smaller number is not
    /// refused, only not honoured below that.
    #[serde(default = "default_checkpoint_retention")]
    pub checkpoint_retention: usize,

    /// Connectors that read a table's rows from outside, by endpoint name.
    /// Feldera's `PipelineConfig::inputs` (`config.rs:142`), in its shape:
    /// each entry names its table as `stream`, and carries a `transport` and a
    /// `format`.
    ///
    /// Kafka is the only transport, and JSON the only format. See
    /// [`InputEndpoint`] for what else is refused, and why.
    #[serde(default)]
    pub inputs: BTreeMap<String, InputEndpoint>,
}

/// One input connector, as a configuration file writes it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputEndpoint {
    /// The table the rows go into.
    pub stream: String,
    pub transport: Transport,
    /// Optional, as in Feldera; absent is `json` with its defaults.
    #[serde(default)]
    pub format: Option<FormatSpec>,
    /// While this many rows are waiting for a transaction, the connector stops
    /// reading. Feldera's `ConnectorConfig::max_queued_records`, and its
    /// default.
    #[serde(default = "default_max_queued_records")]
    pub max_queued_records: u64,
}

/// `transport: {name, config}`, with the config kept untyped until the name
/// says which type it is — the order Feldera's own tagged enum reads it in.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transport {
    pub name: String,
    #[serde(default)]
    pub config: serde_yaml::Value,
}

/// `format: {name, config}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormatSpec {
    pub name: String,
    #[serde(default)]
    pub config: JsonFormat,
}

/// The `json` format's settings: Feldera's `JsonParserConfig`, less what this
/// server's decoder does not have.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonFormat {
    /// `insert_delete` or `weighted`, as `POST /ingress` takes them.
    #[serde(default = "default_update_format")]
    pub update_format: String,
    /// A message is a JSON array of rows rather than rows back to back.
    #[serde(default)]
    pub array: bool,
    /// Accepted and changes nothing: rows may span lines or share one either
    /// way, as over HTTP.
    #[serde(default)]
    pub lines: Option<String>,
}

impl Default for JsonFormat {
    fn default() -> Self {
        JsonFormat {
            update_format: default_update_format(),
            array: false,
            lines: None,
        }
    }
}

fn default_update_format() -> String {
    "insert_delete".to_string()
}

fn default_max_queued_records() -> u64 {
    1_000_000
}

/// A connector, checked and resolved: what the server starts a reader from.
#[derive(Debug, Clone)]
pub struct KafkaInput {
    /// The endpoint's name, as `/stats` reports it.
    pub endpoint: String,
    pub table: String,
    pub config: feldera_types::transport::kafka::KafkaInputConfig,
    pub update_format: grasp_dbsp::json::Format,
    pub array: bool,
    pub max_queued_records: u64,
}

fn default_name() -> String {
    "grasp".to_string()
}

fn default_workers() -> u16 {
    1
}

fn default_checkpoint_retention() -> usize {
    2
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
        // The same, one level down: a Feldera connector's keys that this
        // server's connectors do not have.
        if let Some(serde_yaml::Value::Mapping(inputs)) = mapping.get("inputs") {
            for (endpoint, body) in inputs {
                let (Some(endpoint), serde_yaml::Value::Mapping(body)) = (endpoint.as_str(), body)
                else {
                    continue;
                };
                for key in body.keys().filter_map(|k| k.as_str()) {
                    if let Some(why) = unsupported_in_endpoint(key) {
                        diags.push(Diagnostic::error(
                            Pass::Config,
                            None,
                            format!("input `{endpoint}`: `{key}` {why}. Remove the key."),
                        ));
                    }
                }
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

        if let Err(more) = self.kafka_inputs() {
            diags.extend(more);
        }

        if diags.is_empty() { Ok(()) } else { Err(diags) }
    }

    /// Every input connector, resolved into what a reader starts from.
    ///
    /// Called by [`Self::parse`], so that a file with a connector this server
    /// cannot run is refused before anything else happens, and again by the
    /// host that starts the readers.
    pub fn kafka_inputs(&self) -> Result<Vec<KafkaInput>, Vec<Diagnostic>> {
        let mut diags = Vec::new();
        let mut out = Vec::new();
        for (endpoint, input) in &self.inputs {
            let mut refuse = |message: String| {
                diags.push(Diagnostic::error(
                    Pass::Config,
                    None,
                    format!("input `{endpoint}`: {message}"),
                ))
            };

            if input.transport.name != "kafka_input" {
                refuse(format!(
                    "`transport.name: {}` is not a transport this server has. Its one input \
                     connector is `kafka_input`; rows arrive otherwise over `POST /ingress`.",
                    input.transport.name
                ));
                continue;
            }

            let format = input.format.clone().unwrap_or(FormatSpec {
                name: "json".to_string(),
                config: JsonFormat::default(),
            });
            if format.name != "json" {
                refuse(format!(
                    "`format.name: {}` is not implemented. This server decodes `json` and \
                     nothing else.",
                    format.name
                ));
            }
            let update_format = match format.config.update_format.as_str() {
                "insert_delete" => Some(grasp_dbsp::json::Format::InsertDelete),
                "weighted" => Some(grasp_dbsp::json::Format::Weighted),
                other => {
                    refuse(format!(
                        "`update_format: {other}` is not implemented. This server has \
                         `insert_delete` and `weighted`."
                    ));
                    None
                }
            };
            if let Some(lines) = &format.config.lines
                && lines != "single"
                && lines != "multiple"
            {
                refuse(format!(
                    "`lines: {lines}` is neither `single` nor `multiple`."
                ));
            }

            let raw = match &input.transport.config {
                serde_yaml::Value::Mapping(m) => m.clone(),
                serde_yaml::Value::Null => serde_yaml::Mapping::new(),
                _ => {
                    refuse("`transport.config` is a mapping of Kafka settings.".to_string());
                    continue;
                }
            };
            let mut named = false;
            for key in raw.keys().filter_map(|k| k.as_str()) {
                if let Some(why) = unsupported_in_kafka(key) {
                    refuse(format!("`{key}` {why}. Remove the key."));
                    named = true;
                }
            }
            if named {
                continue;
            }
            let config: feldera_types::transport::kafka::KafkaInputConfig =
                match serde_yaml::from_value(serde_yaml::Value::Mapping(raw)) {
                    Ok(c) => c,
                    Err(e) => {
                        refuse(format!("the Kafka settings: {e}"));
                        continue;
                    }
                };
            if let Some(update_format) = update_format {
                out.push(KafkaInput {
                    endpoint: endpoint.clone(),
                    table: input.stream.clone(),
                    config,
                    update_format,
                    array: format.config.array,
                    max_queued_records: input.max_queued_records,
                });
            }
        }
        if diags.is_empty() {
            Ok(out)
        } else {
            Err(diags)
        }
    }

    /// The keys that name something in the program, checked against it.
    ///
    /// Separate from [`Self::parse`] because it needs the program, and a
    /// configuration is readable — and worth reporting on — before there is a
    /// compiled plan to read it against. A view named here that the program
    /// does not have is the mistake this catches, and it is an easy one: a
    /// renamed node leaves the configuration behind.
    pub fn check_against(&self, plan: &grasp_dbsp::typecheck::Plan) -> Result<(), Vec<Diagnostic>> {
        let views = plan.views();
        let mut diags: Vec<Diagnostic> = self
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

        // One connector per table. Offsets are counted per table and
        // partition, so two topics feeding one table would both write a
        // partition 0, and their offsets would interleave meaninglessly.
        let tables: Vec<&str> = plan.inputs().into_iter().map(|(_, t)| t).collect();
        let mut fed: BTreeMap<&str, &str> = BTreeMap::new();
        for (endpoint, input) in &self.inputs {
            if !tables.contains(&input.stream.as_str()) {
                diags.push(Diagnostic::error(
                    Pass::Config,
                    None,
                    format!(
                        "input `{endpoint}` feeds `{}`, which is not a table of this program. \
                         Its tables are: {}.",
                        input.stream,
                        if tables.is_empty() {
                            "none".to_string()
                        } else {
                            tables.join(", ")
                        }
                    ),
                ));
            } else if let Some(first) = fed.insert(&input.stream, endpoint) {
                diags.push(Diagnostic::error(
                    Pass::Config,
                    None,
                    format!(
                        "inputs `{first}` and `{endpoint}` both feed `{}`. A table has one \
                         connector: offsets are counted per table and partition, and two \
                         topics would both write partition 0.",
                        input.stream
                    ),
                ));
            }
        }
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
        "outputs" => {
            "configures Feldera output connectors — Kafka, files, databases. This server has \
             exactly one output transport, `POST /egress/{view}`, so there is nothing for an \
             output connector configuration to configure"
        }
        "fault_tolerance" => {
            "replays journaled *input* after a crash. This server journals nothing, so there \
             is nothing to replay. Checkpoints themselves are supported, and a Kafka input \
             resumes from the offsets its checkpoint saved: `POST /checkpoint`, and \
             `serve --resume-from`"
        }
        "checkpoint_during_suspend" => {
            "is deprecated in Feldera and has no effect there \
             (`feldera-types/src/config.rs:1014-1015`), and this server has no `/suspend` \
             for it to apply to"
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

/// Why a Feldera connector key is not accepted on an input here, if it is one.
fn unsupported_in_endpoint(key: &str) -> Option<&'static str> {
    Some(match key {
        "preprocessor" | "postprocessor" => {
            "transforms bytes before parsing or after encoding. This server has no such \
             stages"
        }
        "index"
        | "send_snapshot"
        | "enable_output_buffer"
        | "max_output_buffer_time_millis"
        | "max_output_buffer_size_records" => "configures an output connector, not an input",
        "soft_delete" => "turns deletes into updates of a marker column. A delete here is a delete",
        "max_batch_size" | "max_worker_batch_size" => {
            "caps how many records Feldera's controller takes into one step. This server \
             steps once per drain of its command queue, and `max_queued_records` is the knob \
             that bounds it"
        }
        "paused" | "start_after" | "labels" => {
            "orchestrates when Feldera's controller starts a connector. Every connector here \
             starts with the server"
        }
        _ => return None,
    })
}

/// Why a Kafka setting Feldera has is not accepted here, if it is one.
///
/// Feldera's `KafkaInputConfig` flattens every key it does not know into the
/// options it hands librdkafka, so the consumer-group settings are named here:
/// without that they would reach librdkafka and quietly change where reading
/// starts.
fn unsupported_in_kafka(key: &str) -> Option<&'static str> {
    Some(match key {
        "include_headers" | "include_timestamp" | "include_partition" | "include_offset"
        | "include_topic" => {
            "puts Kafka metadata where SQL's `CONNECTOR_METADATA()` reads it. Here a program \
             declares the columns the runtime fills: a table whose `input` names \
             `partition_as` and `offset_as` gets the Kafka partition and offset, and there \
             is no timestamp, topic or header column"
        }
        "synchronize_partitions" => {
            "orders ingestion across partitions by Kafka timestamp, for Feldera's lateness. \
             This language has no lateness, and each row carries its own partition and offset"
        }
        "header_filter" => "is not implemented: every message is read",
        "region" | "oauth_provider" => {
            "configures AWS MSK authentication, which is not implemented. SASL and SSL \
             settings are passed to librdkafka as usual"
        }
        "poller_threads" | "group_join_timeout_secs" => {
            "tunes Feldera's reader. This server reads each topic on one thread and joins no \
             consumer group"
        }
        "fault_tolerance" | "kafka_service" => "is a legacy key that Feldera ignores",
        "group.id" | "enable.auto.commit" | "enable.auto.offset.store" | "auto.offset.reset" => {
            "would hand the read position to Kafka. This reader assigns its partitions itself \
             and keeps its position in the checkpoint; where it starts is `start_from`"
        }
        _ => return None,
    })
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
