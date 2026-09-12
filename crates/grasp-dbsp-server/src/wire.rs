//! The shapes on the wire, and the status codes that go with them.
//!
//! All of it is Feldera's, because a drop-in server that invents its own error
//! envelope is not one: a client's error handling is as much a part of the API
//! as its success path. The struct is
//! `adapters/src/server/error.rs:67-79` and the status mapping is `:343-363`.

use actix_web::http::StatusCode;
use actix_web::{HttpResponse, ResponseError};
use serde::Serialize;
use serde_json::{Value as J, json};

/// Feldera's error body: a sentence, a machine-readable code, and whatever
/// structured detail the code implies.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub message: String,
    pub error_code: String,
    pub details: J,
}

/// Everything this server refuses, and what it refuses with.
#[derive(Debug)]
pub enum ApiError {
    /// A table, view or pipeline this program does not have. 404.
    UnknownName {
        what: &'static str,
        name: String,
    },
    /// A query parameter that is not one this server implements. 400.
    InvalidParam(String),
    /// Rows that did not parse. 400 — and note that the rows which *did* parse
    /// have still been ingested, which is Feldera's behaviour
    /// (`transport/http/input.rs:308-312`) and has to be matched rather than
    /// improved on: a client that retried the whole batch after a 400 would
    /// double-insert.
    ParseErrors {
        total: usize,
        errors: Vec<J>,
    },
    /// The circuit is no longer running. 410, following Feldera's
    /// `Terminating → GONE`.
    Gone(String),
    /// The runner refused: a transaction already open, a commit with nothing
    /// to commit. 409 where Feldera uses it, 400 otherwise.
    Conflict(String),
    Refused(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.body().message)
    }
}

impl ApiError {
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::UnknownName { what: "table", .. } => "UnknownInputTable",
            ApiError::UnknownName { what: "view", .. } => "UnknownOutputTable",
            ApiError::UnknownName {
                what: "pipeline", ..
            } => "UnknownPipelineName",
            // Unreachable: `what` is one of the three above at every
            // construction site. Spelled as a table rather than as a pipeline
            // because a name that reached a handler at all was a relation's.
            ApiError::UnknownName { .. } => "UnknownInputTable",
            ApiError::InvalidParam(_) => "InvalidParam",
            ApiError::ParseErrors { .. } => "ParseErrors",
            ApiError::Gone(_) => "Terminating",
            ApiError::Conflict(_) => "TransactionInProgress",
            ApiError::Refused(_) => "InvalidParam",
        }
    }

    pub fn body(&self) -> ErrorResponse {
        let (message, details) = match self {
            // A process serves exactly one pipeline, so an unknown *pipeline*
            // name is not a thing this pipeline lacks — it is a pipeline that
            // is somewhere else, if it exists at all. Saying "this pipeline
            // has no pipeline named ..." would be both odd and wrong.
            ApiError::UnknownName {
                what: "pipeline",
                name,
            } => (
                format!("this process serves no pipeline named `{name}`"),
                json!({}),
            ),
            ApiError::UnknownName { what, name } => (
                format!("this pipeline has no {what} named `{name}`"),
                json!({}),
            ),
            ApiError::InvalidParam(m) | ApiError::Refused(m) | ApiError::Conflict(m) => {
                (m.clone(), json!({}))
            }
            ApiError::Gone(m) => (m.clone(), json!({})),
            ApiError::ParseErrors { total, errors } => (
                format!(
                    "failed to parse {total} {}",
                    if *total == 1 { "record" } else { "records" }
                ),
                json!({"num_errors": total, "errors": errors}),
            ),
        };
        ErrorResponse {
            message,
            error_code: self.code().to_string(),
            details,
        }
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        match self {
            ApiError::UnknownName { .. } => StatusCode::NOT_FOUND,
            ApiError::InvalidParam(_) | ApiError::ParseErrors { .. } | ApiError::Refused(_) => {
                StatusCode::BAD_REQUEST
            }
            ApiError::Gone(_) => StatusCode::GONE,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
        }
    }

    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status_code()).json(self.body())
    }
}

/// One parse failure, shaped as Feldera shapes it
/// (`adapterlib/src/format.rs:929-966`). The fields this server can fill are
/// filled; the ones that describe a connector are left out rather than faked.
pub fn parse_error(event_number: usize, description: String, invalid_text: &str) -> J {
    json!({
        "description": description,
        "event_number": event_number,
        "invalid_text": invalid_text,
        "suggestion": "Example valid JSON: '{\"insert\": {...}}' or '{\"weight\": 1, \"data\": {...}}'",
    })
}
