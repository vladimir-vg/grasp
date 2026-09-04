# DBSP Runner — Overview

DBSP Runner is a runtime for declarative dataflow programs. It reads a source
program written in a small purpose-built language, instantiates it as a DBSP
circuit using the [`dbsp`](https://github.com/feldera/feldera/tree/main/crates/dbsp)
crate, and executes that circuit incrementally: input changes stream through
the circuit and output changes are emitted as they are produced.

## What it is

- A **single Rust crate** (`dbsp-runner`) linking against `dbsp` and Feldera's
  value libraries (`feldera-sqllib`, `feldera-fxp`, `feldera-types`).
- A **runtime interpreter**: the program is parsed at startup and the circuit
  is assembled at startup, through `dbsp`'s ordinary operator API instantiated
  at a single universal value type. There is **no code generation and no Rust
  toolchain at runtime** — assembling the circuit is still a build step, but it
  needs no compiler.
- A **thin layer over `dbsp`**. DBSP defines the computational model (Z-sets,
  incremental operators, weights, epochs, state); DBSP Runner defines the
  language, the value model, and how the language maps onto `dbsp`. It does
  not reimplement or re-describe DBSP's model.

## Scope (v1)

- The source language described in [`language.md`](language.md).
- The operator set listed there: inputs; the mapping family (`map`, `filter`,
  `flat_map`, `map_index`, `flat_map_index`); the join family (`join`,
  `join_index`, `antijoin`); `distinct`; `aggregate` over
  `min`/`max`/`sum`/`avg`/`count`; `weighted_count`; the algebraic operators
  (`neg`, `plus`, `minus`, `sum`); and `integrate`, `differentiate`, `delay`.
- Expressions in function bodies: field and element access, record and tuple
  construction, arithmetic and comparison, conditionals, and a small builtin
  library.
- The type system described in [`language.md`](language.md) (batch types and
  value types).
- The value model described in [`mapping.md`](mapping.md).
- Feldera-native JSON input/output, emitting deltas — `weighted` by default,
  `insert_delete` for compatibility.

Outputs are **not** part of the source language. A program declares streams; the
set of nodes to observe is supplied when the runner starts, by node name.

## Future work

- **Recursive queries** (`circuit` / `iterate`), mapped onto `dbsp`'s
  `RootCircuit::recursive`. The `Vec<Stream>` form of the underlying
  `dyn_recursive` allows a runtime-determined number of mutually recursive
  streams, which is the shape this runtime needs.
- **Checkpoint and restore.** Every stateful `dbsp` operator takes a
  `persistent_id`; wiring node names through as stable ids is what makes
  checkpoints restorable. See [`mapping.md`](mapping.md).
- **Multi-worker execution.** `Runtime::init_circuit` shards by key hash, so
  this depends on the hashing invariants in [`mapping.md`](mapping.md).
- **`left_join`.** `dbsp` has one, but its right-hand input must be
  `Option`-valued, so exposing it needs either a language-level constraint or a
  wrapping step in the lowering. See [`mapping.md`](mapping.md).
- **`consolidate`.** Applies to trace-carrying streams, which this language does
  not yet produce; it becomes meaningful together with recursion.
- **Windowing and ranking operators** — `window`, `waterline`, `topk`, `rank`,
  `row_number`, `lag`, `asof_join`, `star_join`. All exist in `dbsp`; none are
  in v1.
- **A richer expression library** — user-defined functions, and a fuller
  arithmetic/string/temporal builtin set.
- **Convenience operator macros** — ergonomic forms (for example field-based
  joins) that desugar onto the primitives.
- **The CLI / HTTP surface** — the current `validate` / `run` / `serve`
  commands are placeholders and subject to change.

## Relationship to other projects

- **grasp-dbsp** — an Erlang DBSP runtime with its own language. DBSP Runner
  borrows only the *grammar* (declaration forms, `name := op(...)`,
  `name :: type`, `fun((params) -> ...)`, and `record(...)` where grasp-dbsp
  writes `struct(...)`). Operators, types, and semantics come from `dbsp`, not
  from grasp-dbsp.
- **dbsp** — the computational engine. DBSP Runner relies on `dbsp` for all
  execution and state management. The design documents describe only *how the
  language is mapped onto* `dbsp`.
- **Feldera's SQL compiler** — solves the same lowering problem by generating
  Rust. DBSP Runner does the same lowering at runtime instead, so the two agree
  on operator vocabulary and on the shape of aggregation, but share no code.

## Architecture

A single crate with these logical layers:

| module | responsibility |
|---|---|
| `lang` | lexing and parsing the source language |
| `typecheck` | type inference/checking; produces the schema (`TypeDesc`) |
| `value` | the runtime value model (`DynValue`, `TypeDesc`) |
| `expr` | compiling expressions into closures over `DynValue` |
| `json` | the Feldera JSON codec (`weighted`, `insert_delete`) |
| `lower` | mapping the program onto `dbsp` operators |
| `runtime` | circuit construction, input/output handles, transactions |
| `serve` / CLI | process entry points (`validate` / `run` / `serve`) |

The layered mapping is: source text → AST → typed AST (`TypeDesc`) → `dbsp`
circuit, with values flowing through a single runtime value type.

Two types are reused from `feldera-types` rather than rebuilt:
`SqlSerdeConfig` for the JSON codec's format options, and `program_schema::
{Relation, Field}` for reporting relation schemas on the API surface. `TypeDesc`
itself is our own, because `ColumnType` cannot express the plain-builtin half of
the type vocabulary.
