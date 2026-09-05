//! DBSP Runner: a runtime for declarative dataflow programs.
//!
//! Reads a program written in a small purpose-built language, instantiates it
//! as a DBSP circuit using the `dbsp` crate, and executes that circuit
//! incrementally over Feldera-native JSON input/output.
//!
//! The design is described in `docs/design/`:
//! - `overview.md`  — goals, scope, future work, architecture
//! - `language.md`  — the source language (types, operators, builtins)
//! - `mapping.md`   — how the language maps onto the `dbsp` crate
//!
//! This is a walking skeleton: a narrow but complete path from program text to
//! JSON deltas. See `overview.md` for what is deliberately not implemented yet.

pub mod diag;
pub mod expr;
pub mod json;
pub mod lang;
pub mod lower;
pub mod typecheck;
pub mod value;

use crate::diag::Diagnostic;
use crate::typecheck::Plan;

/// Parse and type-check a program.
///
/// The one door into the compilation pipeline: as passes are added, they go
/// here and every caller picks them up. Returns a vector because a pass will
/// eventually report more than one problem — today it always holds exactly one.
pub fn compile(source: &str) -> Result<Plan, Vec<Diagnostic>> {
    let program = lang::parse(source).map_err(|d| vec![d])?;
    typecheck::check(&program).map_err(|d| vec![d])
}
