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
pub mod core;
pub mod desugar;
pub mod diag;
pub mod infer;
pub mod lex;
pub mod parse;
pub mod ty;

use crate::diag::{Diagnostic, Pass};

/// Check a grasp program, and return what inference concluded about it.
///
/// The front half of [`compile`] — parse, desugar, infer — stopping where the
/// types are still visible. Everything after this point erases them, so a
/// caller that wants to know what a column or a variable was given has to stop
/// here or not ask at all. The `expected_types` fixtures are that caller.
///
/// This is not a second door into the pipeline: [`compile`] is defined as this
/// plus the stages that follow, so a pass inserted here is picked up by both.
pub fn check(source: &str) -> Result<infer::Typed, Vec<Diagnostic>> {
    let program = parse::parse(source).map_err(|d| vec![d])?;
    let core = desugar::desugar(&program)?;
    infer::infer(core)
}

/// Compile a grasp program to grasp-dbsp.
///
/// The one door to grasp-dbsp, as `grasp_dbsp_runner::compile` is for the
/// runner: as passes are added they go here, and every caller picks them up.
/// Returns a vector because a pass will eventually report more than one
/// problem — today it always holds exactly one.
///
/// What this compiler cannot do yet, it says so through
/// [`Diagnostic::unimplemented`] rather than by silence or by a caller
/// consulting a table of how far it has got. That is what the test suite reads
/// to tell a fixture waiting on unwritten code from a fixture the compiler gets
/// wrong.
pub fn compile(source: &str) -> Result<String, Vec<Diagnostic>> {
    let _typed = check(source)?;
    Err(vec![Diagnostic::unimplemented(
        Pass::Plan,
        None,
        "the stages after inference",
    )])
}
