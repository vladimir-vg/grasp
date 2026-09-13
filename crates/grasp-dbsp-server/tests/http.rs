//! The routes, through actix and not through a socket.
//!
//! `actix_web::test::init_service` builds the application and calls it
//! directly, so there is no port to bind, no address already in use, and no
//! listener race. A test that binds a port can fail for reasons that have
//! nothing to do with the code, and a test that fails for unrelated reasons
//! stops being read.
//!
//! **What is tested here is the HTTP layer and only that**: parameter
//! handling, status codes, the error envelope, and the two path spellings.
//! Everything about *when* a transaction runs, what a completion token means
//! and why a snapshot does not overlap the stream is in `tests/threading.rs`,
//! where it can be asserted without a request at all.
//!
//! **Egress is absent from this file, deliberately.** Its response never ends —
//! that is what a subscription is — so `test::call_service` would hand back a
//! body that cannot be collected, and a test that reads a fixed number of
//! chunks and drops it is asserting the framing rather than the behaviour. The
//! behaviour is `tests/threading.rs`; the framing is one round trip in
//! `tests/socket.rs`, against a real listener, because `init_service` does not
//! exercise chunked transfer at all.

use actix_web::{App, test, web};
use grasp_dbsp::lower::{Runner, RunnerConfig};
use grasp_dbsp_server::circuit;
use grasp_dbsp_server::http::{self, State};
use serde_json::Value as J;
use std::collections::HashMap;
use std::time::Duration;

const SOURCE: &str = "\
orders := input(\"orders\")
orders :: zset(record(id: i64, total: f64))
big := filter(orders, function((r) -> r.total > 100.0))
";

/// Builds the application over a real circuit, with the keepalive pushed past
/// any test's lifetime so nothing arrives that a test did not cause.
fn state(materialized: &[&str]) -> (web::Data<State>, std::thread::JoinHandle<()>) {
    let plan = grasp_dbsp::compile(SOURCE)
        .unwrap_or_else(|d| panic!("compiles: {}", grasp_dbsp::diag::render(&d)));
    let views: Vec<String> = plan.views().into_iter().map(str::to_string).collect();
    let runner = Runner::build(&plan, &views, RunnerConfig::default())
        .unwrap_or_else(|d| panic!("builds: {}", grasp_dbsp::diag::render(&d)));
    let shapes: HashMap<_, _> = views
        .iter()
        .filter_map(|v| grasp_dbsp::lower::shape(&plan, v).map(|t| (v.clone(), t.clone())))
        .collect();
    let tables: Vec<String> = plan
        .inputs()
        .into_iter()
        .map(|(_, t)| t.to_string())
        .collect();
    let materialized: Vec<String> = materialized.iter().map(|s| s.to_string()).collect();
    let (handle, thread) = circuit::start(runner, shapes, &materialized, tables, true);
    (
        web::Data::new(State {
            handle,
            pipeline: "shop".to_string(),
            keepalive: Duration::from_secs(3600),
        }),
        thread,
    )
}

macro_rules! app {
    ($state:expr) => {
        test::init_service(
            App::new()
                .app_data($state.clone())
                .configure(http::configure),
        )
        .await
    };
}

#[actix_web::test]
async fn a_row_ingests_and_yields_a_token_that_resolves() {
    let (state, _t) = state(&[]);
    let app = app!(state);

    let req = test::TestRequest::post()
        .uri("/v0/pipelines/shop/ingress/orders?format=json")
        .set_payload(r#"{"insert": {"id": 1, "total": 250.0}}"#)
        .to_request();
    let body: J = test::call_and_read_body_json(&app, req).await;
    let token = body["token"].as_str().expect("a token").to_string();

    let req = test::TestRequest::get()
        .uri(&format!(
            "/v0/pipelines/shop/completion_status?token={token}"
        ))
        .to_request();
    let status: J = test::call_and_read_body_json(&app, req).await;
    assert_eq!(
        status["status"], "complete",
        "the token should be complete once the transaction it names has run"
    );
}

#[actix_web::test]
async fn both_path_spellings_reach_the_same_pipeline() {
    let (state, _t) = state(&[]);
    let app = app!(state);

    for uri in ["/stats", "/v0/pipelines/shop/stats"] {
        let req = test::TestRequest::get().uri(uri).to_request();
        let response = test::call_service(&app, req).await;
        assert!(
            response.status().is_success(),
            "`{uri}` answered {}",
            response.status()
        );
    }
}

#[actix_web::test]
async fn another_pipelines_name_is_not_this_one() {
    let (state, _t) = state(&[]);
    let app = app!(state);

    let req = test::TestRequest::get()
        .uri("/v0/pipelines/somewhere-else/stats")
        .to_request();
    let response = test::call_service(&app, req).await;
    assert_eq!(response.status(), 404);
}

#[actix_web::test]
async fn csv_is_refused_and_says_what_to_pass_instead() {
    let (state, _t) = state(&[]);
    let app = app!(state);

    let req = test::TestRequest::post()
        .uri("/ingress/orders?format=csv")
        .set_payload("1,2")
        .to_request();
    let response = test::call_service(&app, req).await;
    assert_eq!(response.status(), 400);
    let body: J = test::read_body_json(response).await;
    assert_eq!(body["error_code"], "InvalidParam");
    assert!(
        body["message"].as_str().unwrap().contains("format=json"),
        "the message should name the format to pass: {}",
        body["message"]
    );
}

#[actix_web::test]
async fn an_unknown_table_is_a_404_with_feldera_s_error_code() {
    let (state, _t) = state(&[]);
    let app = app!(state);

    let req = test::TestRequest::post()
        .uri("/ingress/nonesuch?format=json")
        .set_payload("{}")
        .to_request();
    let response = test::call_service(&app, req).await;
    assert_eq!(response.status(), 404);
    let body: J = test::read_body_json(response).await;
    assert_eq!(body["error_code"], "UnknownInputTable");
}

/// The output half of the same guarantee, and it is a separate test because the
/// two codes came from different places and only one of them used to be
/// reachable. An unknown *view* travelled as `Fault::NoSuchName` with no word
/// for what kind of name it was, so it fell through to `UnknownPipelineName` —
/// a client asking for a view it had misspelled was told the pipeline did not
/// exist, and `UnknownOutputTable` was never emitted at all.
#[actix_web::test]
async fn an_unknown_view_is_a_404_about_the_view_and_not_about_the_pipeline() {
    let (state, _t) = state(&[]);
    let app = app!(state);

    let req = test::TestRequest::post()
        .uri("/egress/nonesuch?format=json")
        .to_request();
    let response = test::call_service(&app, req).await;
    assert_eq!(response.status(), 404);
    let body: J = test::read_body_json(response).await;
    assert_eq!(body["error_code"], "UnknownOutputTable");
    let message = body["message"].as_str().expect("a message");
    assert!(
        message.contains("view") && message.contains("nonesuch"),
        "the message should name the view and call it one: {message}"
    );
}

/// Feldera ingests the rows that parsed and reports the ones that did not, and
/// this has to be matched rather than improved on: a client that retried the
/// whole batch after a 400 would double-insert the good rows.
#[actix_web::test]
async fn a_bad_row_among_good_ones_reports_and_still_ingests_the_good() {
    let (state, _t) = state(&[]);
    let app = app!(state);

    let before: J =
        test::call_and_read_body_json(&app, test::TestRequest::get().uri("/stats").to_request())
            .await;

    let req = test::TestRequest::post()
        .uri("/ingress/orders?format=json")
        .set_payload(
            "{\"insert\": {\"id\": 1, \"total\": 250.0}}\n\
             {\"insert\": {\"id\": \"not a number\", \"total\": 1.0}}",
        )
        .to_request();
    let response = test::call_service(&app, req).await;
    assert_eq!(response.status(), 400);
    let body: J = test::read_body_json(response).await;
    assert_eq!(body["error_code"], "ParseErrors");
    assert_eq!(body["details"]["num_errors"], 1);

    let after: J =
        test::call_and_read_body_json(&app, test::TestRequest::get().uri("/stats").to_request())
            .await;
    let count = |s: &J| s["global_metrics"]["total_input_records"].as_u64().unwrap();
    assert_eq!(
        count(&after),
        count(&before) + 1,
        "the row that parsed should have been ingested despite the 400"
    );
}

#[actix_web::test]
async fn a_weighted_row_ingests_where_feldera_would_panic() {
    // Feldera's weighted *parser* is a `todo!()`
    // (`adapters/src/format/json/input.rs:163-169`), so this is a divergence in
    // our favour and worth pinning, or somebody will "fix" it to match.
    let (state, _t) = state(&[]);
    let app = app!(state);

    let req = test::TestRequest::post()
        .uri("/ingress/orders?format=json&update_format=weighted")
        .set_payload(r#"{"weight": 3, "data": {"id": 1, "total": 250.0}}"#)
        .to_request();
    let response = test::call_service(&app, req).await;
    assert!(
        response.status().is_success(),
        "a weighted row was refused: {}",
        response.status()
    );
}

#[actix_web::test]
async fn a_transaction_is_visible_in_stats_and_does_not_nest() {
    let (state, _t) = state(&[]);
    let app = app!(state);

    let body: J = test::call_and_read_body_json(
        &app,
        test::TestRequest::post()
            .uri("/start_transaction")
            .to_request(),
    )
    .await;
    assert_eq!(body["transaction_id"], 1);

    let stats: J =
        test::call_and_read_body_json(&app, test::TestRequest::get().uri("/stats").to_request())
            .await;
    assert_eq!(
        stats["global_metrics"]["transaction_status"],
        "TransactionInProgress"
    );

    // Feldera answers 409 for this, not 400.
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/start_transaction")
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), 409);

    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/commit_transaction")
            .to_request(),
    )
    .await;
    assert!(response.status().is_success());
}

#[actix_web::test]
async fn a_commit_with_nothing_open_is_a_diagnostic_not_a_panic() {
    let (state, _t) = state(&[]);
    let app = app!(state);

    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/commit_transaction")
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), 400);
}

#[actix_web::test]
async fn a_token_from_another_run_is_refused_by_name() {
    let (state, _t) = state(&[]);
    let app = app!(state);

    let stale = grasp_dbsp_server::token::Token::new(uuid::Uuid::new_v4(), 1).encode();
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/completion_status?token={stale}"))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), 400);
    let body: J = test::read_body_json(response).await;
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("an earlier run of the pipeline"),
        "the message should name the restart: {}",
        body["message"]
    );
}

#[actix_web::test]
async fn pausing_is_accepted_and_says_what_changed() {
    let (state, _t) = state(&[]);
    let app = app!(state);

    let response =
        test::call_service(&app, test::TestRequest::post().uri("/pause").to_request()).await;
    assert_eq!(response.status(), 202);
    let body: J = test::read_body_json(response).await;
    assert_eq!(body, "Pipeline transitioning from Running to Paused");

    // The pipeline serves GET and the manager POSTs; both are accepted,
    // because the difference is Feldera's own inconsistency.
    let response =
        test::call_service(&app, test::TestRequest::get().uri("/start").to_request()).await;
    assert_eq!(response.status(), 202);
}

#[actix_web::test]
async fn metadata_lists_what_may_be_written_to_and_watched() {
    let (state, _t) = state(&["big"]);
    let app = app!(state);

    let body: J = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri("/v0/pipelines/shop/metadata")
            .to_request(),
    )
    .await;
    assert_eq!(body["name"], "shop");
    assert_eq!(body["tables"], serde_json::json!(["orders"]));
    assert_eq!(body["views"], serde_json::json!(["big", "orders"]));
    assert_eq!(body["materialized"], serde_json::json!(["big"]));
}

/// Tests that open a storage directory run one at a time, as
/// `grasp-dbsp`'s `tests/storage.rs` does.
static STORAGE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `state`, over a circuit with a storage directory to checkpoint into.
fn state_with_storage(path: &std::path::Path) -> (web::Data<State>, std::thread::JoinHandle<()>) {
    use dbsp::circuit::{CircuitStorageConfig, StorageCacheConfig, StorageConfig, StorageOptions};
    let plan = grasp_dbsp::compile(SOURCE)
        .unwrap_or_else(|d| panic!("compiles: {}", grasp_dbsp::diag::render(&d)));
    let views: Vec<String> = plan.views().into_iter().map(str::to_string).collect();
    let storage = CircuitStorageConfig::for_config(
        StorageConfig {
            path: path.to_string_lossy().into_owned(),
            cache: StorageCacheConfig::default(),
        },
        StorageOptions::default(),
    )
    .expect("a storage backend");
    let runner = Runner::build(
        &plan,
        &views,
        RunnerConfig {
            storage: Some(storage),
            ..RunnerConfig::default()
        },
    )
    .unwrap_or_else(|d| panic!("builds: {}", grasp_dbsp::diag::render(&d)));
    let shapes: HashMap<_, _> = views
        .iter()
        .filter_map(|v| grasp_dbsp::lower::shape(&plan, v).map(|t| (v.clone(), t.clone())))
        .collect();
    let tables: Vec<String> = plan
        .inputs()
        .into_iter()
        .map(|(_, t)| t.to_string())
        .collect();
    let (handle, thread) = circuit::start(runner, shapes, &[], tables, true);
    (
        web::Data::new(State {
            handle,
            pipeline: "shop".to_string(),
            keepalive: Duration::from_secs(3600),
        }),
        thread,
    )
}

/// The whole checkpoint conversation, in Feldera's shapes: the request answers
/// with a sequence number before anything is durable, the status reports that
/// number once it has landed, and the catalog then lists it.
#[actix_web::test]
async fn a_checkpoint_is_acknowledged_then_reported_then_listed() {
    let _lock = STORAGE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::TempDir::new().expect("a storage directory");
    let (state, _t) = state_with_storage(dir.path());
    let app = app!(state);

    let req = test::TestRequest::post()
        .uri("/ingress/orders?format=json")
        .set_payload(r#"{"insert": {"id": 1, "total": 250.0}}"#)
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 200);

    let req = test::TestRequest::post().uri("/checkpoint").to_request();
    let body: J = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["checkpoint_sequence_number"], 1);
    let incarnation = body["incarnation_uuid"]
        .as_str()
        .expect("an incarnation")
        .to_string();

    let mut landed = false;
    for _ in 0..200 {
        let req = test::TestRequest::get()
            .uri(&format!(
                "/checkpoint_status?incarnation_uuid={incarnation}"
            ))
            .to_request();
        let status: J = test::call_and_read_body_json(&app, req).await;
        assert!(
            status["failure"].is_null(),
            "the checkpoint failed: {status}"
        );
        if status["success"] == 1 {
            landed = true;
            break;
        }
        actix_web::rt::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(landed, "checkpoint 1 never reported success");

    let req = test::TestRequest::get().uri("/checkpoints").to_request();
    let list: J = test::call_and_read_body_json(&app, req).await;
    let list = list.as_array().expect("a list");
    assert_eq!(list.len(), 1, "{list:?}");
    assert!(list[0]["uuid"].is_string(), "{list:?}");
}

/// A client polling across a restart is told so, in Feldera's words, rather
/// than being shown a status that belongs to a different run.
#[actix_web::test]
async fn a_status_asked_of_another_incarnation_is_refused() {
    let (state, _t) = state(&[]);
    let app = app!(state);
    let req = test::TestRequest::get()
        .uri("/checkpoint_status?incarnation_uuid=00000000-0000-0000-0000-000000000000")
        .to_request();
    let response = test::call_service(&app, req).await;
    assert_eq!(response.status(), 400);
    let body: J = test::read_body_json(response).await;
    assert_eq!(body["error_code"], "IncarnationUuidMismatch");
}

/// Without storage there is nowhere to write one, and the refusal says what to
/// configure — a 400, not the 409 that means "not now".
#[actix_web::test]
async fn a_checkpoint_without_storage_says_what_to_configure() {
    let (state, _t) = state(&[]);
    let app = app!(state);
    let req = test::TestRequest::post().uri("/checkpoint").to_request();
    let response = test::call_service(&app, req).await;
    assert_eq!(response.status(), 400);
    let body: J = test::read_body_json(response).await;
    let message = body["message"].as_str().expect("a message");
    assert!(message.contains("storage_config"), "{message}");
}

/// A checkpoint is a snapshot between transactions, so one requested inside a
/// transaction is Feldera's 409.
#[actix_web::test]
async fn a_checkpoint_inside_a_transaction_is_a_conflict() {
    let _lock = STORAGE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::TempDir::new().expect("a storage directory");
    let (state, _t) = state_with_storage(dir.path());
    let app = app!(state);

    let req = test::TestRequest::post()
        .uri("/start_transaction")
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 200);

    let req = test::TestRequest::post().uri("/checkpoint").to_request();
    let response = test::call_service(&app, req).await;
    assert_eq!(response.status(), 409);
    let body: J = test::read_body_json(response).await;
    assert_eq!(body["error_code"], "TransactionInProgress");
}

/// A circuit over a storage directory, materializing `materialized`, and — given
/// a checkpoint uuid — restored from it the way `main.rs` does: build first, so
/// the manifest is accepted before the saved rows are read.
fn storage_state(
    path: &std::path::Path,
    materialized: &[&str],
    resume: Option<&str>,
) -> (web::Data<State>, std::thread::JoinHandle<()>) {
    use dbsp::circuit::{CircuitStorageConfig, StorageCacheConfig, StorageConfig, StorageOptions};
    let plan = grasp_dbsp::compile(SOURCE)
        .unwrap_or_else(|d| panic!("compiles: {}", grasp_dbsp::diag::render(&d)));
    let views: Vec<String> = plan.views().into_iter().map(str::to_string).collect();
    let mut storage = CircuitStorageConfig::for_config(
        StorageConfig {
            path: path.to_string_lossy().into_owned(),
            cache: StorageCacheConfig::default(),
        },
        StorageOptions::default(),
    )
    .expect("a storage backend");
    if let Some(uuid) = resume {
        storage = storage.with_init_checkpoint(Some(uuid.parse().expect("a uuid")));
    }
    let runner = Runner::build(
        &plan,
        &views,
        RunnerConfig {
            storage: Some(storage),
            ..RunnerConfig::default()
        },
    )
    .unwrap_or_else(|d| panic!("builds: {}", grasp_dbsp::diag::render(&d)));
    let materialized: Vec<String> = materialized.iter().map(|s| s.to_string()).collect();
    let restored = match resume {
        Some(uuid) => grasp_dbsp_server::snapshot::load(path, uuid, &materialized)
            .expect("loads the saved views"),
        None => HashMap::new(),
    };
    let shapes: HashMap<_, _> = views
        .iter()
        .filter_map(|v| grasp_dbsp::lower::shape(&plan, v).map(|t| (v.clone(), t.clone())))
        .collect();
    let tables: Vec<String> = plan
        .inputs()
        .into_iter()
        .map(|(_, t)| t.to_string())
        .collect();
    let (handle, thread) =
        circuit::start_restored(runner, shapes, &materialized, tables, true, restored);
    (
        web::Data::new(State {
            handle,
            pipeline: "shop".to_string(),
            keepalive: Duration::from_secs(3600),
        }),
        thread,
    )
}

/// Takes a checkpoint through the API, waits for it to land, and returns its
/// uuid from the catalog.
macro_rules! take_checkpoint {
    ($app:expr) => {{
        let req = test::TestRequest::post().uri("/checkpoint").to_request();
        let body: J = test::call_and_read_body_json(&$app, req).await;
        let sequence = body["checkpoint_sequence_number"].clone();
        let mut landed = false;
        for _ in 0..200 {
            let req = test::TestRequest::get()
                .uri("/checkpoint_status")
                .to_request();
            let status: J = test::call_and_read_body_json(&$app, req).await;
            assert!(
                status["failure"].is_null(),
                "the checkpoint failed: {status}"
            );
            if status["success"] == sequence {
                landed = true;
                break;
            }
            actix_web::rt::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(landed, "the checkpoint never landed");
        let req = test::TestRequest::get().uri("/checkpoints").to_request();
        let list: J = test::call_and_read_body_json(&$app, req).await;
        list.as_array()
            .and_then(|l| l.last())
            .and_then(|c| c["uuid"].as_str())
            .expect("a checkpoint uuid")
            .to_string()
    }};
}

/// A snapshot of a restored view starts from the rows the checkpoint held.
///
/// The fold behind `send_snapshot=true` lives outside the circuit, and a
/// restored circuit replays nothing — so before the rows were saved with the
/// checkpoint, this snapshot came back empty while the circuit held a row.
#[actix_web::test]
async fn a_restored_snapshot_starts_from_the_rows_the_checkpoint_held() {
    let _lock = STORAGE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::TempDir::new().expect("a storage directory");

    let uuid = {
        let (state, thread) = storage_state(dir.path(), &["big"], None);
        let app = app!(state);
        for row in [
            r#"{"insert": {"id": 1, "total": 250.0}}"#,
            r#"{"insert": {"id": 2, "total": 50.0}}"#,
        ] {
            let req = test::TestRequest::post()
                .uri("/ingress/orders?format=json")
                .set_payload(row)
                .to_request();
            assert_eq!(test::call_service(&app, req).await.status(), 200);
        }
        let uuid = take_checkpoint!(app);
        // Every handle to the circuit dropped, so its thread exits and releases
        // the storage directory for the restored one.
        drop(app);
        drop(state);
        thread.join().expect("the circuit thread exits");
        uuid
    };

    let (state, _t) = storage_state(dir.path(), &["big"], Some(&uuid));
    let subscription = state
        .handle
        .ask(|reply| circuit::Command::Subscribe {
            view: "big".to_string(),
            snapshot: true,
            backpressure: false,
            reply,
        })
        .await
        .expect("asks")
        .expect("subscribes");
    let rows = subscription.snapshot.expect("a snapshot");
    assert_eq!(rows.len(), 1, "only the order over 100 is big: {rows:?}");
}

/// A view materialized now and not saved then is refused, not started empty.
#[actix_web::test]
async fn a_view_the_checkpoint_did_not_save_is_refused_on_resume() {
    let _lock = STORAGE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::TempDir::new().expect("a storage directory");

    let uuid = {
        let (state, thread) = storage_state(dir.path(), &[], None);
        let app = app!(state);
        let uuid = take_checkpoint!(app);
        drop(app);
        drop(state);
        thread.join().expect("the circuit thread exits");
        uuid
    };

    let err = grasp_dbsp_server::snapshot::load(dir.path(), &uuid, &["big".to_string()])
        .expect_err("refused");
    assert!(err.contains("taken without `big`"), "{err}");
}

/// An input table the runtime numbers, beside one it does not.
const PARTITIONED: &str = "\
inbox := input(\"inbox\", partition_as: \"p\", offset_as: \"o\")
inbox :: zset(record(o: i64, p: i64, term: i64))
plain := input(\"plain\")
plain :: zset(record(term: i64))
";

fn partitioned_state() -> (web::Data<State>, std::thread::JoinHandle<()>) {
    let plan = grasp_dbsp::compile(PARTITIONED)
        .unwrap_or_else(|d| panic!("compiles: {}", grasp_dbsp::diag::render(&d)));
    let views: Vec<String> = plan.views().into_iter().map(str::to_string).collect();
    let runner = Runner::build(&plan, &views, RunnerConfig::default())
        .unwrap_or_else(|d| panic!("builds: {}", grasp_dbsp::diag::render(&d)));
    let shapes: HashMap<_, _> = views
        .iter()
        .filter_map(|v| grasp_dbsp::lower::shape(&plan, v).map(|t| (v.clone(), t.clone())))
        .collect();
    let tables: Vec<String> = plan
        .inputs()
        .into_iter()
        .map(|(_, t)| t.to_string())
        .collect();
    let (handle, thread) = circuit::start(runner, shapes, &[], tables, true);
    (
        web::Data::new(State {
            handle,
            pipeline: "shop".to_string(),
            keepalive: Duration::from_secs(3600),
        }),
        thread,
    )
}

/// `(offset, partition, term)` for every row of the next chunk on `inbox`.
async fn next_inbox_rows(
    chunks: &mut tokio::sync::mpsc::Receiver<circuit::Chunk>,
) -> Vec<(i64, i64, i64)> {
    let chunk = actix_web::rt::time::timeout(Duration::from_secs(5), chunks.recv())
        .await
        .expect("a chunk within five seconds")
        .expect("the stream is open");
    chunk
        .deltas
        .iter()
        .map(|d| match &d.key {
            grasp_dbsp::value::DynValue::Record(f) => match (&f[0], &f[1], &f[2]) {
                (
                    grasp_dbsp::value::DynValue::I64(o),
                    grasp_dbsp::value::DynValue::I64(p),
                    grasp_dbsp::value::DynValue::I64(t),
                ) => (*o, *p, *t),
                other => panic!("three i64s, found {other:?}"),
            },
            other => panic!("a record, found {other:?}"),
        })
        .collect()
}

/// `partition=` names the partition; leaving it out takes the runtime's choice,
/// and on a table with no partition column it is ignored.
#[actix_web::test]
async fn ingress_takes_a_partition_and_the_runtime_fills_the_columns() {
    let (state, _t) = partitioned_state();
    let app = app!(state);
    let mut subscription = state
        .handle
        .ask(|reply| circuit::Command::Subscribe {
            view: "inbox".to_string(),
            snapshot: false,
            backpressure: false,
            reply,
        })
        .await
        .expect("asks")
        .expect("subscribes");

    let req = test::TestRequest::post()
        .uri("/ingress/inbox?format=json")
        .set_payload(r#"{"insert": {"term": 7}}"#)
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 200);
    assert_eq!(
        next_inbox_rows(&mut subscription.chunks).await,
        vec![(0, 0, 7)],
        "no partition named: the runtime's choice, and the first offset in it"
    );

    let req = test::TestRequest::post()
        .uri("/ingress/inbox?format=json&partition=3")
        .set_payload(r#"{"insert": {"term": 8}}"#)
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 200);
    assert_eq!(
        next_inbox_rows(&mut subscription.chunks).await,
        vec![(0, 3, 8)],
        "partition 3 counts from its own zero"
    );

    let req = test::TestRequest::post()
        .uri("/ingress/plain?format=json&partition=5")
        .set_payload(r#"{"insert": {"term": 1}}"#)
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        200,
        "a plain table ignores a partition rather than refusing it"
    );
}

/// The columns the runtime fills are not the client's to supply.
#[actix_web::test]
async fn a_row_that_supplies_a_runtime_column_is_refused() {
    let (state, _t) = partitioned_state();
    let app = app!(state);
    let req = test::TestRequest::post()
        .uri("/ingress/inbox?format=json")
        .set_payload(r#"{"insert": {"term": 7, "o": 41}}"#)
        .to_request();
    let response = test::call_service(&app, req).await;
    assert_eq!(response.status(), 400);
    let body: J = test::read_body_json(response).await;
    assert_eq!(body["error_code"], "ParseErrors");
    assert!(body.to_string().contains("unknown field `o`"), "{body}");
}
