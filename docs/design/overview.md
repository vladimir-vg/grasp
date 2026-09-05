# DBSP Runner — Overview

DBSP Runner is a runtime for declarative dataflow programs. It reads a source
program written in a small purpose-built language, instantiates it as a DBSP
circuit using the [`dbsp`](https://github.com/feldera/feldera/tree/main/crates/dbsp)
crate, and executes that circuit incrementally: input changes stream through
the circuit and output changes are emitted as they are produced.

## What it is

- A **single Rust crate** (`dbsp-runner`) linking against `dbsp`,
  `feldera-sqllib` for the `sql.*` value types, and `feldera-macros` for the
  `IsNone` derive `dbsp`'s `DBData` bound requires.
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
- `circuit` definitions, instantiated either by expansion at the call site —
  including a circuit instantiated inside another — or by `fixpoint`.
- Content-addressed nodes: identical operator, inputs and parameters means one
  node, however many times it is written.
- The operator set listed there: inputs; the mapping family (`map`, `filter`,
  `flat_map`, `map_index`, `flat_map_index`); the join family (`join`,
  `join_index`, `antijoin`); `distinct`; `aggregate` over
  `min`/`max`/`sum`/`avg`/`count`; `weighted_count`; the algebraic operators
  (`neg`, `plus`, `minus`, `sum`); and `integrate`, `differentiate`, `delay`.
- Expressions in function bodies: literals, parameters, record field access,
  `record(...)` construction, arithmetic, comparison and logic, and a small
  builtin library. `(k, v)` pairs and `[…]` lists are syntax rather than
  values; see [`language.md`](language.md).
- The type system described in [`language.md`](language.md) (batch types and
  value types). The value vocabulary implemented so far is `bool`, `i64`,
  `f64`, `String`, `sql.SqlString`, `optional(T)` and `record(...)`.
- The value model described in [`mapping.md`](mapping.md).
- Feldera-native JSON input and output, emitting deltas — `weighted` by default
  here because it represents a Z-set delta exactly, `insert_delete` for
  compatibility with Feldera's own default. Both work in both directions.

One level of `fixpoint` is supported, and that is deliberate rather than
pending: a `fixpoint` inside a `fixpoint` is rejected. `dbsp` allows circuits at
arbitrary depth, but each level is a distinct Rust circuit type needing its own
instantiation of the lowering, and one level covers the recursive queries this
language is for.

Outputs are **not** part of the source language. A program declares streams; the
set of nodes to observe is supplied when the runner starts, by node name.

## Future work

- **Checkpoint and restore.** Every stateful `dbsp` operator takes a
  `persistent_id`; wiring node names through as stable ids is what makes
  checkpoints restorable. See [`mapping.md`](mapping.md).
- **Multi-worker execution.** `Runtime::init_circuit` shards by key hash, so
  this depends on the hashing invariants in [`mapping.md`](mapping.md).
- **`left_join`.** `dbsp` has one, but its right-hand input must be
  `Option`-valued, so exposing it needs either a language-level constraint or a
  wrapping step in the lowering. See [`mapping.md`](mapping.md).
- **`consolidate`.** Applies to trace-carrying streams. Recursion has landed
  and this still does not apply, because no operator in the language produces a
  stream carrying a trace rather than a batch. See [`mapping.md`](mapping.md).
- **Windowing and ranking operators** — `window`, `waterline`, `topk`, `rank`,
  `row_number`, `lag`, `asof_join`, `star_join`. All exist in `dbsp`; none are
  in v1.
- **The rest of the value vocabulary.** The other integer widths and `f32`;
  and every `sql.*` type but `SqlString` — `ByteArray`, `SqlDecimal`, `Date`,
  `Time`, `Timestamp`, `TimestampTz`, `LongInterval`, `ShortInterval`, `Uuid`,
  `Variant`, `Array`, `Map`. Shallow but wide: each needs a `DynValue` variant,
  a `TypeDesc` variant, a parser name, JSON coding and an ordering that upholds
  the invariants in [`mapping.md`](mapping.md), and the temporal and decimal
  types additionally need `SqlSerdeConfig` for their JSON formats.

  Adding a variant shifts `DynValue`'s archived discriminant, which is a
  persisted storage format. Nothing is persisted yet, so the variant order is
  still free to settle — which stops being true after the first stored batch.

- **A runtime list value**, and with it `Vec(T)`, `Tup0..Tup10`, element access
  (`e[0]`), and data-dependent fan-out. Today `[…]` and `(key, value)` are
  syntax rather than values, which is why `flat_map`'s fan-out is fixed by the
  source; see [`language.md`](language.md).

- **The `raw` JSON format** (a bare object meaning insert) and the `update`
  operation for keyed partial updates, which needs primary keys the language
  does not have. Both are Feldera-native; neither is implemented.

- **Conditionals** — `if`/`then`/`else`, which shipped ahead of their design and
  were withdrawn. They are also what a propagating form of arithmetic would need:
  `optional(T)` operands are rejected today, so an expression that should be
  none when its input is cannot yet be written.

- **`cast`**, which needs a type argument in expression position and a
  conversion matrix over the value vocabulary — so it is worth doing once that
  vocabulary has settled.

- **A richer expression library** — user-defined functions, and a fuller
  arithmetic/string/temporal builtin set.
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
| `diag` | one `Diagnostic` type for every pass, with severity, pass and span |
| `expr` | type-checked expressions and their tree-walking evaluator |
| `json` | the Feldera JSON codec (`weighted`, `insert_delete`) |
| `lower` | mapping the program onto `dbsp` operators; also circuit construction, input/output handles and transactions (`Runner`) |
| `serve` / CLI | *not implemented* — `src/main.rs` is a placeholder. YAML fixtures are the surface for now |

The layered mapping is: source text → AST → typed AST (`TypeDesc`) → `dbsp`
circuit, with values flowing through a single runtime value type.

