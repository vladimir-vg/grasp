//! The grasp compiler frontend.
//!
//! grasp is a Datalog dialect with no runtime of its own. This crate reads it,
//! checks it, and emits [grasp-dbsp](../../../docs/grasp-dbsp/language.md) —
//! text, which `grasp-dbsp-runner` executes. It does not run anything, hold
//! state, or exist at runtime.
//!
//! grasp is specified in `docs/grasp/`: the surface language in `syntax.md`,
//! `types.md` and `semantics.md`, how types are found in `inference.md`, the
//! pipeline this crate implements in `compilation.md`, and what it emits in
//! `mapping.md`. Those documents are the specification; where this crate and
//! they disagree, they are right.
//!
//! grasp-dbsp is deliberately a *compilation target*: explicit, uniform, and
//! not required to be convenient. `docs/grasp-dbsp/overview.md` records the
//! properties it is held to, which are worth reading before emitting it.

pub mod ast;
pub mod diag;
pub mod lex;
pub mod parse;

use crate::diag::{Diagnostic, Pass};

/// The furthest pass the pipeline currently reaches.
///
/// The test suite runs on this. A fixture asserting something only a later pass
/// can produce is expected to *fail*, and is reported as pending rather than as
/// a failure; a fixture that passes anyway is an error, because it means either
/// the stage landed and nobody bumped this constant, or the fixture asserts
/// less than it claims to.
///
/// So: landing a stage means bumping this one variant, and every fixture
/// waiting on that stage goes live at once. See `tests/cases/README.md`.
pub const IMPLEMENTED: Pass = Pass::Parse;

/// Compile a grasp program to grasp-dbsp.
///
/// The one door into the pipeline, as [`grasp_dbsp_runner::compile`] is for the
/// runner: as passes are added they go here, and every caller picks them up.
/// Returns a vector because a pass will eventually report more than one
/// problem — today it always holds exactly one.
pub fn compile(source: &str) -> Result<String, Vec<Diagnostic>> {
    let _program = parse::parse(source).map_err(|d| vec![d])?;
    Err(vec![Diagnostic::error(
        Pass::Emit,
        None,
        format!("not implemented: the pipeline stops at `{IMPLEMENTED}`"),
    )])
}
