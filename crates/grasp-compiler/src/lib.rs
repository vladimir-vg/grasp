//! The grasp compiler frontend.
//!
//! Nothing here yet. This crate will compile grasp — a Datalog dialect, with no
//! runtime of its own — into grasp-dbsp, the language specified in
//! `docs/grasp-dbsp/language.md`, which `grasp-dbsp-runner` executes. What is
//! settled about grasp is in `docs/grasp/`; the rest is not designed.
//!
//! grasp-dbsp is deliberately a *compilation target*: explicit, uniform, and
//! not required to be convenient. `docs/grasp-dbsp/overview.md` records the
//! properties it is held to, which are worth reading before emitting it.
