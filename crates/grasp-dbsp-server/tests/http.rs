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
use grasp_dbsp_runner::lower::{Runner, RunnerConfig};
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
    let plan = grasp_dbsp_runner::compile(SOURCE)
        .unwrap_or_else(|d| panic!("compiles: {}", grasp_dbsp_runner::diag::render(&d)));
    let views: Vec<String> = plan.views().into_iter().map(str::to_string).collect();
    let runner = Runner::build(&plan, &views, RunnerConfig::default())
        .unwrap_or_else(|d| panic!("builds: {}", grasp_dbsp_runner::diag::render(&d)));
    let shapes: HashMap<_, _> = views
        .iter()
        .filter_map(|v| grasp_dbsp_runner::lower::shape(&plan, v).map(|t| (v.clone(), t.clone())))
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
