//! Reading a Kafka topic into a table.
//!
//! One plain thread per connector, polling a `BaseConsumer` — no async
//! runtime, for the reason the circuit thread has none. It sends what it reads
//! to the circuit thread as [`Command::PushMessages`], one command per batch;
//! the circuit loop already makes a drain one transaction, so there is no timer
//! here and no batching knob.
//!
//! **Partitions are assigned, not subscribed.** No consumer group, nothing
//! committed to Kafka: the read position lives in the checkpoint, as in
//! Feldera's fault-tolerant reader (`adapters/src/transport/kafka/ft.rs:94-150`).
//! Every starting offset is resolved to a number before the port is bound, so
//! the position a checkpoint saves is never a symbolic "latest" that would
//! resolve differently the second time.
//!
//! **A message is its rows at its offset.** Every row of one message shares
//! the message's offset, which is Feldera's metadata rule, and what a program
//! sees in its `offset_as` column. A row that does not decode is skipped and
//! counted while the rest of its message is kept; a null payload is skipped;
//! either way the position moves past the message.

use crate::circuit::{Command, Handle, Message};
use crate::config::KafkaInput;
use crate::connectors::{Connector, Metrics, Position};
use feldera_types::transport::kafka::{KafkaLogLevel, KafkaStartFromConfig};
use rdkafka::Message as _;
use rdkafka::config::{ClientConfig, RDKafkaLogLevel};
use rdkafka::consumer::{BaseConsumer, Consumer};
use rdkafka::topic_partition_list::{Offset, TopicPartitionList};
use rdkafka::types::RDKafkaErrorCode;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;

/// How long a metadata or watermark request may take at startup.
const METADATA_TIMEOUT: Duration = Duration::from_secs(10);

/// The most messages one command carries. A batch ends sooner when the topic
/// has nothing more waiting.
const BATCH: usize = 10_000;

/// How long an idle poll waits, which is also how soon a stopping reader
/// notices.
const POLL: Duration = Duration::from_millis(100);

/// A connector whose consumer is created and assigned, not yet reading.
pub struct Opened {
    input: KafkaInput,
    consumer: BaseConsumer,
    position: Position,
}

impl Opened {
    /// Where it will start: per partition, the next offset it reads.
    pub fn position(&self) -> &Position {
        &self.position
    }
}

/// Opens every connector, or says why one cannot be.
///
/// `saved` is what the checkpoint being resumed kept for each endpoint, and
/// `counters` its manifest's offset counters — the table-side view of the same
/// history. A table whose checkpoint numbered its records while no position was
/// saved for its connector was fed some other way, and reading the topic from
/// `start_from` would issue offsets that history already holds.
pub fn open_all(
    inputs: &[KafkaInput],
    saved: Option<&crate::connectors::Positions>,
    counters: Option<&BTreeMap<String, BTreeMap<i64, i64>>>,
) -> Result<Vec<Opened>, String> {
    inputs
        .iter()
        .map(|input| {
            let position = saved.and_then(|s| s.get(&input.endpoint));
            let numbered = counters
                .and_then(|c| c.get(&input.table))
                .is_some_and(|partitions| !partitions.is_empty());
            if position.is_none() && numbered {
                return Err(format!(
                    "input `{}`: the checkpoint numbered the records of `{}` without saving a \
                     position for this input, so its rows came from somewhere else. Reading \
                     the topic from `start_from` would give new records offsets its history \
                     already holds. Start without `--resume-from`, or keep feeding `{}` the \
                     way it was fed.",
                    input.endpoint, input.table, input.table
                ));
            }
            open(input, position)
        })
        .collect()
}

/// Creates one connector's consumer and assigns it its starting offsets.
pub fn open(input: &KafkaInput, saved: Option<&Position>) -> Result<Opened, String> {
    let config = &input.config;
    let topic = config.topic.as_str();
    let fail = |what: String| format!("input `{}`: {what}", input.endpoint);

    let mut client = ClientConfig::new();
    for (key, value) in &config.kafka_options {
        client.set(key, value);
    }
    // A reader that owns its position, as Feldera's is: a group id nobody else
    // has, nothing committed, and an out-of-range offset reported rather than
    // quietly reset — a reset would re-read or skip records.
    client
        .set("group.id", uuid::Uuid::new_v4().to_string())
        .set("enable.auto.commit", "false")
        .set("enable.auto.offset.store", "false")
        .set("auto.offset.reset", "error")
        .set("enable.partition.eof", "false");
    if let Some(level) = config.log_level {
        client.set_log_level(log_level(level));
    }
    let consumer: BaseConsumer = client
        .create()
        .map_err(|e| fail(format!("creating the Kafka consumer: {e}")))?;

    let metadata = consumer
        .fetch_metadata(Some(topic), METADATA_TIMEOUT)
        .map_err(|e| fail(format!("reading the metadata of topic `{topic}`: {e}")))?;
    let Some(found) = metadata.topics().iter().find(|t| t.name() == topic) else {
        return Err(fail(format!("topic `{topic}` does not exist")));
    };
    if let Some(error) = found.error() {
        return Err(fail(format!("topic `{topic}`: {error:?}")));
    }
    let mut existing: Vec<i32> = found.partitions().iter().map(|p| p.id()).collect();
    existing.sort();
    if existing.is_empty() {
        return Err(fail(format!("topic `{topic}` does not exist")));
    }
    let partitions = match &config.partitions {
        None => existing.clone(),
        Some(named) => {
            if let Some(p) = named.iter().find(|p| !existing.contains(p)) {
                return Err(fail(format!(
                    "topic `{topic}` has no partition {p}; its partitions are {existing:?}"
                )));
            }
            named.clone()
        }
    };

    let watermarks = |p: i32| {
        consumer
            .fetch_watermarks(topic, p, METADATA_TIMEOUT)
            .map_err(|e| fail(format!("reading the offsets of partition {p}: {e}")))
    };

    let mut next = BTreeMap::new();
    match saved {
        Some(saved) => {
            if saved.topic != topic {
                return Err(fail(format!(
                    "the checkpoint read topic `{}`, and this configuration names `{topic}`. A \
                     position in one topic means nothing in another.",
                    saved.topic
                )));
            }
            for &p in &partitions {
                let (low, _) = watermarks(p)?;
                let at = match saved.partitions.get(&p) {
                    // A partition the checkpoint never read, added to the topic
                    // since: all of it is new.
                    None => low,
                    Some(&at) if at < low && config.resume_earliest_if_data_expires => {
                        eprintln!(
                            "grasp-dbsp-server: input `{}`: partition {p} no longer holds offset \
                             {at}, where the checkpoint stopped; resuming from {low}, as \
                             `resume_earliest_if_data_expires` says",
                            input.endpoint
                        );
                        low
                    }
                    Some(&at) if at < low => {
                        return Err(fail(format!(
                            "partition {p} no longer holds offset {at}, where the checkpoint \
                             stopped: its earliest is now {low}, so resuming would skip the \
                             records between. Set `resume_earliest_if_data_expires: true` to \
                             resume from {low} anyway."
                        )));
                    }
                    Some(&at) => at,
                };
                next.insert(p, at);
            }
        }
        None => match &config.start_from {
            KafkaStartFromConfig::Earliest => {
                for &p in &partitions {
                    next.insert(p, watermarks(p)?.0);
                }
            }
            KafkaStartFromConfig::Latest => {
                for &p in &partitions {
                    next.insert(p, watermarks(p)?.1);
                }
            }
            KafkaStartFromConfig::Offsets(offsets) => {
                if offsets.len() != partitions.len() {
                    return Err(fail(format!(
                        "`start_from` gives {} offset(s) and {} partition(s) are read: one \
                         offset per partition, in the order of `partitions`",
                        offsets.len(),
                        partitions.len()
                    )));
                }
                for (&p, &at) in partitions.iter().zip(offsets) {
                    let (low, high) = watermarks(p)?;
                    if at < low || at > high {
                        return Err(fail(format!(
                            "`start_from` gives offset {at} for partition {p}, which holds \
                             {low} to {high}"
                        )));
                    }
                    next.insert(p, at);
                }
            }
            KafkaStartFromConfig::Timestamp(timestamp) => {
                let mut times = TopicPartitionList::new();
                for &p in &partitions {
                    times
                        .add_partition_offset(topic, p, Offset::Offset(*timestamp))
                        .map_err(|e| fail(e.to_string()))?;
                }
                let found = consumer
                    .offsets_for_times(times, METADATA_TIMEOUT)
                    .map_err(|e| fail(format!("finding offsets by timestamp: {e}")))?;
                for element in found.elements() {
                    let p = element.partition();
                    let at = match element.offset() {
                        Offset::Offset(at) => at,
                        // No message that late: start at the end.
                        Offset::End => watermarks(p)?.1,
                        other => {
                            return Err(fail(format!(
                                "finding offsets by timestamp: partition {p} answered {other:?}"
                            )));
                        }
                    };
                    next.insert(p, at);
                }
            }
        },
    }

    let mut assignment = TopicPartitionList::new();
    for (&p, &at) in &next {
        assignment
            .add_partition_offset(topic, p, Offset::Offset(at))
            .map_err(|e| fail(e.to_string()))?;
    }
    consumer
        .assign(&assignment)
        .map_err(|e| fail(format!("assigning partitions: {e}")))?;

    Ok(Opened {
        input: input.clone(),
        consumer,
        position: Position {
            topic: topic.to_string(),
            partitions: next,
        },
    })
}

fn log_level(level: KafkaLogLevel) -> RDKafkaLogLevel {
    match level {
        KafkaLogLevel::Emerg => RDKafkaLogLevel::Emerg,
        KafkaLogLevel::Alert => RDKafkaLogLevel::Alert,
        KafkaLogLevel::Critical => RDKafkaLogLevel::Critical,
        KafkaLogLevel::Error => RDKafkaLogLevel::Error,
        KafkaLogLevel::Warning => RDKafkaLogLevel::Warning,
        KafkaLogLevel::Notice => RDKafkaLogLevel::Notice,
        KafkaLogLevel::Info => RDKafkaLogLevel::Info,
        KafkaLogLevel::Debug => RDKafkaLogLevel::Debug,
    }
}

/// The reader threads, and how to stop them.
pub struct Readers {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Readers {
    /// Stops every reader and waits for it, which drops its handle to the
    /// circuit. The circuit thread only exits once every handle is gone, so a
    /// host stops the readers before it joins that thread.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads {
            let _ = thread.join();
        }
    }
}

/// Starts every opened connector reading into the circuit behind `handle`.
///
/// Records the connectors on the handle first, so that the HTTP layer the
/// handle is given to afterwards knows them, and tells the circuit thread
/// each starting position before any message, so that a checkpoint taken
/// straight away saves it. Blocks for those replies, so call it outside an
/// async runtime.
pub fn start(opened: Vec<Opened>, handle: &mut Handle) -> Result<Readers, String> {
    let connectors: Vec<Connector> = opened
        .iter()
        .map(|o| Connector {
            endpoint: o.input.endpoint.clone(),
            table: o.input.table.clone(),
            topic: o.position.topic.clone(),
            metrics: Arc::new(Metrics::default()),
        })
        .collect();
    handle.connectors = Arc::new(connectors.clone());

    for o in &opened {
        let (reply, answer) = oneshot::channel();
        handle
            .send(Command::Connect {
                endpoint: o.input.endpoint.clone(),
                position: o.position.clone(),
                reply,
            })
            .map_err(|e| e.to_string())?;
        answer
            .blocking_recv()
            .map_err(|_| handle.shared.why_gone())?;
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    for (o, c) in opened.into_iter().zip(connectors) {
        let handle = handle.clone();
        let stop = Arc::clone(&stop);
        threads.push(
            std::thread::Builder::new()
                .name(format!("grasp-kafka-{}", o.input.endpoint))
                .spawn(move || read(o, handle, c.metrics, stop))
                .map_err(|e| format!("starting a Kafka reader: {e}"))?,
        );
    }
    Ok(Readers { stop, threads })
}

/// The loop: poll a batch, decode it, hand it to the circuit, and wait for the
/// answer before reading more.
///
/// Waiting is the first half of backpressure — a reader is never more than one
/// batch ahead of the circuit. `max_queued_records` is the second: while that
/// many rows wait for a transaction — a paused pipeline, or an open one — the
/// partitions are paused, and resumed once the rows are consumed.
fn read(opened: Opened, handle: Handle, metrics: Arc<Metrics>, stop: Arc<AtomicBool>) {
    let Opened {
        input,
        consumer,
        position,
    } = opened;
    let endpoint = input.endpoint.as_str();
    let fatal = |why: String| {
        eprintln!("grasp-dbsp-server: input `{endpoint}` stopped: {why}");
        if let Ok(mut slot) = metrics.fatal_error.lock() {
            *slot = Some(why);
        }
    };

    let Some(row_type) = handle.ingress.get(&input.table).map(|i| i.row_type.clone()) else {
        fatal(format!("`{}` is not a table of this program", input.table));
        return;
    };
    let mut next = position.partitions.clone();
    let mut all = TopicPartitionList::new();
    for &p in next.keys() {
        all.add_partition(&position.topic, p);
    }

    let mut paused = false;
    while !stop.load(Ordering::Relaxed) {
        let full = handle.shared.buffered_input_records.load(Ordering::Relaxed)
            >= input.max_queued_records;
        if full != paused {
            let changed = if full {
                consumer.pause(&all)
            } else {
                consumer.resume(&all)
            };
            match changed {
                Ok(()) => paused = full,
                Err(e) => {
                    metrics.num_transport_errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!("grasp-dbsp-server: input `{endpoint}`: {e}");
                }
            }
        }

        let mut messages = Vec::new();
        let mut wait = POLL;
        while messages.len() < BATCH {
            let Some(result) = consumer.poll(wait) else {
                break;
            };
            wait = Duration::ZERO;
            let message = match result {
                Ok(message) => message,
                Err(e) => {
                    metrics.num_transport_errors.fetch_add(1, Ordering::Relaxed);
                    if e.rdkafka_error_code() == Some(RDKafkaErrorCode::Fatal) {
                        fatal(e.to_string());
                        return;
                    }
                    eprintln!("grasp-dbsp-server: input `{endpoint}`: {e}");
                    continue;
                }
            };

            let (partition, offset) = (message.partition(), message.offset());
            let expected = next.get(&partition).copied().unwrap_or(0);
            if offset < expected {
                eprintln!(
                    "grasp-dbsp-server: input `{endpoint}`: partition {partition} delivered \
                     offset {offset} after {}; dropped",
                    expected - 1
                );
                continue;
            }
            next.insert(partition, offset + 1);

            let rows = match message.payload() {
                // A tombstone: nothing to ingest, and still read.
                None => Vec::new(),
                Some(bytes) => match std::str::from_utf8(bytes) {
                    Err(e) => {
                        metrics.num_parse_errors.fetch_add(1, Ordering::Relaxed);
                        eprintln!(
                            "grasp-dbsp-server: input `{endpoint}`: partition {partition}, \
                             offset {offset} is not UTF-8: {e}"
                        );
                        Vec::new()
                    }
                    Ok(text) => {
                        let (rows, errors) =
                            crate::decode::rows(text, &row_type, input.update_format, input.array);
                        if !errors.is_empty() {
                            metrics
                                .num_parse_errors
                                .fetch_add(errors.len() as u64, Ordering::Relaxed);
                            eprintln!(
                                "grasp-dbsp-server: input `{endpoint}`: partition {partition}, \
                                 offset {offset}: {} value(s) did not decode: {}",
                                errors.len(),
                                serde_json::Value::Array(errors)
                            );
                        }
                        rows
                    }
                },
            };
            metrics
                .total_records
                .fetch_add(rows.len() as u64, Ordering::Relaxed);
            messages.push(Message {
                partition,
                offset,
                rows,
            });
        }
        if messages.is_empty() {
            continue;
        }

        let (reply, answer) = oneshot::channel();
        let sent = handle.send(Command::PushMessages {
            endpoint: endpoint.to_string(),
            table: input.table.clone(),
            messages,
            reply,
        });
        match sent.ok().and_then(|()| answer.blocking_recv().ok()) {
            // The circuit is gone, and so is anything to read into.
            None => return,
            Some(Err(e)) => {
                fatal(e.to_string());
                return;
            }
            Some(Ok(pushed)) => {
                for why in pushed.refused {
                    eprintln!("grasp-dbsp-server: input `{endpoint}`: refused {why}");
                }
            }
        }
    }
}
