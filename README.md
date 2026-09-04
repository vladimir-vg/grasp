# dbsp-runner

A runtime for declarative dataflow programs, executed incrementally as DBSP
circuits via the [Feldera `dbsp` crate](https://github.com/feldera/feldera).

Design documents live in [`docs/design/`](docs/design/).

The `Cargo.toml` path-dependencies point at a local Feldera checkout at
`~/repos/feldera`; adjust them if your layout differs.
