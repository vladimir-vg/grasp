//! The routes, and the chunked egress stream.
//!
//! Every path is mounted twice: bare, as a Feldera *pipeline* serves it, and
//! under `/v0/pipelines/{name}/`, as a Feldera *manager* proxies it. The
//! manager adds no semantics — it is a byte-for-byte streaming proxy
//! (`pipeline-manager/src/api/endpoints/pipeline_interaction.rs:107-121`) — so
//! one set of handlers answers both, and a client pointed at this process as if
//! it were a manager works. The manager spelling is the one the documented curl
//! examples, the Python SDK and `fda` use, which is why it is there at all.
//!
//! Where the two disagree on method — the pipeline serves `GET /start`, the
//! manager `POST /start` — both are accepted, because refusing one would break
//! a client for a difference that is Feldera's own inconsistency.

use crate::circuit::{self, Command, Fault, Handle};
use crate::token::Token;
use crate::wire::{ApiError, parse_error};
use actix_web::{HttpResponse, Responder, web};
use grasp_dbsp_runner::json::{
    Format, decode_delta, decode_delta_insert_delete, encode_delta, encode_delta_insert_delete,
};
use grasp_dbsp_runner::lower::Delta;
use grasp_dbsp_runner::value::BatchType;
use serde::Deserialize;
use serde_json::{Value as J, json};
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Everything a handler needs.
pub struct State {
    pub handle: Handle,
    pub pipeline: String,
    /// How long an egress connection waits before sending a payload-free
    /// chunk. Feldera hard-codes three seconds and explains why
    /// (`transport/http/output.rs:238-247`): actix does not notice a
    /// disconnected client on an idle stream, so without this a closed tab
    /// holds a subscription for ever. It is a field so a test can push it past
    /// the test's own lifetime and observe only the chunks it caused.
    pub keepalive: Duration,
}

impl From<Fault> for ApiError {
    fn from(f: Fault) -> ApiError {
        match f {
            Fault::NoSuchName(name) => ApiError::UnknownName {
                what: "relation",
                name,
            },
            Fault::Gone(m) => ApiError::Gone(m),
            Fault::Refused(m) => ApiError::Refused(m),
        }
    }
}

/// Mounts every route, bare and under the manager's prefix.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/v0/pipelines/{pipeline}")
            .service(routes())
            .default_service(web::to(unknown_pipeline)),
    )
    .service(routes());
}

fn routes() -> actix_web::Scope {
    web::scope("")
        .route("/ingress/{table}", web::post().to(ingress))
        .route("/egress/{view}", web::post().to(egress))
        .route("/start", web::get().to(start))
        .route("/start", web::post().to(start))
        .route("/resume", web::post().to(start))
        .route("/pause", web::get().to(pause))
        .route("/pause", web::post().to(pause))
        .route("/stop", web::post().to(stop))
        .route("/start_transaction", web::post().to(start_transaction))
        .route("/commit_transaction", web::post().to(commit_transaction))
        .route("/completion_status", web::get().to(completion_status))
        .route("/stats", web::get().to(stats))
        .route("/metadata", web::get().to(metadata))
}

/// A request under `/v0/pipelines/{name}/` for a name this process does not
/// answer to — what a manager would say about a pipeline it had never heard of.
async fn unknown_pipeline(req: actix_web::HttpRequest) -> Result<HttpResponse, ApiError> {
    Err(ApiError::UnknownName {
        what: "pipeline",
        name: req
            .match_info()
            .get("pipeline")
            .unwrap_or_default()
            .to_string(),
    })
}

/// Checks the `{pipeline}` segment, where there is one.
fn named(state: &State, path: &actix_web::HttpRequest) -> Result<(), ApiError> {
    match path.match_info().get("pipeline") {
        Some(name) if name != state.pipeline => Err(ApiError::UnknownName {
            what: "pipeline",
            name: name.to_string(),
        }),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Ingress
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct IngressArgs {
    #[serde(default = "default_format")]
    format: String,
    #[serde(default = "default_update_format")]
    update_format: String,
    #[serde(default)]
    array: bool,
}

fn default_format() -> String {
    "json".to_string()
}

fn default_update_format() -> String {
    "insert_delete".to_string()
}

async fn ingress(
    state: web::Data<State>,
    req: actix_web::HttpRequest,
    path: web::Path<(String,)>,
    args: web::Query<IngressArgs>,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    named(&state, &req)?;
    let table = req.match_info().get("table").unwrap_or(&path.0).to_string();

    check_format(&args.format)?;
    let format = update_format(&args.update_format)?;

    if !state.handle.tables.contains(&table) {
        return Err(ApiError::UnknownName {
            what: "table",
            name: table,
        });
    }
    let shape = state
        .handle
        .shapes
        .get(&table)
        .ok_or_else(|| ApiError::UnknownName {
            what: "table",
            name: table.clone(),
        })?;
    let BatchType::ZSet(row_type) = shape else {
        return Err(ApiError::InvalidParam(format!(
            "`{table}` is an indexed relation; only a flat input table can be ingested into"
        )));
    };

    // Feldera splits a body into values with a brace-depth scanner and accepts
    // whitespace, newlines or nothing between them (`format/json/input.rs:426-
    // 454`). `StreamDeserializer` accepts exactly the same shape, which is why
    // `lines=single` and `lines=multiple` are both honoured by doing nothing
    // differently.
    let text = std::str::from_utf8(&body)
        .map_err(|e| ApiError::InvalidParam(format!("the body is not UTF-8: {e}")))?;

    let mut rows = Vec::new();
    let mut errors = Vec::new();
    let mut n = 0usize;
    let push = |value: &J, n: usize, errors: &mut Vec<J>, rows: &mut Vec<_>| {
        let decoded = match format {
            Format::Weighted => decode_delta(value, row_type),
            Format::InsertDelete => decode_delta_insert_delete(value, row_type),
        };
        match decoded {
            Ok((row, weight)) => rows.push((row, weight)),
            Err(e) => errors.push(parse_error(n, e.0, &value.to_string())),
        }
    };

    for value in serde_json::Deserializer::from_str(text).into_iter::<J>() {
        n += 1;
        match value {
            Ok(J::Array(items)) if args.array => {
                for item in &items {
                    push(item, n, &mut errors, &mut rows);
                }
            }
            Ok(value) => push(&value, n, &mut errors, &mut rows),
            Err(e) => errors.push(parse_error(n, e.to_string(), "")),
        }
    }

    // The rows that parsed are pushed even when others did not — Feldera's
    // behaviour, and the one a retrying client depends on.
    let accepted = if rows.is_empty() {
        None
    } else {
        Some(
            state
                .handle
                .ask(|reply| Command::Push {
                    table: table.clone(),
                    rows,
                    reply,
                })
                .await??,
        )
    };

    if !errors.is_empty() {
        return Err(ApiError::ParseErrors {
            total: errors.len(),
            errors,
        });
    }

    // An empty body is not an error: it yields a token for everything accepted
    // so far, which is how a client asks "has everything I sent landed?".
    let count = match accepted {
        Some(c) => c,
        None => state.handle.accepted_now(),
    };
    Ok(HttpResponse::Ok().json(json!({
        "token": Token::new(state.handle.shared.incarnation, count).encode()
    })))
}

fn check_format(format: &str) -> Result<(), ApiError> {
    match format {
        "json" => Ok(()),
        "csv" => Err(ApiError::InvalidParam(
            "`format=csv` is not implemented: this runtime encodes and decodes the two \
             Feldera JSON formats and nothing else. Pass `format=json`. (Feldera's own \
             default is csv, so a client that omits the parameter works there and not \
             here.)"
                .to_string(),
        )),
        other => Err(ApiError::InvalidParam(format!(
            "`format={other}` is not a format this runtime has. Pass `format=json`."
        ))),
    }
}

fn update_format(name: &str) -> Result<Format, ApiError> {
    match name {
        "insert_delete" => Ok(Format::InsertDelete),
        "weighted" => Ok(Format::Weighted),
        other => Err(ApiError::InvalidParam(format!(
            "`update_format={other}` is not implemented. This runtime has \
             `insert_delete` and `weighted`."
        ))),
    }
}

// ---------------------------------------------------------------------------
// Egress
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct EgressArgs {
    #[serde(default = "default_format")]
    format: String,
    #[serde(default = "default_update_format")]
    update_format: String,
    #[serde(default)]
    send_snapshot: bool,
    #[serde(default)]
    backpressure: bool,
}

async fn egress(
    state: web::Data<State>,
    req: actix_web::HttpRequest,
    path: web::Path<(String,)>,
    args: web::Query<EgressArgs>,
) -> Result<HttpResponse, ApiError> {
    named(&state, &req)?;
    let view = req.match_info().get("view").unwrap_or(&path.0).to_string();

    check_format(&args.format)?;
    let format = update_format(&args.update_format)?;

    let subscription = state
        .handle
        .ask(|reply| Command::Subscribe {
            view: view.clone(),
            snapshot: args.send_snapshot,
            backpressure: args.backpressure,
            reply,
        })
        .await??;

    let keepalive = state.keepalive;
    let shape = subscription.shape.clone();
    let mut chunks = subscription.chunks;
    let snapshot = subscription.snapshot;

    let body = async_stream::stream! {
        let mut sequence: u64 = 0;

        // The snapshot first, flagged, then the live stream. Feldera
        // distinguishes the two with the same per-chunk boolean
        // (`transport/http/output.rs:64-72`).
        if let Some(rows) = snapshot {
            let deltas: Vec<Delta> = rows
                .iter()
                .map(|((key, value), weight)| Delta {
                    key: key.clone(),
                    value: value.clone(),
                    weight: *weight,
                })
                .collect();
            for batch in deltas.chunks(SNAPSHOT_ROWS) {
                yield Ok::<_, actix_web::Error>(frame(sequence, true, batch, &shape, format));
                sequence += 1;
            }
        }

        loop {
            match tokio::time::timeout(keepalive, chunks.recv()).await {
                Ok(Some(chunk)) => {
                    // A dropped chunk burns its sequence number rather than
                    // renumbering, so a client that sees 7 then 9 knows it lost
                    // one. Feldera numbers before the send that may fail.
                    sequence += chunk.skipped;
                    yield Ok(frame(sequence, false, &chunk.deltas, &shape, format));
                    sequence += 1;
                }
                // The circuit thread is gone: end the response rather than
                // holding a connection open on a dead pipeline.
                Ok(None) => break,
                Err(_) => {
                    yield Ok(keepalive_frame(sequence));
                    sequence += 1;
                }
            }
        }
    };

    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .streaming(body))
}

/// How many rows go in one snapshot chunk, so that a large view does not become
/// one enormous JSON object a client must buffer whole.
const SNAPSHOT_ROWS: usize = 1000;

/// One chunk: a JSON object and a CRLF, which is the framing Feldera's own
/// reader expects (`transport/http/output.rs:139-144`).
fn frame(
    sequence: u64,
    snapshot: bool,
    deltas: &[Delta],
    shape: &BatchType,
    format: Format,
) -> web::Bytes {
    let mut data = Vec::new();
    for delta in deltas {
        match format {
            Format::Weighted => {
                if let Ok(j) = encode_delta(delta, shape) {
                    data.push(j);
                }
            }
            Format::InsertDelete => {
                if let Ok(js) = encode_delta_insert_delete(delta, shape) {
                    data.extend(js);
                }
            }
        }
    }
    let mut bytes = serde_json::to_vec(&json!({
        "sequence_number": sequence,
        "snapshot": snapshot,
        "json_data": data,
    }))
    .unwrap_or_default();
    bytes.extend_from_slice(b"\r\n");
    web::Bytes::from(bytes)
}

fn keepalive_frame(sequence: u64) -> web::Bytes {
    let mut bytes = serde_json::to_vec(&json!({
        "sequence_number": sequence,
        "snapshot": false,
    }))
    .unwrap_or_default();
    bytes.extend_from_slice(b"\r\n");
    web::Bytes::from(bytes)
}

// ---------------------------------------------------------------------------
// Lifecycle, transactions, observation
// ---------------------------------------------------------------------------

async fn start(
    state: web::Data<State>,
    req: actix_web::HttpRequest,
) -> Result<impl Responder, ApiError> {
    named(&state, &req)?;
    set_running(&state, true).await
}

async fn pause(
    state: web::Data<State>,
    req: actix_web::HttpRequest,
) -> Result<impl Responder, ApiError> {
    named(&state, &req)?;
    set_running(&state, false).await
}

async fn set_running(state: &State, running: bool) -> Result<HttpResponse, ApiError> {
    let was = state
        .handle
        .ask(|reply| Command::SetRunning { running, reply })
        .await?;
    let name = |r: bool| if r { "Running" } else { "Paused" };
    Ok(HttpResponse::Accepted().json(format!(
        "Pipeline transitioning from {} to {}",
        name(was),
        name(running)
    )))
}

async fn stop(
    state: web::Data<State>,
    req: actix_web::HttpRequest,
) -> Result<impl Responder, ApiError> {
    named(&state, &req)?;
    state
        .handle
        .ask(|reply| Command::Shutdown { reply })
        .await?;
    Ok(HttpResponse::Accepted().json("Pipeline is stopping"))
}

async fn start_transaction(
    state: web::Data<State>,
    req: actix_web::HttpRequest,
) -> Result<impl Responder, ApiError> {
    named(&state, &req)?;
    match state
        .handle
        .ask(|reply| Command::BeginTransaction { reply })
        .await?
    {
        Ok(id) => Ok(HttpResponse::Ok().json(json!({"transaction_id": id}))),
        // A second `start_transaction` is Feldera's 409, not a 400
        // (`adapterlib/src/errors/controller.rs:873-903`).
        Err(Fault::Refused(m)) => Err(ApiError::Conflict(m)),
        Err(other) => Err(other.into()),
    }
}

async fn commit_transaction(
    state: web::Data<State>,
    req: actix_web::HttpRequest,
) -> Result<impl Responder, ApiError> {
    named(&state, &req)?;
    state
        .handle
        .ask(|reply| Command::CommitTransaction { reply })
        .await??;
    Ok(HttpResponse::Ok().json("Transaction commit initiated"))
}

#[derive(Debug, Deserialize)]
struct TokenArgs {
    token: String,
}

async fn completion_status(
    state: web::Data<State>,
    req: actix_web::HttpRequest,
    args: web::Query<TokenArgs>,
) -> Result<impl Responder, ApiError> {
    named(&state, &req)?;
    let token = Token::decode(&args.token, state.handle.shared.incarnation)
        .map_err(ApiError::InvalidParam)?;

    let step = state.handle.step_for(token.c);
    let complete = step.is_some_and(|s| circuit::is_complete(&state.handle.shared, s));
    Ok(HttpResponse::Ok().json(json!({
        "status": if complete { "complete" } else { "inprogress" },
        "step": step,
    })))
}

/// Only what this runtime actually knows.
///
/// Feldera's field *names*, so a client deserializing into its own
/// `ExternalControllerStatus` succeeds — but nothing it does not know is
/// reported as zero, because a fabricated number is worse than an absent one.
async fn stats(
    state: web::Data<State>,
    req: actix_web::HttpRequest,
) -> Result<impl Responder, ApiError> {
    named(&state, &req)?;
    let s = &state.handle.shared;
    let transaction_status = if s.committing.load(Ordering::Relaxed) {
        "CommitInProgress"
    } else if s.transaction_open.load(Ordering::Relaxed) {
        "TransactionInProgress"
    } else {
        "NoTransaction"
    };
    let completed = s.completed_steps.load(Ordering::Relaxed);
    Ok(HttpResponse::Ok().json(json!({
        "global_metrics": {
            "state": if s.running.load(Ordering::Relaxed) { "Running" } else { "Paused" },
            "incarnation_uuid": s.incarnation,
            "transaction_status": transaction_status,
            "transaction_id": s.transaction_id.load(Ordering::Relaxed),
            "buffered_input_records": s.buffered_input_records.load(Ordering::Relaxed),
            "total_input_records": s.total_input_records.load(Ordering::Relaxed),
            "total_initiated_steps": s.initiated_steps.load(Ordering::Relaxed),
            "total_completed_steps": completed,
            "pipeline_complete": s.buffered_input_records.load(Ordering::Relaxed) == 0,
        }
    })))
}

/// What this pipeline holds. Feldera has a `/metadata` that returns an opaque
/// blob given at startup; this one answers the question a client of *this*
/// server actually has, which is what it may ingest into and watch.
async fn metadata(
    state: web::Data<State>,
    req: actix_web::HttpRequest,
) -> Result<impl Responder, ApiError> {
    named(&state, &req)?;
    Ok(HttpResponse::Ok().json(json!({
        "name": state.pipeline,
        "tables": state.handle.tables,
        "views": state.handle.views,
        "materialized": state.handle.materialized,
    })))
}
