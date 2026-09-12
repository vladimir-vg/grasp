//! A Feldera-compatible HTTP server for one grasp-dbsp program.
//!
//! The crate is separate from `grasp-dbsp` rather than living in that crate's
//! own binary, and the reason is in `grasp-compiler`'s manifest: it dev-depends
//! on `grasp-dbsp`, and that dependency "must stay one … a normal dependency
//! would put `dbsp`, `rkyv` and `feldera-sqllib` into the graph of a crate that
//! only manipulates strings". An HTTP stack added to `grasp-dbsp` would land in
//! that same graph, and `cargo test -p grasp-compiler` would link actix to run
//! a fixture that compiles a string.

pub mod circuit;
pub mod config;
pub mod http;
pub mod token;
pub mod wire;
