//! Completion tokens: how a client learns that the rows it sent have landed.
//!
//! `POST /ingress` answers with one of these, and `GET /completion_status`
//! resolves it. That is the whole contract ingestion offers — the request
//! returns as soon as the rows are queued, so without a token a client has no
//! way to know whether its write has been computed.
//!
//! **The encoding is Feldera's, byte for byte**: base64url, no padding, of
//! `{"u": <uuid>, "e": <endpoint>, "c": <count>}`
//! (`adapters/src/controller/stats.rs:126-167`). Not because a client parses it
//! — it is opaque, and every client round-trips it untouched — but because a
//! token that decodes with Feldera's own `CompletionToken::decode` is evidence
//! of compatibility that a token of our own invention could not be.
//!
//! What the fields mean here:
//!
//! - `u` is the incarnation, generated at startup. A token minted before a
//!   restart decodes fine and is refused, rather than being satisfied by a
//!   step number that has come round again.
//! - `e` is Feldera's endpoint id, which distinguishes its connectors. There is
//!   one ingestion path here, so it is always zero — a field kept for the
//!   shape rather than for what it carries.
//! - `c` is the number of rows this ingestion path has accepted, including
//!   this request's. It is a watermark, not a row count: the answer to "has
//!   everything up to here been processed?".

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Token {
    /// The incarnation this token was minted in.
    pub u: uuid::Uuid,
    /// The ingestion endpoint. Always zero here; see the module comment.
    pub e: u64,
    /// The accepted-row watermark this token stands for.
    pub c: u64,
}

impl Token {
    pub fn new(incarnation: uuid::Uuid, count: u64) -> Token {
        Token {
            u: incarnation,
            e: 0,
            c: count,
        }
    }

    pub fn encode(&self) -> String {
        // Infallible: the struct is three scalars.
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).unwrap_or_default())
    }

    /// Reads a token, or says what is wrong with it.
    ///
    /// The message matters: a token from a previous run is the case an
    /// operator actually hits, after restarting a server and replaying a
    /// script, and "invalid token" would send them looking for a typo.
    pub fn decode(text: &str, incarnation: uuid::Uuid) -> Result<Token, String> {
        let bytes = URL_SAFE_NO_PAD
            .decode(text)
            .map_err(|e| format!("a completion token is base64url: {e}"))?;
        let token: Token = serde_json::from_slice(&bytes)
            .map_err(|e| format!("a completion token is a JSON object: {e}"))?;
        if token.u != incarnation {
            return Err(format!(
                "this token was issued by an earlier run of the pipeline ({}), and this \
                 run ({incarnation}) started from nothing — so what it refers to was \
                 never computed here",
                token.u
            ));
        }
        Ok(token)
    }
}
