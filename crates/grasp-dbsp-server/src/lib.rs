//! A Feldera-compatible HTTP server for one grasp-dbsp program.
//!
//! The crate is separate from `grasp-dbsp-runner` rather than living in its
//! `main.rs`, and the reason is in `grasp-compiler`'s manifest: it dev-depends
//! on the runner, and that dependency "must stay one … a normal dependency
//! would put `dbsp`, `rkyv` and `feldera-sqllib` into the graph of a crate that
//! only manipulates strings". An HTTP stack added to the runner would land in
//! that same graph, and `cargo test -p grasp-compiler` would link actix to run
//! a fixture that compiles a string.

pub mod config;
