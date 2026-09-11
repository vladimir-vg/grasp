# grasp-dbsp — Mapping onto `dbsp`

This document describes how the language is mapped onto the `dbsp` crate: the
runtime value model, circuit construction, the operator mapping, the
incrementalization principle, and input/output handling. It assumes the DBSP
computational model (Z-sets, weights, deltas, epochs, stateful operators) and
describes only how we use it.

It describes the mapping as implemented.

## Value model

Every stream in the circuit carries the same Rust value type. There is one
universal value type, `DynValue`; the language's type system exists at the level
of `TypeDesc`, which the type checker computes per node and the JSON codec reads.

### `DynValue`

A single recursive enum, modeled on `feldera_sqllib::Variant`
(`sqllib/src/variant.rs:29-83`) with a record variant added. What is
implemented:

- `None` — no value. Variant 0, so it sorts before everything; see
  [`language.md`](language.md) for why it is a value rather than SQL's
  propagating `NULL`.
- `Bool(bool)`, `I64(i64)`, `F64(F64)` — `dbsp::algebra::F64`, since bare
  `f64` is not `Ord`.
- `String(String)` — one string type. An earlier cut also had `SqlString`, a
  cheaply-cloned `ArcStr`; it was withdrawn along with the `sql.*` namespace,
  because two spellings of one thing is a decision an emitter has to make with
  no information.
- `Record(Vec<DynValue>)` — the language's `record(...)`. **Positional.**
- `Array(Vec<DynValue>)` — the language's `array(T)`.
- `Json(FlatVariant)` — the language's `json`, a byte-encoded document.
- `Dict(BTreeMap<DynValue, DynValue>)` — the language's `dict(K,V)`.
- `Date`, `Time`, `Timestamp`, `Interval` — `sqllib`'s `Date`, `Time`,
  `Timestamp` and `ShortInterval`, each one integer behind a newtype.
- `Bytes(ByteArray)` — the language's `bytes`, ordered lexicographically.
- `Dynamic(FlatVariant)` — the language's `dynamic`: `Json`'s payload with the
  type tags kept.

The rest of the vocabulary — the other integer widths, `f32`, and the `sql.*`
types — is future work, listed in [`overview.md`](overview.md). **Append new
variants at the end**: the variant order is the archived discriminant, which is
a storage format. Removing one shifts it too, which is why `SqlString` could go
now and could not once anything is stored.

`Record`, `Array` and `Dict` are the containers whose `Ord`, `Hash` and archived
ordering are ours rather than borrowed. The appended six borrow theirs from a
foreign payload instead, which makes the agreement below something they are
*given* rather than something they derive — a different way to be right, and no
less worth checking.

So the proptests in `tests/invariants.rs` draw **every** variant, and a test
beside them holds the generator to that: its match over `DynValue` stops
compiling when a variant is appended, and its comparison fails until the
generator draws the new one. The discipline in this section is only as real as
the values the properties are fed.

One consequence of a borrowed payload, found by widening them: **an `Interval`
has exactly one way back out of the archived form.** `ShortInterval`'s
hand-written `Deserialize` downcasts to `dbsp::storage::file::Deserializer` to
read the storage format version — its representation changed from milliseconds
to microseconds at version 4 — and panics given any other deserializer. That is
the path `dbsp` takes for a spilled batch, so nothing in the runner is affected;
but code that reads a `DynValue` back with a deserializer of its own choosing
would find out at runtime.

### Why `json` is a `FlatVariant`

`feldera_sqllib::FlatVariant` is `{ buf: Arc<[u8]>, start: u32, len: u32 }` — a
byte-encoded document. It is chosen for its invariants rather than its
convenience, because it earns three of the four **by construction**:

- **Archived ordering equals in-memory ordering** — the archived form *is* the
  byte encoding, so there are not two orderings that could disagree.
- **`Eq` and `Hash` agree** — both route through functions over the same bytes
  (`eq_values`, `cmp_values`, `hash_value`).
- **The hash is stable**, being a walk over those bytes.

And a fourth property not on that list but worth as much: **map entries are
stored sorted and deduplicated**, so `{"a":1,"b":2}` and `{"b":2,"a":1}` produce
identical bytes — one value, one hash, one Z-set key. Key order in the source
JSON is canonicalised away rather than becoming a silent non-annihilation bug.

`json` is still in the proptests, for the opposite reason to `Array`: it is
supposed to satisfy them without our help, so a failure would mean the
representation is not doing what it claims.

Two consequences for the code that reads one. `From<&FlatVariant> for Variant`
decodes the **whole** document recursively, so extraction navigates with
`FlatVariant::index_string` — which shares the buffer rather than cloning, and
yields the absent sentinel for a missing key or a non-object — and decodes only
the leaf it lands on. Decoding at the root would be O(document) per row. And the
derived `IsNone` answers "never", since the struct is not an `Option`: absence
lives in the encoding's tag, so it is tested by comparing against the one-byte
`sql_null` sentinel.

**A `DynValue::Json` never holds that sentinel.** `get` converts it to
`DynValue::None` at the boundary, and nothing else produces one — the
deserializer maps a bare `null` to `TAG_VARIANT_NULL`, and so does building a
document from an absent value. The encoding distinguishes three states; the
language shows two, because the third would be a value that serializes as `null`
without being the null document, which nothing could observe.

There is no tuple variant. `map_index`, `join_index` and `flat_map_index` take
an ordinary `record(key: …, value: …)` and the lowering splits it, so nothing
pair-shaped is ever streamed.

### Why `dict` is a `BTreeMap`

A dict's canonical form is **structural**, not a rule. A `BTreeMap` cannot hold
its entries unsorted or hold a key twice, so two dicts built from the same
entries in different orders are the same value — one `Ord`, one `Hash`, one
Z-set key — without any construction site having to remember to sort. Entry
order is not part of a dict's identity, and there is no way to write one for
which it is.

`Record` reaches the same place from the other side: its *type* sorts its fields
by name, so the positional value follows. A dict cannot do that, because its keys
are values rather than a fixed part of the type — so the container has to carry
the invariant instead.

This upholds invariant 1 because `ArchivedBTreeMap::cmp` is
`self.iter().cmp(other.iter())` — the same lexicographic comparison over the same
order that `BTreeMap` itself uses. `sqllib::Variant` stores its `Map` the same
way (`sqllib/src/map.rs:9`), behind an `Arc` it needs for sharing and this does
not.

**Keys are scalars** — `bool`, `i64`, `f64`, `string` — which is a JSON
constraint rather than a representation one: `DynValue::Dict` can structurally
hold any key, and the proptests exercise that, but a dict encodes as an object
and an object's keys are strings. See [Input / output](#input--output).

### Records are positional

A record value is a bare `Vec<DynValue>`. Field *names* live only in
`TypeDesc::Record`, never in the value — and that vector is in **canonical
order**, sorted by field name, because field order is not part of a record's
identity. `TypeDesc::record` sorts the type and the checker sorts the literal's
expressions with it, so the two stay aligned by position. The alternative — storing `(name, value)`
pairs — would put a copy of every column name in every row of every batch, and
carry those copies through serialization and into spilled storage.

Field access is resolved at lowering time: the type checker knows the record's
`TypeDesc` at each expression position, so `row.name` becomes a constant index
and the hot path does no string comparison.

`TypeDesc` is what drives the JSON codec: a record encodes as a JSON object,
with the field names coming from the schema rather than the value.

### `TypeDesc`

The schema, carried alongside the value: a tree mirroring the value grammar —
`Bool | I64 | F64 | String | Optional(T) | Record(fields) | Array(T)` — and
growing alongside `DynValue`. The type checker produces a
`TypeDesc` for every node; the JSON codec uses it to know which `DynValue`
variant is expected at each position. This is what makes I/O schema-driven
without compile-time code generation.

`TypeDesc` is our own type rather than `feldera_types::program_schema::ColumnType`
because `ColumnType` cannot express a list, a tuple, or `optional` as a wrapper —
it models nullability as a flag on the type.

Two things from `feldera-types` are *candidates* for reuse, not currently used:
`serde_with_context::SqlSerdeConfig`
(`feldera-types/src/serde_with_context/serde_config.rs:123`) for the
date/time/decimal/binary/uuid formats, once those types exist; and
`program_schema::{Relation, Field}` for reporting relation schemas, once there is
an API surface. The crate is not a dependency today.

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

**`SizeOf` is not bookkeeping, it is the spill decision.** `dbsp` chooses
between keeping a batch in memory and writing it to a file entirely from this
number: `BatchReader::approximate_byte_size` samples a hundred keys and
multiplies (`dbsp/src/trace.rs:568-579`, `dbsp/src/dynamic/vec.rs:280-300`), and
a builder spills part-way through once the keys it has accumulated cross the
threshold (`dbsp/src/trace/ord/fallback/wset.rs:562-578`). A value that reports
less than it holds is a value that never spills, however large the relation
grows — a failure that ends in the process being killed rather than in a wrong
answer, which is the family the invariants below belong to.

So `DynValue`'s impl is **hand-written**. The derive cannot do it: it emits a
`where Vec<DynValue>: SizeOf` bound while proving `DynValue: SizeOf`, which the
solver reports as an overflow, and its escape hatch — `#[size_of(skip)]` on the
recursive field — does not make a payload weightless, only invisible. A written
impl carries no such bound, so `Vec`, `BTreeMap` and `String` resolve through
their own impls and walk the tree. `sqllib::ByteArray` hand-writes its own for
the same reason (`sqllib/src/binary.rs:73-88`).

Two things it does *not* claim. It reports what a value **owns**, not what the
process resident set grows by. And a `json` or `dynamic` document's buffer is an
`Arc`, counted once per `Context` but once *per row* across rows that share it,
so a document held by a thousand rows counts a thousand times — an over-count,
which errs toward spilling early rather than late.

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
encoding (`sqllib/src/flat_variant.rs:325-352`, `:1129-1155`). `DynValue` derives
the two independently, so the agreement is asserted rather than constructed: a
proptest in `tests/invariants.rs` checks
`a.cmp(b) == archived(a).cmp(archived(b))` over generated values.

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

**Why this does not bite today, which is the part worth writing down.** Every
column is statically typed and the language has no implicit conversion, so two
numeric variants never meet inside one batch: a join requires equal key types,
`plus` requires identical batch types, and arithmetic requires identical operand
types. The invariant is therefore about values whose type is *dynamic*.

That has two consequences. Adding `i32` and `f32` is safe — they are separate
language types that never compare against `i64` — even though appending them
puts `I32(5)` after `String("a")` in the derived order, which looks alarming and
is unobservable.

And `json` is the dynamic case, which is why it is restricted. `FlatVariant`
compares tag-first, so a document holding `5` and one holding `5.0` are
**different Z-set keys**. That is sound — they are genuinely different documents,
and the language says so rather than pretending otherwise — but an *ordering*
over documents would be well defined and meaningless, putting every number before
every string. So `==` and `!=` are allowed, while `<`, `<=`, `>`, `>=` and a
`min`/`max` projection are rejected: this ordering is load-bearing for query
results, not merely for batch layout, and it must not be allowed to decide one.

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

Circuits must be built through `Runtime::init_circuit`, not `RootCircuit::build`.
The bare builder produces a circuit that panics at the first transaction with
*"Attempting to create a spine merger outside of a DBSP runtime"* as soon as any
stateful operator is present — `join`, `aggregate` and `distinct` all maintain
traces, so in practice that is every non-trivial program. The runtime's
constructor closure must therefore be `Clone + Send + 'static` and its return
value `Send`.

The typed layer is also the only one at which the full operator set is
available. `integrate`, `differentiate`, `delay` and `delta0` all require
`HasZero`, which the erased batch types do not implement — constructing an empty
dynamic batch needs factories — but `TypedBatch` does
(`dbsp/src/typed_batch.rs:272`). Built directly on
`Stream<RootCircuit, MonoZSet>`, `delay` and `integrate` would not compile, and
recursive circuits would be unreachable.

### Nodes are deduplicated before the circuit is built

`push_node` (`typecheck/mod.rs`) returns an existing node whenever one already
has the same batch type and the same `PlanOp`. `PlanOp` holds the operator's
input *indices* and its parameters — including the compiled `Arc<TypedExpr>`
function bodies, compared structurally — so equal `PlanOp`s mean genuinely the
same computation over genuinely the same operands.

Because it runs at plan time, `dbsp` never sees the duplicates: the operator
graph handed to `Runtime::init_circuit` is already deduplicated, and nothing in
the lowering has to know the rule exists. It is also why deduplication cannot
change results — it changes which nodes are built, not what any of them
compute.

`Fixpoint` is the one exclusion. Its `PlanOp` carries a whole sub-plan, whose
body nodes hold slot indices meaningful only within their own fixpoint, so
structural equality between two of them would not mean what it means everywhere
else.

### A node's identity is its content

That same identity — `(batch type, PlanOp)`, with the name and span deliberately
excluded — becomes a stable string in `typecheck/content.rs`. It is a **Merkle
hash**: an operand contributes its own id rather than its index, so the result
depends on the shape of the computation and not on where a node landed in the
list, what it was called, or how the source was laid out.

Two things use it.

**It is the `persistent_id` of every operator.** `dbsp` requires ids that
"identify the same computation across restarts", derived "from the program (a
view name, a hash of the subgraph) rather than from anything positional"
(`dbsp/src/operator/recursive.rs`). Node *names* do not qualify: a nested node's
name is `filter@3:12`, which is a source position, and a deduplicated node keeps
whichever name happened to be written first. This matters more than it would for
a hand-written language, because an emitting agent **regenerates** whole programs
rather than editing them — whitespace, declaration order and the names of
intermediates are all unstable across emissions while the computation is not.
`tests/plan.rs` pins that: the same dataflow written two ways produces the same
ids.

**It is the name any node can be observed by.** Outputs are chosen when the
runner starts, and a nested node has no declared name — so without this, "any
node" would quietly exclude every anonymous one.

The hash is xxh3, the same algorithm `dbsp` uses for its own stable hashing and
for the same reason. Rust's `DefaultHasher` is explicitly not guaranteed stable
across releases, which a persistent id cannot tolerate.

### An ordinary `circuit` has no runtime form

"Nested circuit" means different things in the language and in `dbsp`, and only
one of them reaches `dbsp`. Instantiating a `circuit` is inlining at plan time:
the body becomes ordinary nodes in the enclosing node list, and by the time
lowering runs there is nothing left to say it was ever a circuit. That holds for
a circuit instantiated inside another.

`fixpoint` is the sole construct that builds a real nested `dbsp` circuit; see
[Fixpoint](#fixpoint) below.

## Operator mapping

Each language operator lowers to one typed `dbsp` method.

| language | `dbsp` method | note |
|---|---|---|
| `input("t")` | `RootCircuit::add_input_zset` | returns a stream and an input handle |
| `map(s, f)` | `map` | `f` compiled to a `DynValue` closure |
| `filter(s, f)` | `filter` | `f` compiled to a predicate |
| `flat_map(s, f)` | `flat_map` | `f` returns an `array`, so fan-out follows the data |
| `map_index(s, f)` | `map_index` | `f` returns `record(key:, value:)`, split by the lowering |
| `flat_map_index(s, f)` | `flat_map_index` | as both of the above |

Every method in the mapping family exists on an indexed stream too, taking the
element as one `(&K, &V)` tuple — the shape `filter` already used. So each of
those five arms matches on the operand's shape rather than requiring a flat one,
and `map` on an indexed stream returns an `OrdZSet`: the only route out of an
indexed shape that is not a join.
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
| `empty()` | `add_source(Generator::new(HasZero::zero))` | a source yielding the zero batch every cycle |
| `constant([…])` | `add_source(ConstantGenerator::new(batch)).differentiate()` | the rows in the first transaction, the zero batch after |

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
  different thing: it merges per-worker output batches on the read side. The
  runner reads there through `concat().consolidate()` on an
  [accumulating sink](#execution-model), which is the same merge over a whole
  transaction rather than over one step.
- **`left_join`.** Its right-hand input must be
  `OrdIndexedZSet<K, Option<V2>>` (`dbsp/src/mono.rs:200-215`) — the `Option` is
  there to avoid an internal transformation, not to express outer-join
  semantics. That is a *second Rust batch type*, in a design whose leverage is
  that there is exactly one, so exposing it would cost a fourth `Node` variant
  and a second instantiation of everything downstream.

  It is not needed. A left join is `join ∪ (antijoin × null)`, three operators
  that already exist, now that the mapping family accepts an indexed stream and
  `cast` can widen the matched side to `optional`. See
  [`language.md`](language.md); `crates/grasp-dbsp-runner/tests/cases/joins.yaml`
  has it end to end, including the retraction when a missing match later
  arrives.

## Aggregation

`dbsp` has two aggregation paths, and the choice is semantic rather than
cosmetic.

**Linear**, via `aggregate_linear_postprocess(map_fn, post_fn)`. The accumulator
is multiplied by the Z-weight, which is what makes it incremental in one pass.
It is only valid when `f(a+b) = f(a) + f(b)`, and produces wrong answers
otherwise — floating-point sums are excluded for that reason. This is the path
Feldera's SQL compiler uses for every aggregate it can.

`sum` and `avg` over an *integer* projection lower here, and `count` always
does. **Floating-point `sum` and `avg` do not** — see below. Two consequences:

- The accumulator must satisfy `DBWeight = DBData + MonoidValue`
  (`dbsp/src/trace.rs:177`) — additive, with a zero. `DynValue` cannot be that,
  so linear aggregation uses a **separate numeric accumulator type**, not
  `DynValue`.
- A linear aggregate cannot tell "the group summed to zero" from "the group is
  empty", because `post_fn` is not invoked for a zero accumulator. This needs
  **two** counters, which is easy to get wrong:
  - `rows` counts projections that are not `NONE`. It is `count` itself.
  - `present` counts *every* row. Without it, a group whose projections are all
    `NONE` zeroes every field, and `dbsp` drops the group entirely rather than
    reporting `NONE`. Feldera carries the same extra counter for the same reason
    (`sql-to-dbsp-compiler/.../ir/aggregate/LinearAggregate.java:29-45`).

  This was found by a fixture, not by reasoning — the first version had only
  `rows`, and an all-`NONE` group silently vanished.

  **What the second counter does not fix.** All three fields are weight-scaled,
  so a retraction of a *different* row can cancel them: a group holding one
  all-`NONE` row at `+1` and another at `−1` has every field zero and disappears
  even though the Z-set is not empty. That is inherent to linear aggregation
  rather than a gap in the counters, and it is worth stating because the fixture
  style that caught the first bug does not reach this one.

- **What each aggregator reports, and its type.** These follow one rule — a
  result that may be absent says so in its type, and one that cannot be absent
  never returns `NONE`:
  - `count` is `i64`, never optional. An all-`NONE` group counts `0`.
  - `sum` is optional exactly when its projection is. With a definite
    projection the weighted sum is the answer even for a group whose weights
    cancel, so it never returns absence in a column typed `i64`.
  - `avg` is *always* optional, whatever the projection: a mean of no
    contributing rows is undefined, and that can happen at any projection type.

**Non-linear**, via `aggregate(aggregator)`. `min`, `max`, and a
floating-point `sum` or `avg` lower here. It replays the group rather than
maintaining an accumulator, which costs more per change and buys two things the
linear path cannot give.

**Floating point.** fp addition is not associative, so a linear sum would depend
on the order additions and retractions arrived in. Replaying the group in cursor
order does not. That rules out the linear path, not the operation — which is the
conclusion Feldera draws too: `AggregateCompiler.java` takes the linear path only
`if (this.linearAllowed && !this.fp())`. `dbsp`'s `Fold` is itself an
`Aggregator` (`aggregate/fold.rs`), and returns `None` for a group whose weights
all cancel, so the fold needs no third counter to keep an empty group from being
confused with an absent one.

**The two paths must agree**, because which one runs is invisible from the
source. `PlanOp::Aggregate` therefore carries the projection's `TypeDesc`:
`avg`'s *result* cannot say whether it was given floats, being `f64` either way.

**`min` skips absence, and needs help to.** `NONE` is `DynValue` variant 0, so it
sorts before every value. `dbsp`'s `Min` walks the cursor forward and returns the
first key with non-zero weight, so it would report absence for a group that
merely *contains* an absent row — where SQL's `MIN` skips nulls. `Max` calls
`fast_forward_keys()` and walks backward, so it is unaffected.

Feldera hit this and hand-wrote `MinSome1` for it, described in
`sql-to-dbsp-compiler/.../ir/aggregate/DBSPMinMax.java` as a "Special
hand-crafted DBSP aggregator for Min(Option<T>). None values are ignored" — and
has no `MaxSome`, for the reason above. `MinSkippingNone` in `lower.rs` mirrors
its semantics at `DynValue` rather than adopting its `Tup1<Option<V>>` shape,
which would put a second batch value type into a design whose leverage is that
there is exactly one. All three cases matter: the smallest present value; `NONE`
when the group has rows but none present; and no row at all when the group is
empty.

`Min`/`Max` have `Output = V`, so `aggregate(s, min|max, f)` produces
`OrdIndexedZSet(K, A)` where `A` is the type `f` projects. They compare using
`DynValue`'s `Ord` — see invariant 1 above, which is why that ordering matters
for results and not only for storage layout.

## Fixpoint

`fixpoint` lowers to `RootCircuit::recursive_dynamic(arity, f)`
(`dbsp/src/operator/recursive.rs:448`), which takes a **runtime** arity — the
shape a runtime-defined circuit needs — and returns one convergent stream per
recursive parameter. `dbsp` applies `distinct` to each recursive stream itself,
which is what makes the iteration terminate.

Base parameters are carried into the nested circuit with `delta0`, which needs
`HasZero` — another thing `TypedBatch` has and the erased batches do not.

**This is the one place the node list stops being flat.** A fixpoint node holds
a sub-plan whose `Import` and `RecVar` operators stand for an imported parent
stream and a recursive slot. They are resolved where both the parent's streams
and the child circuit are in scope, rather than in the body builder.

**The lowering is instantiated twice.** The nested circuit is `NestedCircuit`,
a different Rust type from `RootCircuit`, and `mono.rs` exposes operators as
inherent methods on concrete circuit types rather than through a trait — so a
function generic over `C: Circuit` cannot call them. The shared operator arms
live in one macro used by both builders, which is what keeps the copies from
drifting. This is a limitation of `dbsp`'s API surface, not of the design.

One wrinkle: `recursive_dynamic`'s closure returns `Result<_, SchedulerError>`
and cannot carry a `Diagnostic` out. A build failure inside it means the checker
and the lowering disagree, so it is parked in a slot and the closure returns its
inputs unchanged to keep the arity right; the diagnostic surfaces after the
call.

## Incrementalization principle

The language does **not** insert `integrate`/`differentiate` around non-linear
operators. `dbsp`'s `join`, `aggregate`, `distinct`, and `antijoin` are
already incremental: they take a stream of *changes* and internally maintain
the state needed to emit a stream of *changes*. `integrate` and `differentiate`
remain available as explicit primitives for two uses only:

1. materializing an accumulated relation (running sum of deltas), and
2. constructing feedback/recursive structures (together with `delay`).

This is the key difference from the Erlang grasp-dbsp, which inserts I/D pairs
as a compilation step. Here that step does not exist because it is unnecessary.

`constant` is the one place the lowering inserts a `differentiate` of its own,
and it is not an exception to the rule above. That rule is about not wrapping
non-linear operators in I/D pairs. This is a *coercion*: the operator names a
relation, every stream carries a change, and `differentiate` is the map from one
to the other.

### Why `constant` is not just a generator

`Generator` and `ConstantGenerator` fire once per **step**, and a transaction may
span many (see [Execution model](#execution-model)). So `ConstantGenerator` alone
is a relation *held constant*, not the change stream that represents one.

`differentiate` is that conversion. It expands to `x − z⁻¹(x)` over a
step-aligned `Z1` (`dbsp/src/operator/differentiate.rs`, `z1.rs`), so the steps
of a transaction telescope:

| | `ConstantGenerator` | `z⁻¹` | `differentiate` |
|---|---|---|---|
| transaction 1, step 1 | `X` | `0` | `X` |
| transaction 1, later steps | `X` | `X` | `0` |
| transaction 2+, any step | `X` | `X` | `0` |

The steps of a transaction sum to `X` in the first and to `0` in every later one,
whatever the step count.

Two nearby constructions are wrong, and both look right:

- **Not `transaction_delay`.** `TransactionZ1` returns the same value on every
  step of a transaction, which is right for an accumulated stream and wrong for a
  delta stream — it would emit the batch once per step.
- **Not a `TransactionGenerator` holding a `bool`.** That is transaction-aligned,
  but the flag is state `dbsp` does not checkpoint, whereas `Z1`'s is; every
  worker would emit its own copy, whereas `ConstantGenerator` guards on
  `Runtime::worker_index() == 0`; and it is not a deterministic source, so a
  `constant`-fed view added to a running pipeline would be refused a concurrent
  bootstrap.

One ordering consequence: `delay()` derives its persistent id from its input's
**at construction time**, and the caller names what the lowering returns — which
is the `minus`, too late for the stream underneath. So the generator's stream is
named before `differentiate` is built, the same care a recursive stream needs.

## Input / output

**Input.** Each `input` node yields an input handle. Records arrive as JSON and
are decoded (via `TypeDesc`) into `DynValue`s, which are pushed onto the handle
with a weight. Pushes are buffered and applied atomically at the next clock
cycle.

**Output.** Output nodes are not marked in the source; the set of output nodes is
supplied when the runner starts, by node name. Each named node gets a
`Stream::output()` handle, which requires only `T: Debug + Clone + Send`
(`dbsp/src/operator/output.rs:41`) and so applies to any node in the program. A
node may be named either by its declared name — including a circuit body node's
dotted `<instance>.<node>` path — or by its **content id**, which is how a
nested node with no declared name is selected. A name matching neither is a
startup error. Nodes that are not named are still constructed: there is no
dead-code elimination.

At each clock cycle the handle exposes the deltas produced by the circuit,
encoded back to JSON. The runner emits these deltas as they are produced; it does
not materialize a final table.

**A dict is an object.** One wire shape, whatever its key type: the key's own
type says how to spell it and how to read it back, so `dict(i64,V)` writes
`{"1": …}` and parses `"1"` as an `i64`. This is why the key types are the
scalars and no more — a composite key has no object-key spelling, and the
alternatives were a second wire shape for some dicts or JSON text nested inside
the key. `DynValue::dict_key_string` and `TypeDesc::parse_dict_key` are the one
definition of that spelling, shared by the codec and by `cast(d, json)` so the
two cannot disagree.

A non-finite `f64` key is refused. Non-finite floats already write as `null` in
value position, and `null` is not a legal object key, so there is no spelling to
give it — the codec says so rather than inventing one.

**Encodings.** Two formats, both Feldera-native. See
<https://docs.feldera.com/formats/json/>
(`feldera-types/src/format/json.rs:79-120`):

- `weighted` — `{"weight": 2, "data": {…}}`. This represents a Z-set delta
  exactly, including weights whose magnitude is greater than one, and is the
  **default for output**.
- `insert_delete` — `{"insert": {…}}` / `{"delete": {…}}`. Feldera's own
  default. It has no way to express a weight of 3 other than repeating the row
  three times, so the encoder expands by `|w|` and the decoder reads each record
  as `±1`.

`weighted` is the default **here**, because this runner's output is a stream of
Z-set deltas and `weighted` is the only format that represents one exactly. A
future API surface should default to `insert_delete`, matching Feldera, whose
connectors speak it.

**The two envelopes differ in shape, deliberately.** `weighted` has three
top-level slots, so an indexed delta spreads across them; `insert_delete` has
exactly one slot whose body *is* the payload, so an indexed delta nests inside
it:

```
flat      {"weight": 1, "data": {"id": 1}}      {"insert": {"id": 1}}
indexed   {"weight": 1, "key": 10,              {"insert": {"key": 10,
                        "value": {"id": 1}}                  "value": {"id": 1}}}
```

The indexed encoding is **ours**, not a Feldera convention: Feldera's relations
are flat, so it has nothing to say about a keyed stream. It is an *output*
shape only — an input is always an `input` node, which is always
`zset(record(...))`, so there is no indexed decoder and nothing that would use
one.

**Encoding refuses what decoding would refuse — with one exception.**
`encode_value` will not write a `null` into a column that is not optional: a
declared type is a promise, and the codec is where a broken one surfaces.
`tests/invariants.rs` pins it as a property — generated values of a generated
type encode and decode to themselves.

The exception is non-finite floats. NaN and the infinities *are* `f64` values;
JSON simply has no syntax for them, and `serde_json`'s `serialize_f64` writes
`null`, as Feldera therefore does. So they are written as `null` too, and such a
row does not decode back into the same schema. That is a different thing from
the refusal above it — `NONE` is not an `f64` at all — and the round-trip
property test excludes them for exactly this reason, with
`non_finite_floats_encode_as_null` pinning why.

**Conventions Feldera fixes, which this codec follows.** A null may be written
as JSON `null` *or by omitting the column entirely*; both decode to `NONE`.
When the deferred types land they encode as: `DATE` `YYYY-MM-DD`, `TIME`
`HH:MM:SS.fff`, `TIMESTAMP` `YYYY-MM-DD HH:MM:SS.fff` or RFC3339, `DECIMAL`
preferably a **string** so precision survives, `VARIANT` any JSON value.

Two further Feldera formats are not implemented: `raw`, where a bare object
means an insert, and the `update` operation for keyed partial updates, which
needs primary keys the language does not have.

## Execution model

A **transaction** is the semantic unit, not a step. `transaction()` starts,
commits and drains one logical clock tick; the clock advances between
transactions, not within them. `step()` is a scheduling knob — it returns
`false` while a transaction is still in progress and `true` once the commit is
complete, so a single transaction may span many steps when inputs are large
(`dbsp/src/circuit/dbsp_handle.rs:1673-1712`). The runner's unit of "feed input,
read output deltas" is the transaction.

A source therefore has to decide what "once" means for itself. `input` drains its
mailbox on the first step; `constant` telescopes with `differentiate`
([above](#why-constant-is-not-just-a-generator)).

**A sink has to decide the same thing, and the answer is not `output`.**
`Stream::output`'s mailbox is *overwritten* each step rather than accumulated,
so a reader gets the last step's batch rather than the transaction's. At one
worker that is the same thing, because these transactions are one step. At
several it is not: a commit spans steps and the workers emit in different ones
(`dbsp/src/operator/output.rs:579-594`), so a reader sees whichever part of the
transaction happened to land last — rows missing, differently on every run.
This was not a hazard to note and revisit; it was wrong, and the corpus could
not see it, because every fixture ran at one worker.

So every output is a `Stream::accumulate_output` sink. Each worker's accumulator
emits once per transaction, and the operator parks those emissions in a cohort
shared by the workers of one host, publishing them together when the last worker
emits — "so a reader sees either all of a transaction's outputs or none". The
runner then reads `concat().consolidate()`, which is the read-side merge over
the whole transaction. A transaction spanning many steps is now ordinary rather
than a thing to revisit.

## Checkpointing and parallelism

**Persistent ids.** Every stateful operator carries one, and it is the node's
[content id](#a-nodes-identity-is-its-content) — set through
`Stream::set_persistent_id` as each node is built. Feldera's own dataflow IR
carries one per node for the same reason (`crates/ir/src/mir.rs:21-46`).

Inside a `fixpoint` the requirement is stricter, and shapes the code rather than
decorating it. In persistent mode a missing id fails the checkpoint outright with
`NoPersistentId`, and `dbsp` derives the ids of the implicit `distinct` and of
the exporting integral from the recursive stream's own id **as they are
constructed** — so each recursive stream is named as the closure's first act,
before `build_body` builds anything from it
(`dbsp/src/operator/recursive.rs:150-171`).

**Determinism.** `Runtime::init_circuit` clones the constructor closure into
**every worker thread** and runs it there — and does **not** check what they
built: it keeps worker 0's answer, saying so in as many words
(`dbsp/src/circuit/dbsp_handle.rs:1121-1125`, "we don't check"). An earlier
version of this paragraph claimed it asserted identical fingerprints. It does
not, and the difference matters: what determinism protects is the per-worker
`Runtime::sequence_next` counter that hands out input and exchange ids
(`dbsp/src/circuit/runtime.rs:1305-1313`), so workers that build different
circuits desync it and then deadlock or cross-wire, with no error anywhere.

Lowering is therefore deterministic by construction: nodes are visited in plan
order, content ids and the output selection are computed before the closure, and
no `HashMap` iteration reaches circuit construction. The one check `dbsp` does
make is `create_bootstrap_circuit`, which re-runs the constructor on each worker
and refuses a fingerprint mismatch (`dbsp_handle.rs:977-991`);
`Runner::recheck_determinism` borrows it, and `tests/workers.rs` calls it. It
compares each worker's two builds rather than the workers with each other, so
what it catches is a constructor that is not a pure function of what it
captured.

**Sharding.** Multi-worker execution shards batches by `key.default_hash()`, so
invariants 2 and 3 above are prerequisites for running with more than one worker,
not optional polish. The count is `RunnerConfig::workers`, a `NonZeroUsize`
because `Layout::new_solo` asserts rather than diagnosing
(`dbsp_handle.rs:105-108`), and it defaults to one. Nothing derives it from the
host: a count from `available_parallelism` would make a placement bug reproduce
on a three-core machine and not on a four-core one.

Which operators need placement is `dbsp`'s business, not this crate's: `join`,
`antijoin`, `distinct` and the aggregates shard their own inputs, and an
embedder that shards on their behalf "is likely to lead to incorrect results"
(`dbsp/src/operator/communication/shard.rs:50-57`). Nothing here calls `shard`.
Input rows are pushed round-robin rather than by key
(`dbsp/src/operator/dynamic/input.rs:690-702`), which is why that is safe.

The evidence is `tests/workers.rs`: one program at one worker and at three,
required to agree delta for delta. Three rather than two or four because
placement functions differing only in a hash's high bits still agree modulo a
power of two (`dbsp/src/operator/dynamic/input.rs:2175-2181`). Every program
there retracts, because the read side merges every worker whatever the
placement — a program that only inserts gets the right answer even when a key's
rows are scattered, and what a misplacement breaks is a retraction cancelling an
insertion.

**A checkpoint will be partitioned by worker.** Batch files are written under a
`w{worker_index}-` prefix (`dbsp/src/storage/file/writer.rs:1161-1162`), and
Feldera's own adapters refuse to change the worker count across a restore
(`crates/adapters/src/controller.rs:5863-5866`). So whatever eventually sets the
count has to record it with the checkpoint and reject a mismatch; it is not a
free tuning knob once anything is stored.
