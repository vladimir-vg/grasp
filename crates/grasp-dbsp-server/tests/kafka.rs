//! The Kafka reader, against a real broker.
//!
//! **Skipped unless `GRASP_KAFKA_BROKERS` is set**, to a bootstrap address such
//! as `127.0.0.1:9092`. A local Redpanda is one command:
//!
//! ```bash
//! rpk container start
//! GRASP_KAFKA_BROKERS=127.0.0.1:9092 cargo test -p grasp-dbsp-server --test kafka
//! ```
//!
//! Every test creates its own topic, so they run in parallel and leave nothing
//! another run could read. What the circuit thread does with messages is
//! `tests/connectors.rs`; this file is the part only a broker can exercise —
//! where reading starts, what a message's payload becomes, and pausing.
//!
//! These tests wait on a broker, so unlike the rest of this crate's they poll
//! with a deadline: a broker's delivery is not caused by the test.

#![cfg(feature = "kafka")]

use dbsp::circuit::{CircuitStorageConfig, StorageCacheConfig, StorageConfig, StorageOptions};
use grasp_dbsp::lower::{Runner, RunnerConfig};
use grasp_dbsp::value::{BatchType, DynValue};
use grasp_dbsp_server::circuit::{self, Command, Handle};
use grasp_dbsp_server::config::PipelineConfig;
use grasp_dbsp_server::{connectors, kafka, snapshot};
use rdkafka::ClientConfig;
use rdkafka::admin::{AdminClient, AdminOptions, NewTopic, TopicReplication};
use rdkafka::client::DefaultClientContext;
use rdkafka::consumer::{BaseConsumer, Consumer};
use rdkafka::producer::{BaseProducer, BaseRecord, Producer};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

const PARTITIONED: &str = "\
inbox := input(\"inbox\", partition_as: \"partition\", offset_as: \"offset\")
inbox :: zset(record(offset: i64, partition: i64, v: i64))
";

const PLAIN: &str = "\
inbox := input(\"inbox\")
inbox :: zset(record(v: i64))
";

fn brokers() -> Option<String> {
    let brokers = std::env::var("GRASP_KAFKA_BROKERS").ok();
    if brokers.is_none() {
        eprintln!("skipped: GRASP_KAFKA_BROKERS is not set");
    }
    brokers
}

/// A topic of its own, with `partitions` partitions, visible in metadata.
fn topic(brokers: &str, name: &str, partitions: i32) -> String {
    let name = format!("grasp-{name}-{}", uuid::Uuid::new_v4());
    let admin: AdminClient<DefaultClientContext> = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .create()
        .expect("an admin client");
    let created = actix_rt::System::new().block_on(admin.create_topics(
        &[NewTopic::new(&name, partitions, TopicReplication::Fixed(1))],
        &AdminOptions::new(),
    ));
    for result in created.expect("creates topics") {
        result.expect("the topic is created");
    }
    let consumer: BaseConsumer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .create()
        .expect("a consumer");
    until("the topic's partitions to appear", || {
        consumer
            .fetch_metadata(Some(&name), Duration::from_secs(5))
            .ok()
            .and_then(|m| {
                m.topics()
                    .first()
                    .map(|t| t.error().is_none() && t.partitions().len() == partitions as usize)
            })
            .unwrap_or(false)
    });
    name
}

/// Produces each `(partition, payload)` in order; `None` is a tombstone.
fn produce(brokers: &str, topic: &str, messages: &[(i32, Option<&str>)]) {
    let producer: BaseProducer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .create()
        .expect("a producer");
    for (partition, payload) in messages {
        let record = BaseRecord::<(), str>::to(topic).partition(*partition);
        let record = match payload {
            Some(p) => record.payload(p),
            None => record,
        };
        producer.send(record).map_err(|(e, _)| e).expect("sends");
    }
    producer.flush(Duration::from_secs(10)).expect("flushes");
}

fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn ask<T>(handle: &Handle, make: impl FnOnce(oneshot::Sender<T>) -> Command) -> T {
    let (tx, rx) = oneshot::channel();
    handle
        .send(make(tx))
        .expect("the circuit thread is listening");
    rx.blocking_recv().expect("the circuit thread replied")
}

/// A pipeline configuration reading `topic` into `inbox`.
fn inputs(brokers: &str, topic: &str, extra: &str) -> Vec<grasp_dbsp_server::config::KafkaInput> {
    let yaml = format!(
        "inputs:\n  inbox_kafka:\n    stream: inbox\n{extra}    transport:\n      name: kafka_input\n      \
         config:\n        bootstrap.servers: \"{brokers}\"\n        topic: \"{topic}\"\n        \
         start_from: earliest\n"
    );
    PipelineConfig::parse(&yaml)
        .unwrap_or_else(|d| panic!("{}", grasp_dbsp::diag::render(&d)))
        .kafka_inputs()
        .unwrap_or_else(|d| panic!("{}", grasp_dbsp::diag::render(&d)))
}

/// A running pipeline over `source`, reading `topic` into `inbox`, which is
/// materialized.
struct Pipeline {
    handle: Handle,
    thread: Option<std::thread::JoinHandle<()>>,
    readers: Option<kafka::Readers>,
}

impl Pipeline {
    fn start(
        source: &str,
        inputs: &[grasp_dbsp_server::config::KafkaInput],
        storage: Option<(&tempfile::TempDir, Option<String>)>,
        running: bool,
    ) -> Pipeline {
        let plan = grasp_dbsp::compile(source)
            .unwrap_or_else(|d| panic!("compiles: {}", grasp_dbsp::diag::render(&d)));
        let views: Vec<String> = plan.views().into_iter().map(str::to_string).collect();
        let config = storage.as_ref().map(|(dir, resume)| {
            let config = CircuitStorageConfig::for_config(
                StorageConfig {
                    path: dir.path().to_string_lossy().into_owned(),
                    cache: StorageCacheConfig::default(),
                },
                StorageOptions::default(),
            )
            .expect("a storage backend");
            config.with_init_checkpoint(resume.as_ref().map(|u| u.parse().expect("a uuid")))
        });
        let runner = Runner::build(
            &plan,
            &views,
            RunnerConfig {
                storage: config,
                ..RunnerConfig::default()
            },
        )
        .unwrap_or_else(|d| panic!("builds: {}", grasp_dbsp::diag::render(&d)));
        let shapes: HashMap<String, BatchType> = views
            .iter()
            .filter_map(|v| grasp_dbsp::lower::shape(&plan, v).map(|t| (v.clone(), t.clone())))
            .collect();

        // What `serve` does on resume: the saved rows, the saved positions, and
        // the manifest's counters.
        let (restored, saved, counters) = match &storage {
            Some((dir, Some(uuid))) => (
                snapshot::load(dir.path(), uuid, &["inbox".to_string()]).expect("rows"),
                connectors::load(dir.path(), uuid).expect("positions"),
                grasp_dbsp::checkpoint::Manifest::read(dir.path(), uuid)
                    .expect("a manifest")
                    .offsets,
            ),
            _ => (HashMap::new(), None, None),
        };
        let opened = kafka::open_all(inputs, saved.as_ref(), counters.as_ref()).expect("opens");

        let (mut handle, thread) = circuit::start_restored(
            runner,
            shapes,
            &["inbox".to_string()],
            vec!["inbox".to_string()],
            running,
            restored,
        );
        let readers = kafka::start(opened, &mut handle).expect("starts reading");
        Pipeline {
            handle,
            thread: Some(thread),
            readers: Some(readers),
        }
    }

    /// Rows every transaction so far has consumed, this run.
    fn done(&self) -> u64 {
        self.handle
            .shared
            .progress
            .lock()
            .map(|p| p.done_count)
            .unwrap_or(0)
    }

    fn metrics(&self) -> &connectors::Metrics {
        &self.handle.connectors[0].metrics
    }

    /// Every row `inbox` holds, as its fields' integers.
    fn contents(&self) -> Vec<Vec<i64>> {
        let subscription = ask(&self.handle, |reply| Command::Subscribe {
            view: "inbox".to_string(),
            snapshot: true,
            backpressure: false,
            reply,
        })
        .expect("subscribes");
        let mut out = Vec::new();
        for ((key, _), weight) in subscription.snapshot.expect("materialized").iter() {
            let DynValue::Record(fields) = key else {
                panic!("a row is a record")
            };
            let ints: Vec<i64> = fields
                .iter()
                .map(|f| match f {
                    DynValue::I64(n) => *n,
                    other => panic!("an i64, found {other:?}"),
                })
                .collect();
            for _ in 0..*weight {
                out.push(ints.clone());
            }
        }
        out.sort();
        out
    }

    /// Takes a checkpoint and stops, leaving it on disk.
    fn checkpoint_and_stop(mut self) {
        ask(&self.handle, |reply| Command::Checkpoint { reply }).expect("a checkpoint starts");
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(readers) = self.readers.take() {
            readers.stop();
        }
        if let Some(thread) = self.thread.take() {
            ask(&self.handle, |reply| Command::Shutdown { reply });
            let _ = thread.join();
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn rows_carry_their_messages_partition_and_offset() {
    let Some(brokers) = brokers() else { return };
    let topic = topic(&brokers, "offsets", 2);
    produce(
        &brokers,
        &topic,
        &[
            // Offset 0: two rows sharing it.
            (0, Some(r#"{"insert": {"v": 1}}{"insert": {"v": 2}}"#)),
            // Offset 1: a tombstone.
            (0, None),
            // Offset 2: a row that is not one, between two that are.
            (
                0,
                Some(r#"{"insert": {"v": 3}} {"nonsense": 1} {"insert": {"v": 4}}"#),
            ),
            // Offset 3: an insert and a delete of one row, which cancel.
            (0, Some(r#"{"insert": {"v": 9}} {"delete": {"v": 9}}"#)),
            // Partition 1 counts from its own zero.
            (1, Some(r#"{"insert": {"v": 1}}"#)),
        ],
    );

    let p = Pipeline::start(PARTITIONED, &inputs(&brokers, &topic, ""), None, true);
    until("seven rows to be computed", || p.done() >= 7);

    assert_eq!(
        p.contents(),
        vec![
            vec![0, 0, 1],
            vec![0, 0, 2],
            vec![0, 1, 1],
            vec![2, 0, 3],
            vec![2, 0, 4],
        ],
        "(offset, partition, v): the message's offset for each of its rows"
    );
    assert_eq!(p.metrics().num_parse_errors.load(Ordering::Relaxed), 1);
    assert_eq!(p.metrics().total_records.load(Ordering::Relaxed), 7);
}

#[test]
fn a_plain_table_takes_the_rows_and_ignores_where_they_came_from() {
    let Some(brokers) = brokers() else { return };
    let topic = topic(&brokers, "plain", 2);
    produce(
        &brokers,
        &topic,
        &[
            (0, Some(r#"{"insert": {"v": 1}}"#)),
            (1, Some(r#"{"insert": {"v": 2}}"#)),
        ],
    );
    let p = Pipeline::start(PLAIN, &inputs(&brokers, &topic, ""), None, true);
    until("two rows", || p.done() >= 2);
    assert_eq!(p.contents(), vec![vec![1], vec![2]]);
}

#[test]
fn a_resumed_reader_neither_replays_nor_skips() {
    let Some(brokers) = brokers() else { return };
    let topic = topic(&brokers, "resume", 1);
    let dir = tempfile::TempDir::new().expect("a directory");
    let inputs = inputs(&brokers, &topic, "");

    produce(
        &brokers,
        &topic,
        &[
            (0, Some(r#"{"insert": {"v": 0}}"#)),
            (0, Some(r#"{"insert": {"v": 1}}"#)),
            (0, Some(r#"{"insert": {"v": 2}}"#)),
        ],
    );
    let first = Pipeline::start(PARTITIONED, &inputs, Some((&dir, None)), true);
    until("three rows", || first.done() >= 3);
    first.checkpoint_and_stop();

    produce(
        &brokers,
        &topic,
        &[
            (0, Some(r#"{"insert": {"v": 3}}"#)),
            (0, Some(r#"{"insert": {"v": 4}}"#)),
        ],
    );
    let config = CircuitStorageConfig::for_config(
        StorageConfig {
            path: dir.path().to_string_lossy().into_owned(),
            cache: StorageCacheConfig::default(),
        },
        StorageOptions::default(),
    )
    .expect("a storage backend");
    let uuid = grasp_dbsp::checkpoint::latest(&config)
        .unwrap_or_else(|d| panic!("{}", grasp_dbsp::diag::render(&d)))
        .expect("a checkpoint");
    drop(config);

    let second = Pipeline::start(PARTITIONED, &inputs, Some((&dir, Some(uuid))), true);
    until("the two new rows", || second.done() >= 2);
    // Nothing else is on its way; a replay would have arrived with these.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        second.contents(),
        (0..5).map(|i| vec![i, 0, i]).collect::<Vec<_>>(),
        "every message once, at its own offset"
    );
    assert_eq!(second.metrics().total_records.load(Ordering::Relaxed), 2);
}

#[test]
fn start_from_latest_skips_what_the_topic_already_holds() {
    let Some(brokers) = brokers() else { return };
    let topic = topic(&brokers, "latest", 1);
    produce(
        &brokers,
        &topic,
        &[
            (0, Some(r#"{"insert": {"v": 0}}"#)),
            (0, Some(r#"{"insert": {"v": 1}}"#)),
        ],
    );
    let mut inputs = inputs(&brokers, &topic, "");
    inputs[0].config.start_from = feldera_types::transport::kafka::KafkaStartFromConfig::Latest;
    let p = Pipeline::start(PARTITIONED, &inputs, None, true);

    produce(&brokers, &topic, &[(0, Some(r#"{"insert": {"v": 2}}"#))]);
    until("the row produced after starting", || p.done() >= 1);
    assert_eq!(p.contents(), vec![vec![2, 0, 2]]);
}

#[test]
fn a_full_queue_pauses_reading_until_a_transaction_drains_it() {
    let Some(brokers) = brokers() else { return };
    let topic = topic(&brokers, "pause", 1);
    let inputs = inputs(&brokers, &topic, "    max_queued_records: 1\n");
    // Paused, so rows wait for a transaction that does not come.
    let p = Pipeline::start(PARTITIONED, &inputs, None, false);

    produce(&brokers, &topic, &[(0, Some(r#"{"insert": {"v": 0}}"#))]);
    until("the first row to be queued", || {
        p.handle
            .shared
            .buffered_input_records
            .load(Ordering::Relaxed)
            >= 1
    });
    produce(
        &brokers,
        &topic,
        &[
            (0, Some(r#"{"insert": {"v": 1}}"#)),
            (0, Some(r#"{"insert": {"v": 2}}"#)),
        ],
    );
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(
        p.handle.shared.total_input_records.load(Ordering::Relaxed),
        1,
        "one row is queued and the queue holds one, so nothing more is read"
    );

    ask(&p.handle, |reply| Command::SetRunning {
        running: true,
        reply,
    });
    until("the rest once the queue drained", || p.done() >= 3);
    assert_eq!(p.contents().len(), 3);
}
