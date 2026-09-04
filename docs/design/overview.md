# DBSP Runner — Overview

DBSP Runner is a runtime for declarative dataflow programs. It reads a source
program written in a small purpose-built language, instantiates it as a DBSP
circuit using the [`dbsp`](https://github.com/feldera/feldera/tree/main/crates/dbsp)
crate, and executes that circuit incrementally: input changes stream through
the circuit and output changes are emitted as they are produced.

## What it is

- A **single Rust crate** (`dbsp-runner`) linking against `dbsp` and Feldera's
  value libraries (`feldera-sqllib`, `feldera-fxp`, `feldera-types`).
- A **runtime interpreter**: the program is parsed at runtime and the circuit
  is assembled at runtime through `dbsp`'s dynamically-typed operator API.
  There is no code generation and no compile step.
- A **thin layer over `dbsp`**. DBSP defines the computational model (Z-sets,
  incremental operators, weights, epochs, state); DBSP Runner defines the
  language, the value model, and how the language maps onto `dbsp`. It does
  not reimplement or re-describe DBSP's model.

## Scope (v1)

- The source language described in [`language.md`](language.md).
- The operator set listed there: inputs, linear transforms, join/antijoin,
  distinct, aggregate, integrate/differentiate, delay, consolidate, output.
- The type system described in [`language.md`](language.md) (batch types and
  value types).
- The value model described in [`mapping.md`](mapping.md).
- Feldera-native JSON input/output (`insert_delete` update format), emitting
  deltas.

## Future work

- **Recursive queries** (`circuit` / `iterate`), mapped onto `dbsp`'s
  `Circuit::iterate`.
- **User-defined function bodies** (arbitrary expressions) and the arithmetic /
  comparison operator library. For now the only callable functions are the
  fixed builtins listed in [`language.md`](language.md).
- **Convenience operator macros** — ergonomic forms (for example field-based
  joins) that desugar onto the primitives.
- **The CLI / HTTP surface** — the current `validate` / `run` / `serve`
  commands are placeholders and subject to change.

## Relationship to other projects

- **grasp-dbsp** — an Erlang DBSP runtime with its own language. DBSP Runner
  borrows only the *grammar* (declaration forms, `name := op(...)`,
  `name :: type`). Operators, types, and semantics come from `dbsp`, not from
  grasp-dbsp.
- **dbsp** — the computational engine. DBSP Runner relies on `dbsp` for all
  execution and state management. The design documents describe only *how the
  language is mapped onto* `dbsp`.

## Architecture

A single crate with these logical layers:

| module | responsibility |
|---|---|
| `lang` | lexing and parsing the source language |
| `typecheck` | type inference/checking; produces the schema (`TypeDesc`) |
| `value` | the runtime value model (`DynValue`, `DynRecord`, `TypeDesc`) |
| `json` | the Feldera JSON codec (`insert_delete`) |
| `lower` | mapping the program onto `dbsp` operators and builtin closures |
| `runtime` | circuit construction, input/output handles, stepping |
| `serve` / CLI | process entry points (`validate` / `run` / `serve`) |

The layered mapping is: source text → AST → typed AST (`TypeDesc`) → `dbsp`
circuit, with values flowing through a single dynamic value type.
