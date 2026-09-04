# DBSP Runner — Language

The source language is declarative: a program is a flat list of declarations
that name streams and describe how they are derived from inputs. The grammar is
borrowed from grasp-dbsp; the operators and types it refers to come from
`dbsp`.

## Program structure

A program is a sequence of declarations. Declarations may appear in any order;
names are resolved after the whole program is parsed (forward references are
allowed).

```
program      := declaration*
declaration  := node_def | typespec | comment

node_def     := NAME ":=" op_call
typespec     := NAME "::" batch_type
comment      := "#" [^\n]*

op_call      := "input" "(" STRING ")"
              | NAME "(" args ")"          # an operator applied to args
              | NAME "(" STRING ")"        # no-arg operators are not used
```

Node definitions assign a name to a derived stream. A `typespec` attaches a
type to a name; it is required only for `input` nodes and optional (but checked)
everywhere else.

## Types

Types describe both the *shape of the stream's batches* and the *values inside
them*. There are two layers.

### Batch types

A stream carries one batch per clock cycle, and the batch type is part of the
stream's type.

```
batch_type := "OrdZSet" "(" value_type ")"
            | "OrdIndexedZSet" "(" value_type "," value_type ")"
```

- `OrdZSet(T)` — a flat weighted set of `T`.
- `OrdIndexedZSet(K, V)` — a keyed set: each key `K` maps to a weighted set of
  values `V`.

These names are the `dbsp` type names, so a reader can map them directly to
`dbsp::OrdZSet` / `dbsp::OrdIndexedZSet`.

### Value types

```
value_type := builtin | "sql" "." sql_type | "DynRecord" "(" fields ")"

builtin    := u8 | u16 | u32 | u64 | i8 | i16 | i32 | i64 | f32 | f64 | bool
            | String | Vec "(" value_type ")" | Option "(" value_type ")"
            | Tup0 | Tup1 | ... | Tup10  (each with the matching number of type arguments)

sql_type   := SqlString | ByteArray | SqlDecimal "(" precision "," scale ")"
            | Date | Time | Timestamp | TimestampTz
            | LongInterval | ShortInterval
            | Uuid | Variant
            | Array "(" value_type ")" | Map "(" value_type "," value_type ")"

fields     := STRING ":" value_type ("," STRING ":" value_type)*
```

Two namespaces are deliberately separated:

- **Plain builtins** (`u64`, `i64`, `String`, `Vec`, `Option`, `Tup*`) are Rust
  primitive/std types `dbsp` works with directly.
- **`sql.*` types** mirror the Feldera SQL value types from `feldera-sqllib`
  (`SqlString`, `Date`, `SqlDecimal`, …). The `sql.` prefix says "this is a
  `sqllib` type, not a plain Rust type". `sql.Array(T)` is `Arc<Vec<T>>` (not
  `Vec<T>`); `sql.SqlDecimal(p,s)` is a fixed-point type (not `f64`).

`DynRecord("f": T, …)` is a named-field record, the runtime's dynamic record
type. `Tup*` is the positional alternative.

### Typing rules

- An **`input` node requires a `typespec`**, always `OrdZSet(DynRecord(...))`
  (a table is a set of named-field rows). The record's field order is the
  deterministic column order used by the JSON codec.
- **All other nodes are inferred** from the operators applied to them. An
  explicit `typespec` elsewhere is checked, not used to drive inference.
- **Scalar value types are allowed** anywhere a `value_type` is expected
  (for example `OrdZSet(i64)` for a projected single column); only the
  `input` node is restricted to `DynRecord`.

## Operators

Operators are `dbsp` primitives exposed as-is. The reference below gives each
operator's input batch shape(s) and result batch shape. `X` means the shape is
preserved.

| operator | arguments | signature |
|---|---|---|
| `input("t")` | table name | → `OrdZSet(T)` |
| `map(s, f)` | stream, function | `OrdZSet(T) → OrdZSet(U)`, `f : T → U` |
| `filter(s, f)` | stream, function | `OrdZSet(T) → OrdZSet(T)`, `f : T → bool` |
| `map_index(s, f)` | stream, function | `OrdZSet(T) → OrdIndexedZSet(K,V)`, `f : T → (K,V)` |
| `join(l, r, f)` | two streams, function | `OrdIndexedZSet(K,V₁) × OrdIndexedZSet(K,V₂) → OrdZSet(OV)`, `f : (K,V₁,V₂) → OV` |
| `antijoin(l, r)` | two streams | `OrdIndexedZSet(K,V) × OrdIndexedZSet(K,V₂) → OrdIndexedZSet(K,V)` |
| `distinct(s)` | stream | `X → X` (deduplicated) |
| `aggregate(s, agg)` | indexed stream, aggregator | `OrdIndexedZSet(K,V) → OrdIndexedZSet(K,Out)` |
| `neg(s)` | stream | `X → X` |
| `plus(a, b)` | two streams | `X × X → X` |
| `sum(a, b, …)` | streams | `X⁺ → X` |
| `integrate(s)` | stream | `X → X` (running sum) |
| `differentiate(s)` | stream | `X → X` |
| `delay(s)` | stream | `X → X` |
| `consolidate(s)` | stream | `X → X` |
| `weighted_count(s)` | stream | `OrdZSet(T) → OrdIndexedZSet(T, i64)` |
| `output(s)` | stream | — (exposes `s` as an output) |

Operator arity follows `dbsp`: `plus` is binary, `sum` is n-ary.

`join` requires equal key type `K` on both sides; `plus`/`sum` require
identical batch types; `aggregate`'s `Out` is determined by the aggregator.

## Functions and builtins

Operators that transform rows (`map`, `filter`, `map_index`, `join`) take a
function argument. For now a function is **exactly one call to a builtin**, with
all inputs explicit:

```
function_arg := "function" "(" "(" fn_params ")" "->" builtin_call ")"
builtin_call := BUILTIN "(" args ")"
```

The parameter list binds the row(s) the operator feeds the function. `map`,
`filter`, and `map_index` take one row; `join` takes three (key, left value,
right value). Parameter names are arbitrary identifiers:

```
function((row) -> identity(row))
function((row) -> field(row, name: "id"))
function((row) -> project(row, fields: ["id", "name"]))
function((row) -> index(row, key: ["id"], value: ["dname"]))
function((k, v1, v2) -> merge(k, v1, v2))
function((row) -> is_null(row))
```

### Builtins

Argument order mirrors the underlying runtime function (positional inputs
first, then keyword parameters).

| builtin | signature | result |
|---|---|---|
| `identity` | `(row)` | the row unchanged |
| `field` | `(row, name: "f")` | the value of field `f` |
| `project` | `(row, fields: ["a","b"])` | a record with the listed fields |
| `index` | `(row, key: [...], value: [...])` | a `(K,V)` pair extracted from the row |
| `merge` | `(k, v1, v2)` | one record combining `k`, `v1`, `v2` |
| `left` | `(k, v1, v2)` | the left value (and key) |
| `right` | `(k, v1, v2)` | the right value (and key) |
| `is_null` / `is_not_null` / `is_positive` / `is_zero` | `(row)` | a boolean |

### Aggregators

`aggregate` takes an aggregator, not a function. Aggregators are bare names:

```
aggregate(idx, sum)      # also: min, max, count, avg
```

- `min`, `max` — the minimum/maximum value in each group.
- `sum` — the sum of values in each group.
- `count` — the number of rows in each group.
- `avg` — the mean of values in each group.

## Example

```
emp := input("emp")
emp :: OrdZSet(DynRecord("id": i64, "name": sql.SqlString, "dept_id": i64))

dept := input("dept")
dept :: OrdZSet(DynRecord("id": i64, "dname": sql.SqlString))

emp_idx  := map_index(emp,  function((row) -> index(row, key: ["dept_id"], value: ["id", "name"])))
dept_idx := map_index(dept, function((row) -> index(row, key: ["id"], value: ["dname"])))

joined := join(emp_idx, dept_idx, function((k, v1, v2) -> merge(k, v1, v2)))
```
