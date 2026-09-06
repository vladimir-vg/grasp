# DBSP Runner — `json` (design)

> **Status: design, not implemented.** The other documents in this directory
> describe what the runtime does today; this one describes a type that has been
> designed but not built. When it lands, this content folds into
> [`language.md`](language.md)'s type system and [`mapping.md`](mapping.md), and
> this file goes away.
>
> It depends on the expression-language rewrite in
> [`expressions.md`](expressions.md), which it motivated.

## `json` is its own type, not a wrapper

The tempting model is "a `json` wraps an ordinary value, and you unwrap it".
That model is wrong, and getting it wrong shapes everything downstream. A JSON
document is stored as a document, has shapes the language's own type vocabulary
does not have, and cannot be viewed as a value of some other type.

So: **`json` is its own type, and matching it is a *parse*, not a test.** A
`record` pattern against a document extracts fields, converts them, and builds a
new record. That is the operation the type exists for, and it is why conversion
had to become pattern matching rather than a `cast` function.

## Representation

`json` is backed by [`feldera_sqllib::FlatVariant`][fv] — `{ buf: Arc<[u8]>,
start: u32, len: u32 }`, a byte-encoded document.

[fv]: ../../vendor/feldera/crates/sqllib/src/flat_variant.rs

This is the pattern [`mapping.md`](mapping.md)'s invariants section already
identifies as the robust one, and choosing it is the single most load-bearing
decision here, because it earns three of the four invariants **by
construction**:

- **Archived ordering equals in-memory ordering** — the archived form *is* the
  byte encoding, so there are not two orderings that could disagree.
- **`Eq` and `Hash` agree** — both route through functions over the same bytes
  (`eq_values`, `cmp_values`, `hash_value`).
- **The hash is stable**, being a walk over those bytes.

And a fourth property that is not on that list but matters just as much: **map
entries are stored sorted and deduplicated** (`sort_map_entries`). So
`{"a":1,"b":2}` and `{"b":2,"a":1}` produce identical bytes — one value, one
hash, one Z-set key. Key order in the source JSON is canonicalized away rather
than becoming a silent non-annihilation bug.

`FlatVariant` already implements `Ord`, `Hash`, `SizeOf`, `rkyv::Archive`,
`rkyv::Serialize`, `IsNone` and serde — essentially the whole `DBData` list —
and is exported from `sqllib`. So the runtime cost of adding `json` is one
`DynValue` variant.

The one invariant it does **not** give us is numeric normalization: `cmp_values`
compares the type tag first, so every integer sorts before every float. See
[Numbers](#numbers).

## Type vocabulary

| type | meaning |
|---|---|
| `json` | a document, any shape |
| `dict(K, V)` | a document known to be an object |
| `array(T)` | a sequence |

`dict` and `array(json)` are the **same representation** as `json` with a
refined static type — no conversion, no allocation, so testing for them is O(1).
`array(T)` for `T ≠ json` is a real `Vec` (a new `DynValue::Array`), converted
element by element when a pattern matches it.

That split is deliberate. Keeping json arrays as documents means the common case
costs nothing; having a real `Vec` for typed arrays means `array(record(…))` is
expressible, which a document representation could not do — `FlatVariant` has
`TAG_ARRAY` and `TAG_MAP` and no concept of a record.

It does mean the invariant burden is not entirely avoided: `DynValue::Array`
is a recursive container whose `Ord`, `Hash` and archived ordering are ours,
so `tests/invariants.rs`'s proptest has to cover it.

### `dynamic` is deferred

A `dynamic` type — "holds anything" — was designed alongside `json` and left
out. `FlatVariant` already carries `Date`, `Timestamp`, `Decimal`, `Uuid` and
`Binary` tags, so `dynamic` would be `json` with those permitted. But nothing in
the language can *produce* one until the `sql.*` vocabulary lands, so today the
two types would be indistinguishable in every observable way. It becomes a
one-line addition when there is something to put in it.

### `optional(json)` is disallowed

**A `json` is never absent. `null` is a value it holds.**

Feldera has nullable `VARIANT` because SQL requires every type to be nullable.
There is no such constraint here, and the type earns nothing:

- `json` already has a null value, so absence adds no expressiveness
- an `optional(json)` column can never *receive* a json-null from input — the
  `Option` deserializer consumes the `null` first, so the case is structurally
  unreachable
- on output it is ambiguous anyway, since `NONE` and json-null both emit `null`

Disallowing it makes the rule stateable in one line, and keeps the two patterns
cleanly disjoint — `null` matches a document that is JSON null, and `NONE` is
simply not applicable to a `json`. That is the distinction
[`language.md`](language.md) went out of its way to establish, preserved at the
one boundary where it was at risk.

## Numbers

**Documents keep their tags.** Storage is lossless and round-trip is exact:
`{"id": 5}` is stored as an integer and comes back as `5`.

**The language exposes json numbers only as `f64`.** So `i64`,
`dict(string, i64)` and `array(i64)` patterns are unsatisfiable against a `json`
scrutinee, and produce the note described in
[`expressions.md`](expressions.md#exhaustiveness-and-dead-arms).

That is the whole rule, and it removes the tag split from the language: there is
one number type to match, and a program never has to handle `5` and `5.0`
separately.

### Why, and what it costs

JSON's grammar does not distinguish integers from floats, but every JSON parser
does. Feldera keeps that distinction all the way through — `visit_i64 →
TAG_BIGINT`, `visit_u64 → TAG_UBIGINT`, `visit_f64 → TAG_DOUBLE`, with no
normalization — and resolves `5` versus `5.0` by requiring a cast before use.
Its casts are deliberately permissive: `TryFrom<Variant> for i64` accepts *any*
numeric tag, and even a numeric string. So Feldera's answer is "don't compare
variants, cast them out and then compare."

Exposing one number type reaches the same place by a shorter road, and the costs
are real and worth stating rather than burying:

- **A json-derived numeric column is `f64` on the wire.** `{"id": 5}` in,
  `5.0` out, once the value has been through a projection.
- **Integers above 2^53 lose exactness.** `9007199254740993` becomes
  `9007199254740992`. Snowflake IDs, 64-bit database keys and nanosecond
  timestamps all live up there, and as join keys they collide.
- **There is no repair.** The language has no conversion between two known
  types at all — `floor`/`ceil`/`round` are type-preserving and there is no
  `cast` — so an `f64` that came out of a document stays one.

Integrality remains checkable with what exists (`x - floor(x) == 0.0`), which
tells you whether a value was written without a decimal point, but does not
recover precision that was already lost.

## Nulls

There are three distinguishable things, and which one you get depends on where
the `null` appears:

| position | `null` decodes to |
|---|---|
| a `json` column | **json-null**, a value (`TAG_VARIANT_NULL`) |
| an `optional(T)` column, `T ≠ json` | **`NONE`**, absence |
| nested inside a document | **json-null**, always |

The third row is not a special case — nesting is parsed inside `FlatVariant`'s
own deserializer (`visit_unit → TAG_VARIANT_NULL`), which never sees the column
type. Feldera behaves identically, and for the same reason: its `Option`
wrapper consumes a top-level `null` before `FlatVariant` is invoked, so column
nullability decides, and nested nulls are always variant nulls.

A **missing key** inside a document is a fourth thing again — `TAG_SQL_NULL`,
absent — and it is what makes navigation total: indexing a document at a key it
does not have yields the absent sentinel rather than failing, so `{user: {id: x,
**}, **}` chains without every level needing a guard.

```
{"user": {}}              # the id position is absent
{"user": {"id": null}}    # the id position is json-null
```

Telling those two apart is the job of a predicate function — see
[Open questions](#open-questions).

### Encoding is lossy, in the same way Feldera's is

`TAG_SQL_NULL` and `TAG_VARIANT_NULL` both serialize to `null`. So absence and
json-null are indistinguishable on the wire. Round-trip is still stable for a
non-optional column: `null` out, `null` back in, json-null again.

### The decoder needs one more arm

`decode_value` (`json.rs`) currently intercepts `null` *before* consulting the
type, so a `json` column would reject a bare `null` document as "the column is
not optional". It needs the type consulted first:

```rust
if j.is_null() {
    return if ty.is_optional()        { Ok(DynValue::None) }
           else if matches!(ty, Json) { Ok(json_null()) }
           else                       { bad(...) };
}
```

## Where a `json` may be used

Ordering is tag-first, which is a fine canonical order for storage and an
arbitrary one for a query result — it puts every number before every string, and
every integer before every float.

- **Allowed**: as an index or join key, inside records, and with `==` / `!=`.
  The encoding is canonical, so equality and hashing are sound and grouping by a
  whole document works correctly.
- **Rejected**: as a `min`/`max` projection, and as an operand of `<`, `<=`,
  `>`, `>=`. Cast first.

This is stricter than Feldera, which permits both. The reason to be stricter is
that [`mapping.md`](mapping.md) already notes the runtime ordering is
load-bearing for query results and not merely for batch layout — and a `min`
that returns "the smallest by tag" is a result nobody asked for.

## Accessors

`.` is **record-only**, resolved to a positional index at check time. It is not
overloaded onto documents: identical syntax meaning a constant offset in one
place and a binary search over sorted keys in another would be a performance
cliff hidden behind a dot.

Documents are reached two ways, and they do different jobs:

- **Structural patterns** branch on shape, for a path known at compile time.
- **`get`, `has_key`, `length`** reach a computed key — and they operate on a
  **`dict`, not on a raw `json`**. You must match a document to a `dict` first.

```
match(doc,
    d::dict(string, json) -> get(d, some_key),      # computed key
    _ -> …)

match(doc,
    {user: {id: x, **}, **} -> x,                   # known path
    _ -> …)
```

Requiring a `dict` for the accessors is what keeps the two mechanisms from
becoming interchangeable: `get` cannot be used to walk a document without first
establishing that each level *is* an object, which is exactly the check a
structural pattern does in one step.

`keys(d) : array(string)` becomes expressible once arrays are values, which is
what makes enumerating a document's keys possible at all.

## Ingestion

A document enters through an ordinary input column:

```
events := input("events")
events :: zset(record(id: i64, payload: json))
```

`decode_value(j, TypeDesc::Json)` builds a `FlatVariant` from the incoming
`serde_json::Value` — `FlatVariant` implements `Deserialize` — and encoding back
is its `Serialize`.

The `weighted` and `insert_delete` envelopes are unaffected. A document nests
inside `data` or inside the insert body, so a payload containing keys named
`"weight"` or `"insert"` cannot collide with the envelope.

Because `optional(json)` is disallowed, a `json` field is never absent, so a
record missing its `payload` key is a decode error rather than a null. That
differs from the Feldera convention of treating an omitted column as null, and
it follows from the type rather than being a separate choice.

## What this costs

- `DynValue` gains `Json(FlatVariant)` and `Array(Vec<DynValue>)`; `TypeDesc`
  gains `Json`, `Dict` and `Array`. Both new `DynValue` variants must be
  appended — the variant order is the archived discriminant, which is a storage
  format.
- `tests/invariants.rs`'s archived-ordering proptest must generate both. `Json`
  should pass by construction; `Array` is the one whose ordering we own and the
  one the test exists for.
- `decode_value` and `encode_value` gain `Json` arms, and `decode_value`'s null
  interception has to consult the type.
- Pattern matching against documents is new machinery in the type checker and
  the evaluator, described in [`expressions.md`](expressions.md).

## Open questions

1. **The absent-versus-json-null predicate.** A missing key and an explicit
   `null` are distinguishable in the representation, and with structural
   patterns unable to express "this key is absent", a predicate is now the only
   way to tell them apart. It has been named but not specified.
2. **Is `zset(json)` legal for a derived node?** `input` stays record-only, but
   nothing obviously prevents a derived stream of bare documents.
3. **Should `dict(K, V)` for `V ≠ json` ever be constructible?** It is rejected
   as a pattern against a document because converting every value is O(document)
   rather than O(pattern). But `array(T)` for typed `T` *is* allowed, and does
   the same amount of work — so the two are currently asymmetric for the same
   cost.
