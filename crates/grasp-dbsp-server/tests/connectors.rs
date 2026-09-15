//! What the circuit thread does with a connector's messages, with no broker.
//!
//! A Kafka reader is a loop that turns a topic into [`Command::PushMessages`];
//! everything that decides what a program sees — the offsets its rows carry,
//! which messages move the position, what a checkpoint saves — happens on the
//! circuit thread, and none of it needs Kafka to be wrong. So it is tested here
//! by sending the commands a reader sends. `tests/kafka.rs` is the loop itself,
//! against a real broker.

// Not `actix_web::test` by name: imported, it shadows the `#[test]` attribute
// the synchronous tests below use.
use actix_web::{App, web};
use dbsp::circuit::{CircuitStorageConfig, StorageCacheConfig, StorageConfig, StorageOptions};
use grasp_dbsp::json::decode_value;
use grasp_dbsp::lower::{Runner, RunnerConfig};
use grasp_dbsp::value::{BatchType, DynValue};
use grasp_dbsp_server::circuit::{self, Command, Handle, Message};
use grasp_dbsp_server::connectors::{self, Connector, Metrics, Position};
use grasp_dbsp_server::http::{self, State};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

const SOURCE: &str = "\
inbox := input(\"inbox\", partition_as: \"partition\", offset_as: \"offset\")
inbox :: zset(record(offset: i64, partition: i64, v: i64))
";

fn storage(dir: &tempfile::TempDir) -> CircuitStorageConfig {
    CircuitStorageConfig::for_config(
        StorageConfig {
            path: dir.path().to_string_lossy().into_owned(),
            cache: StorageCacheConfig::default(),
        },
        StorageOptions::default(),
    )
    .expect("a storage backend")
}

/// A started circuit over [`SOURCE`], with `inbox` materialized so a test can
/// read what it holds.
fn started(
    storage: Option<CircuitStorageConfig>,
) -> (
    Handle,
    std::thread::JoinHandle<()>,
    grasp_dbsp::typecheck::Plan,
) {
    let plan = grasp_dbsp::compile(SOURCE)
        .unwrap_or_else(|d| panic!("compiles: {}", grasp_dbsp::diag::render(&d)));
    let views: Vec<String> = plan.views().into_iter().map(str::to_string).collect();
    let runner = Runner::build(
        &plan,
        &views,
        RunnerConfig {
            storage,
            ..RunnerConfig::default()
        },
    )
    .unwrap_or_else(|d| panic!("builds: {}", grasp_dbsp::diag::render(&d)));
    let shapes: HashMap<String, BatchType> = views
        .iter()
        .filter_map(|v| grasp_dbsp::lower::shape(&plan, v).map(|t| (v.clone(), t.clone())))
        .collect();
    let (handle, thread) = circuit::start(
        runner,
        shapes,
        &["inbox".to_string()],
        vec!["inbox".to_string()],
        true,
    );
    (handle, thread, plan)
}

fn ask<T>(handle: &Handle, make: impl FnOnce(oneshot::Sender<T>) -> Command) -> T {
    let (tx, rx) = oneshot::channel();
    handle
        .send(make(tx))
        .expect("the circuit thread is listening");
    rx.blocking_recv().expect("the circuit thread replied")
}

fn row(plan: &grasp_dbsp::typecheck::Plan, v: i64) -> (DynValue, dbsp::ZWeight) {
    let ty = plan.ingress_type("inbox").expect("an input");
    (decode_value(&json!({"v": v}), &ty).expect("a row"), 1)
}

/// `(offset, partition, v)` for every row `inbox` holds.
fn contents(handle: &Handle) -> Vec<(i64, i64, i64)> {
    let subscription = ask(handle, |reply| Command::Subscribe {
        view: "inbox".to_string(),
        snapshot: true,
        backpressure: false,
        reply,
    })
    .expect("subscribes");
    let rows = subscription.snapshot.expect("a materialized view");
    let mut out = Vec::new();
    for ((key, _), weight) in rows.iter() {
        let DynValue::Record(fields) = key else {
            panic!("a row is a record")
        };
        let int = |i: usize| match &fields[i] {
            DynValue::I64(n) => *n,
            other => panic!("an i64, found {other:?}"),
        };
        for _ in 0..*weight {
            out.push((int(0), int(1), int(2)));
        }
    }
    out.sort();
    out
}

#[test]
fn a_connectors_messages_keep_their_offsets_and_a_checkpoint_saves_its_position() {
    let dir = tempfile::TempDir::new().expect("a directory");
    let config = storage(&dir);
    let (handle, thread, plan) = started(Some(config.clone()));

    ask(&handle, |reply| Command::Connect {
        endpoint: "inbox_kafka".to_string(),
        position: Position {
            topic: "inbox".to_string(),
            partitions: BTreeMap::from([(0, 5), (1, 0)]),
        },
        reply,
    });

    let pushed = ask(&handle, |reply| Command::PushMessages {
        endpoint: "inbox_kafka".to_string(),
        table: "inbox".to_string(),
        messages: vec![
            // Two rows sharing one message's offset.
            Message {
                partition: 0,
                offset: 5,
                rows: vec![row(&plan, 1), row(&plan, 2)],
            },
            // A tombstone: no rows, and the position still moves past it.
            Message {
                partition: 0,
                offset: 6,
                rows: vec![],
            },
            Message {
                partition: 1,
                offset: 2,
                rows: vec![row(&plan, 1)],
            },
            // Behind partition 0, which is at 7 now: refused, rows and all.
            Message {
                partition: 0,
                offset: 3,
                rows: vec![row(&plan, 9)],
            },
        ],
        reply,
    })
    .expect("pushes");
    assert_eq!(pushed.accepted, 3, "three rows went in");
    assert_eq!(pushed.refused.len(), 1, "{:?}", pushed.refused);
    assert!(
        pushed.refused[0].contains("offset 3"),
        "{:?}",
        pushed.refused
    );

    assert_eq!(
        contents(&handle),
        vec![(2, 1, 1), (5, 0, 1), (5, 0, 2)],
        "each row carries its message's partition and offset"
    );

    ask(&handle, |reply| Command::Checkpoint { reply }).expect("a checkpoint starts");
    // Shutting down joins the thread committing it, so it is on disk after.
    ask(&handle, |reply| Command::Shutdown { reply });
    drop(handle);
    thread.join().expect("the circuit thread exits");

    let uuid = grasp_dbsp::checkpoint::latest(&config)
        .unwrap_or_else(|d| panic!("{}", grasp_dbsp::diag::render(&d)))
        .expect("a checkpoint");
    let saved = connectors::load(dir.path(), &uuid)
        .expect("reads")
        .expect("the checkpoint saved positions");
    assert_eq!(
        saved["inbox_kafka"],
        Position {
            topic: "inbox".to_string(),
            partitions: BTreeMap::from([(0, 7), (1, 3)]),
        },
        "past the tombstone in partition 0, and not moved back by the refused message"
    );
}

#[test]
fn a_message_with_no_rows_starts_no_transaction() {
    let (handle, thread, _plan) = started(None);
    let before = handle
        .shared
        .completed_steps
        .load(std::sync::atomic::Ordering::Relaxed);
    let pushed = ask(&handle, |reply| Command::PushMessages {
        endpoint: "inbox_kafka".to_string(),
        table: "inbox".to_string(),
        messages: vec![Message {
            partition: 0,
            offset: 0,
            rows: vec![],
        }],
        reply,
    })
    .expect("pushes");
    assert_eq!(pushed.accepted, 0);
    assert_eq!(
        handle
            .shared
            .completed_steps
            .load(std::sync::atomic::Ordering::Relaxed),
        before,
        "nothing was pushed, so there was nothing to step for"
    );
    ask(&handle, |reply| Command::Shutdown { reply });
    drop(handle);
    thread.join().expect("exits");
}

#[actix_web::test]
async fn ingress_into_a_table_a_connector_feeds_is_refused() {
    let (mut handle, _thread, _plan) = started(None);
    handle.connectors = Arc::new(vec![Connector {
        endpoint: "inbox_kafka".to_string(),
        table: "inbox".to_string(),
        topic: "raft-node-1".to_string(),
        metrics: Arc::new(Metrics::default()),
    }]);
    let state = web::Data::new(State {
        handle,
        pipeline: "grasp".to_string(),
        keepalive: Duration::from_secs(3600),
    });
    let app = actix_web::test::init_service(
        App::new()
            .app_data(state.clone())
            .configure(http::configure),
    )
    .await;

    let req = actix_web::test::TestRequest::post()
        .uri("/ingress/inbox?format=json")
        .set_payload(r#"{"insert": {"v": 1}}"#)
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = actix_web::test::read_body_json(resp).await;
    let message = body["message"].as_str().expect("a message");
    assert!(
        message.contains("fed by input `inbox_kafka`") && message.contains("raft-node-1"),
        "{message}"
    );

    let req = actix_web::test::TestRequest::get()
        .uri("/stats")
        .to_request();
    let stats: serde_json::Value = actix_web::test::call_and_read_body_json(&app, req).await;
    assert_eq!(stats["inputs"][0]["endpoint_name"], "inbox_kafka");
    assert_eq!(stats["inputs"][0]["config"]["stream"], "inbox");
    assert_eq!(stats["inputs"][0]["metrics"]["num_parse_errors"], 0);
}
