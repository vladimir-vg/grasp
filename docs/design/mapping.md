# DBSP Runner — Mapping onto `dbsp`

This document describes how the language is mapped onto the `dbsp` crate: the
runtime value model, the operator mapping, the incrementalization principle,
and input/output handling. It assumes the DBSP computational model (Z-sets,
weights, deltas, epochs, stateful operators) and describes only how we use it.

## Value model

The dynamically-typed API represents every value as `DynData` (a trait object
over `dbsp`'s `Data` trait). DBSP Runner therefore uses **one concrete value
type** that implements `Data`, and boxes instances into `DynData`.

### `DynValue`

A single recursive enum covering the whole value vocabulary:

- **Plain builtins**: `bool`, `i8..i64`, `u8..u64`, `f32`/`f64` (as
  `dbsp::algebra::F32`/`F64`, since `f32`/`f64` are not `Ord`), `String`,
  `Vec<DynValue>`, `Option<Box<DynValue>>`, `Tuple` (for `Tup0..Tup10`), and
  `Record`.
- **`sql.*` types**: each wraps the actual `feldera-sqllib` type —
  `SqlString`, `ByteArray`, `Date`, `Time`, `Timestamp`, `TimestampTz`,
  `LongInterval`, `ShortInterval`, `Uuid`, `Variant`.
- **`sql.SqlDecimal(p,s)`**: the runtime value is `feldera_fxp::DynamicDecimal`
  (an integer plus a scale). Precision and scale are not part of the value;
  they are schema metadata (see `TypeDesc`).
- **`sql.Array(T)`** / **`sql.Map(K,V)`**: `Arc<Vec<DynValue>>` /
  `Arc<BTreeMap<DynValue, DynValue>>`, distinct from the plain `Vec` variant.

### `DynRecord`

A named-field record: an ordered list of `(name, DynValue)` pairs. Field order
is the deterministic column order shared with the JSON codec.

### `TypeDesc`

The schema, carried alongside the value: a tree mirroring the value grammar
(`Builtin`, `Sql`, `Vec`, `Option`, `Tuple`, `Record`, `Array`, `Map`), with
extra parameters where the value grammar needs them (decimal precision/scale,
tuple arity, record field names and types). The type checker produces a
`TypeDesc` for every node; the JSON codec uses it to know which `DynValue`
variant is expected at each position. This is what makes I/O schema-driven
without compile-time code generation.

## Operator mapping

Each language operator is lowered to one `dbsp` primitive. Operators that
reshape data use `dbsp`'s dynamically-typed (`dyn_*`) forms; operators that
only manipulate weights use the ordinary typed forms, which already work over
`DynData` batches.

| language | `dbsp` primitive | note |
|---|---|---|
| `input("t")` | `dyn_add_input_zset_mono` | an `OrdZSet<DynData>` input with a handle |
| `map(s, f)` | `dyn_map_mono` | `f` compiled to a `DynData` closure |
| `filter(s, f)` | `dyn_filter_mono` | `f` compiled to a predicate |
| `map_index(s, f)` | `dyn_map_index_mono` | `f` writes a `(K,V)` pair |
| `join(l, r, f)` | `dyn_join_mono` | `f` maps `(K,V₁,V₂)` to an output row |
| `antijoin(l, r)` | `dyn_antijoin_mono` | |
| `distinct(s)` | `dyn_distinct_mono` | |
| `aggregate(s, agg)` | `dyn_aggregate_mono` | `agg` becomes an `Aggregator` |
| `neg(s)` | `neg` | weight negation |
| `plus(a, b)` / `sum(...)` | `plus` / `sum` | |
| `integrate(s)` | `integrate` | running sum |
| `differentiate(s)` | `differentiate` | |
| `delay(s)` | `delay` | `z⁻¹` feedback |
| `consolidate(s)` | `consolidate` | |
| `weighted_count(s)` | `weighted_count` | |
| `output(s)` | `.output()` | an output handle |

Builtin functions and aggregators are fixed: `map`/`filter`/`map_index`/`join`
functions are pre-built closures over `DynValue`; `min`/`max`/`sum`/`count`/
`avg` map to `dbsp`'s ready-made `Aggregator`s.

## Incrementalization principle

The language does **not** insert `integrate`/`differentiate` around non-linear
operators. `dbsp`'s `join`, `aggregate`, `distinct`, and `antijoin` are
already incremental: they take a stream of *changes* and internally maintain
the state needed to emit a stream of *changes*. `integrate` and `differentiate`
remain available as explicit primitives for two uses only:

1. materializing an accumulated relation (running sum of deltas), and
2. constructing feedback/recursive structures (together with `delay`).

This is the key difference from grasp-dbsp, which inserts I/D pairs as a
compilation step. Here that step does not exist because it is unnecessary.

## Input / output

- **Input**: each `input` node yields an input handle. Records arrive as
  `insert_delete` JSON and are decoded (via `TypeDesc`) into `DynValue`s,
  which are pushed onto the handle with weight `+1` (insert) or `-1` (delete).
  Pushes are buffered and applied atomically at the next clock cycle.
- **Output**: `output(s)` yields an output handle. At each clock cycle the
  handle exposes the deltas produced by the circuit, encoded back to
  `insert_delete` JSON (a positive weight becomes `{"insert": …}`, a negative
  weight becomes `{"delete": …}`). The runner emits these deltas as they are
  produced; it does not materialize a final table.

Output nodes are not marked in the source; the set of output nodes is supplied
when the runner starts.
