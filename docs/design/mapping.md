# DBSP Runner — Mapping onto `dbsp`

This document describes how the language is mapped onto the `dbsp` crate: the
runtime value model, circuit construction, the operator mapping, the
incrementalization principle, and input/output handling. It assumes the DBSP
computational model (Z-sets, weights, deltas, epochs, stateful operators) and
describes only how we use it.

## Value model

Every stream in the circuit carries the same Rust value type. There is one
universal value type, `DynValue`; the language's type system exists at the level
of `TypeDesc`, which the type checker computes per node and the JSON codec reads.

### `DynValue`

A single recursive enum covering the whole value vocabulary, modeled on
`feldera_sqllib::Variant` (`sqllib/src/variant.rs:29-83`) with a record variant
added:

- **Plain builtins**: `bool`, `i8..i64`, `u8..u64`, `f32`/`f64` (as
  `dbsp::algebra::F32`/`F64`, since `f32`/`f64` are not `Ord`), `String`,
  `Vec<DynValue>`, `Option<Box<DynValue>>`, and `Tuple` (for `Tup0..Tup10`).
- **`sql.*` types**: each wraps the actual `feldera-sqllib` type —
  `SqlString`, `ByteArray`, `Date`, `Time`, `Timestamp`, `TimestampTz`,
  `LongInterval`, `ShortInterval`, `Uuid`, `Variant`.
- **`sql.SqlDecimal(p,s)`**: the runtime value is `feldera_fxp::DynamicDecimal`
  (an integer plus a scale). Precision and scale are not part of the value;
  they are schema metadata (see `TypeDesc`).
- **`sql.Array(T)`** / **`sql.Map(K,V)`**: `Arc<Vec<DynValue>>` /
  `Arc<BTreeMap<DynValue, DynValue>>`, distinct from the plain `Vec` variant.
- **`Record(Vec<DynValue>)`**: the language's `record(...)` type. **Positional.**

### Records are positional

A record value is a bare `Vec<DynValue>`. Field *names* live only in
`TypeDesc::Record`, never in the value. The alternative — storing `(name, value)`
pairs — would put a copy of every column name in every row of every batch, and
carry those copies through serialization and into spilled storage.

Field access is resolved at lowering time: the type checker knows the record's
`TypeDesc` at each expression position, so `row.name` becomes a constant index
and the hot path does no string comparison.

`Record` and `Tuple` have the same runtime representation and are distinguished
only by `TypeDesc`. That distinction is what drives the JSON codec: a record
encodes as a JSON object, a tuple as a JSON array.

### `TypeDesc`

The schema, carried alongside the value: a tree mirroring the value grammar
(`Builtin`, `Sql`, `Vec`, `Option`, `Tuple`, `Record`, `Array`, `Map`), with
extra parameters where the value grammar needs them (decimal precision/scale,
tuple arity, record field names and types). The type checker produces a
`TypeDesc` for every node; the JSON codec uses it to know which `DynValue`
variant is expected at each position. This is what makes I/O schema-driven
without compile-time code generation.

`TypeDesc` is our own type rather than `feldera_types::program_schema::ColumnType`
because `ColumnType` cannot express `Vec`, `Tup*`, or `Option` as a wrapper — it
models nullability as a flag on the type. Two things from `feldera-types` are
reused as-is: `serde_with_context::SqlSerdeConfig`
(`feldera-types/src/serde_with_context/serde_config.rs:123`) for the codec's
date/time/decimal/binary/uuid/variant formats, and `program_schema::{Relation,
Field}` for reporting input and output relation schemas on the API surface.

### What `DynValue` must implement

`DynValue` is used as both the key and the value type of every batch, so it must
be `DBData` (`dbsp/src/trace.rs:98`):

```
Default + Clone + Eq + Ord + Hash + SizeOf + Send + Sync + Debug
  + ArchivedDBData + IsNone<Inner: ArchivedDBData> + SupportsRoaring + 'static
```

`DBData` is blanket-implemented, and `impl<T: DBData> Data for T`
(`dbsp/src/dynamic/data.rs:81`) gives the erased `Data` vtable in turn, so
satisfying the list above is the entire job. `#[derive(feldera_macros::IsNone)]`
supplies both `IsNone` and `SupportsRoaring`; the rest are ordinary derives plus
rkyv's, exactly as every `sqllib` type does it.

One trap worth naming: the `SizeOf` in that list is `feldera-size-of`, a renamed
fork that Feldera depends on as `size-of = { package = "feldera-size-of" }`. The
unrelated `size-of` crate on crates.io defines a different trait of the same
name, and a `DynValue` deriving that one will not satisfy `DBData`. The same
applies to `rkyv`: the derived `Archive` impls only unify with the bounds `dbsp`
imposes if both crates resolve to the same `rkyv` version.

`SupportsRoaring` should report `false` — its default. It only selects between an
exact roaring-bitmap key filter and a Bloom filter for file-backed batches
(`dbsp/src/utils/supports_roaring.rs:17`); reporting `false` costs a slightly
worse false-positive rate and nothing else, while reporting `true` incorrectly
panics.

## Invariants

Four properties of `DynValue` are load-bearing. Each fails silently rather than
loudly, so each is worth a test.

**1. Archived ordering must equal in-memory ordering.** `ArchivedDBData` requires
`Archived: Ord` (`dbsp/src/dynamic/rkyv.rs:14-27`), and nested containers compare
archived-to-archived element-wise while their stored element order was fixed by
in-memory `Ord` at serialize time. If the two disagree, container comparison
becomes non-transitive and rkyv's `ArchivedBTreeMap` binary search breaks.

The robust fix is to make them agree by construction rather than deriving them
independently, as `FlatVariant` does: it routes `Eq`, `Ord` and `Hash` through
one set of functions over its byte encoding, and its archived form *is* that
encoding (`sqllib/src/flat_variant.rs:325-352`, `:1129-1155`). Worth a proptest
asserting `a.cmp(b) == archived(a).cmp(archived(b))` over generated values.

**2. `Eq` and `Hash` must agree, including `-0.0` and `NaN`.** Batches are sharded
across workers by `key.default_hash() % num_workers`
(`dbsp/src/operator/dynamic/communication/shard.rs`). Two values that compare
equal but hash differently land on different workers and never consolidate:
retractions stop cancelling insertions, joins miss, and the Z-set silently
accumulates `+1`/`-1` pairs that never annihilate. Route float payloads through
the `F32`/`F64` wrappers for exactly this reason.

**3. The hash must be stable across builds and processes.** `default_hash` is
xxh3 over `Hash` (`dbsp/src/hash.rs:7-11`), and dbsp pins the expected value in a
test because *"if the hash function changes, then restoring from a checkpoint will
fail"* (`dbsp/src/dynamic/data.rs:99-112`). No `RandomState`, no pointer
addresses, no map iteration order.

**4. Integer variants must be normalized.** `Variant`'s derived `Ord` is
discriminant-first, so `Int(5)` and `BigInt(5)` are different keys and are not
numerically ordered against each other. If the runtime can produce the same
logical number in two variants, normalize on construction or define `Ord`
manually.

One thing the design gets for free: dbsp's `Comparable`/`Clonable` vtables assume
both operands behind a `DynData` are the same concrete Rust type, and check it
only in debug builds (`dbsp/src/dynamic/comparable.rs:56-60`). With exactly one
value type erased everywhere, that holds by construction.

## Circuit construction

Circuits are built with `dbsp`'s **ordinary typed API**, instantiated at
`DynValue`:

```rust
type ZSet    = OrdZSet<DynValue>;                 // typed_batch::OrdZSet
type Indexed = OrdIndexedZSet<DynValue, DynValue>;
```

This is not a separate representation from the type-erased one. `typed_batch::
OrdZSet<K>` is `TypedBatch<K, (), ZWeight, DynOrdZSet<DynData>>`
(`dbsp/src/typed_batch.rs:539`) — its inner batch is exactly
`MonoZSet = OrdZSet<DynData>`, the type the `dyn_*_mono` operators work on
(`dbsp/src/operator/dynamic.rs:44-48`). The typed layer is a safe façade over the
same erased machinery, and `dbsp/src/mono.rs` is that façade's implementation.

Building at the typed layer means the runtime does not construct factories by
hand, does not write out-parameter closures, does not assemble `TraceJoinFuncs`
literals, and contains no `unsafe` downcasts. Operator functions compiled from
the source language are ordinary Rust closures over `&DynValue`:

```rust
let out = input.map(move |row: &DynValue| eval(&expr, row));
let idx = input.map_index(move |row| (eval(&key, row), eval(&val, row)));
let j   = idx.join(&other, move |k, l, r| eval3(&f, k, l, r));
```

It is also the only layer at which the full operator set is available.
`integrate`, `differentiate`, `delay` and `delta0` all require `HasZero`, which
the erased batch types do not implement — constructing an empty dynamic batch
needs factories — but `TypedBatch` does (`dbsp/src/typed_batch.rs:272`). Built
directly on `Stream<RootCircuit, MonoZSet>`, `delay` and `integrate` would not
compile, and recursive circuits would be unreachable.

## Operator mapping

Each language operator lowers to one typed `dbsp` method.

| language | `dbsp` method | note |
|---|---|---|
| `input("t")` | `RootCircuit::add_input_zset` | returns a stream and an input handle |
| `map(s, f)` | `map` | `f` compiled to a `DynValue` closure |
| `filter(s, f)` | `filter` | `f` compiled to a predicate |
| `flat_map(s, f)` | `flat_map` | `f` returns an iterator |
| `map_index(s, f)` | `map_index` | `f` returns a `(K,V)` pair |
| `flat_map_index(s, f)` | `flat_map_index` | |
| `join(l, r, f)` | `join` | `f` maps `(K,V₁,V₂)` to an output row |
| `join_index(l, r, f)` | `join_index` | keeps the result indexed |
| `antijoin(l, r)` | `antijoin` | |
| `distinct(s)` | `distinct` | |
| `aggregate(s, agg, f)` | see below | |
| `weighted_count(s)` | `weighted_count` | sums Z-weights per key |
| `neg(s)` | `neg` | weight negation |
| `plus(a,b)` / `minus(a,b)` / `sum(...)` | `plus` / `minus` / `sum` | |
| `integrate(s)` | `integrate` | running sum |
| `differentiate(s)` | `differentiate` | |
| `delay(s)` | `delay` | `z⁻¹` |

These are the methods `dbsp` exposes under its default `backend-mode` feature,
where they come from `dbsp/src/mono.rs`; with that feature off, the equivalent
polymorphic definitions in `dbsp/src/operator/*.rs` apply instead. Do not build
with `default-features = false`: a few methods, `left_join` among them, exist
only in the `mono` module.

Two operators that look like they belong in this table but do not:

- **`consolidate`.** `Stream::consolidate` (`dbsp/src/operator/consolidate.rs:26`)
  is bounded on `T: Trace<Time = ()>` — it merges a *trace* into a single batch.
  Every stream in this language carries a batch, not a trace, so there is nothing
  for it to consolidate. Traces arise from `integrate_trace` and from nested
  circuits, so this operator becomes meaningful only alongside recursion. The
  identically named `OutputHandle::consolidate` (`operator/output.rs:465`) is a
  different thing: it merges per-worker output batches on the read side, and the
  runner uses it there.
- **`left_join`.** Its right-hand input must be
  `OrdIndexedZSet<K, Option<V2>>` and must not actually contain any `None`
  (`dbsp/src/mono.rs:200-215`) — the `Option` is there to avoid an internal
  transformation, not to express outer-join semantics. Exposing it would mean
  either constraining the right side's value type in the language or having the
  lowering wrap values in `Some`. Deferred rather than shipped on the strength of
  its name.

## Aggregation

`dbsp` has two aggregation paths, and the choice is semantic rather than
cosmetic.

**Linear**, via `aggregate_linear_postprocess(map_fn, post_fn)`. The accumulator
is multiplied by the Z-weight, which is what makes it incremental in one pass.
It is only valid when `f(a+b) = f(a) + f(b)`, and produces wrong answers
otherwise — floating-point sums are excluded for that reason. This is the path
Feldera's SQL compiler uses for every aggregate it can.

`sum`, `avg` and `count` lower here. Two consequences:

- The accumulator must satisfy `DBWeight = DBData + MonoidValue`
  (`dbsp/src/trace.rs:177`) — additive, with a zero. `DynValue` cannot be that,
  so linear aggregation uses a **separate numeric accumulator type**, not
  `DynValue`.
- A linear aggregate cannot tell "the group summed to zero" from "the group is
  empty", because `post_fn` is not invoked for a zero result. The accumulator
  therefore carries an extra row counter, and `post_fn` reports an empty group
  when that counter is zero. This is exactly the shape Feldera uses
  (`sql-to-dbsp-compiler/.../ir/aggregate/LinearAggregate.java:29-45`).

**Non-linear**, via `aggregate(aggregator)`. `min` and `max` lower here, to
`dbsp`'s ready-made `Min` and `Max` (`dbsp/src/operator/dynamic/aggregate/min.rs:31`,
`max.rs:27`). Note that these are the *only* ready-made aggregators: `Fold` is a
builder, not an aggregator, and there is no `Sum`, `Count` or `Avg`.

`Min`/`Max` have `Output = V`, so `aggregate(s, min|max, f)` produces
`OrdIndexedZSet(K, A)` where `A` is the type `f` projects. They compare using
`DynValue`'s `Ord` — see invariant 1 above, which is why that ordering matters
for results and not only for storage layout.

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

**Input.** Each `input` node yields an input handle. Records arrive as JSON and
are decoded (via `TypeDesc`) into `DynValue`s, which are pushed onto the handle
with a weight. Pushes are buffered and applied atomically at the next clock
cycle.

**Output.** Output nodes are not marked in the source; the set of output nodes is
supplied when the runner starts, by node name. Each named node gets a
`Stream::output()` handle, which requires only `T: Debug + Clone + Send`
(`dbsp/src/operator/output.rs:41`) and so applies to any node in the program. A
name that does not match a declared node is a startup error. Nodes that are not
named are still constructed — there is no dead-code elimination in v1.

At each clock cycle the handle exposes the deltas produced by the circuit,
encoded back to JSON. The runner emits these deltas as they are produced; it does
not materialize a final table.

**Encodings.** Two formats, both Feldera-native
(`feldera-types/src/format/json.rs:79-120`):

- `weighted` — `{"weight": 2, "data": {…}}`. This represents a Z-set delta
  exactly, including weights whose magnitude is greater than one, and is the
  **default for output**.
- `insert_delete` — `{"insert": {…}}` / `{"delete": {…}}`. A compatibility
  format. It has no way to express a weight of 3 other than repeating the row
  three times, so the encoder must expand by `|w|`.

## Execution model

A **transaction** is the semantic unit, not a step. `transaction()` starts,
commits and drains one logical clock tick; the clock advances between
transactions, not within them. `step()` is a scheduling knob — it returns
`false` while a transaction is still in progress and `true` once the commit is
complete, so a single transaction may span many steps when inputs are large
(`dbsp/src/circuit/dbsp_handle.rs:1673-1712`). The runner's unit of "feed input,
read output deltas" is the transaction.

## Checkpointing and parallelism

**Persistent ids.** Every stateful operator accepts a
`persistent_id: Option<&str>`, which is what lets its state be checkpointed and
restored. v1 may pass `None`, but the choice should be deliberate: node names are
the natural stable ids, and retrofitting them later changes which checkpoints are
restorable. Feldera's own dataflow IR carries one per node
(`crates/ir/src/mir.rs:21-46`).

**Determinism.** `Runtime::init_circuit` runs the constructor closure **once per
worker thread** and asserts that the resulting circuits have identical
fingerprints (`dbsp/src/circuit/dbsp_handle.rs:695-757`). Lowering must therefore
be deterministic: iterate nodes in a fixed order, and never let `HashMap`
iteration order reach circuit construction.

**Sharding.** Multi-worker execution shards batches by `key.default_hash()`, so
invariants 2 and 3 above are prerequisites for running with more than one worker,
not optional polish.
