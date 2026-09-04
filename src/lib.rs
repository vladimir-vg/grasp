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

// Implementation modules will be added here as the project grows:
// lang, typecheck, value, json, lower, runtime, serve.
