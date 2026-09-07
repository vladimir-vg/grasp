# DBSP Runner — `json` (design)

> **Status: design, not implemented.** The other documents in this directory
> describe what the runtime does today; this one describes a type that has been
> designed but not built. When it lands, this content folds into
> [`language.md`](language.md)'s type system and [`mapping.md`](mapping.md), and
> this file goes away.
>
> **It now owns `match` as well.** An expression-language rewrite was designed
> alongside this type and has since been split. The parts that stand on their
> own have landed and are described in [`language.md`](language.md): `function`
> replacing `fun`, `record(key:, value:)` instead of `(key, value)` pairs, and
> arrays instead of list syntax. What is left here is what only `json` needs —
> `match`, type patterns, structural patterns, open records, `dict`, and
> exhaustiveness. Converting a document *is* pattern matching, which is why
> there is no `cast` in the plan.
>
> Two things below need revisiting before this is built, both raised by the
> review that produced the split. `dict(K, V)` is described as a refinement of
> `json` — the same bytes, a narrower static type — but a `dict(i64, json)`
> cannot come from parsed JSON, whose object keys are always strings; if `dict`
> is to be a constructible map value it is a second type with its own
> representation, ordering and encoding, not a refinement. And the numeric-tag
> question under [Numbers](#numbers) is a matter of **key identity**, not only
> of sort order: two documents the language shows identically can be different
> Z-set keys, which bears on whether `json` may be a join key or an `==` operand
> at all. See invariant 4 in [`mapping.md`](mapping.md).

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
[Exhaustiveness and dead arms](#exhaustiveness-and-dead-arms).

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
  the evaluator, new machinery, designed in the pattern sections below.

## `match`, and the pattern language

Converting a document is a *parse*, not a test, so conversion had to become
pattern matching rather than a `cast` function. That is the part of the
expression rewrite that did not stand on its own, and it is kept here.

### `match`

Sequential, first match wins, and every `match` must be exhaustive.

```
match_expr := "match" "(" scrutinee "," arm ("," arm)* ")"
scrutinee  := expr | "(" expr ("," expr)+ ")"
arm        := pattern "->" expr
```

The scrutinee may be a **list** of expressions, matched against a
correspondingly wide pattern:

```
match((a, b),
    record(key: NONE, value: NONE)   -> 0,
    record(key: x::f64, value: NONE)   -> x,
    record(key: NONE, value: y::f64) -> y,
    record(key: x::f64, value: y::f64) -> x + y)
```

That list is *not* a tuple value — there is no tuple type and nothing is
constructed. `match((a), …)` is illegal; a single scrutinee is written
`match(a, …)`.

`return` is a statement and arms are expressions, so `return` cannot appear
inside an arm:

```
match(x, 0 -> return 1, _ -> return 2)      # error
return match(x, 0 -> 1, _ -> 2)            # the form that works
```

#### Patterns

| form | binds | notes |
|---|---|---|
| `NONE`, `null`, `0`, `""`, `true` | — | value patterns |
| `x::f64`, `x::string`, `x::bool` | the value | type patterns |
| `record(id: i64, name: string, **)` | a constructed record | `**` means "and other fields" |
| `dict(string, json)`, `array(json)` | the value | shape tests |
| `{key: pattern, **}` | nested bindings | structural, `json` only |
| `[first, *]` and friends | elements | structural, `json` only |
| `_` | — | wildcard |

**Type patterns are the only conversion mechanism in the language.** There is no
`cast`. `x::f64` against an `optional(f64)` unwraps it, and against a `json`
converts it — so the same body handles both, with the `NONE` arm simply dead in
the first case:

```
match(v, NONE -> 0, x::f64 -> x)
```

There is no conversion between two *known* types. `floor`/`ceil`/`round` are
type-preserving, and nothing turns an `f64` into an `i64`. See
[`json.md`](json.md) for what that means for numbers coming out of documents.

##### Open records

`record(id: i64, name: string, **)` matches a record or document that has *at
least* those fields, and binds a **closed** record of exactly the named ones —
the extras are tested against nothing and discarded, which is visible in the
bound variable's type.

**`**` is pattern-only. It is never a type.** An open record type would break
the invariant that makes the value model work: [`mapping.md`](mapping.md) states
that field names live only in `TypeDesc` and never in the value, so a value
carrying fields the schema does not name would either need names back in every
row or would silently lose them.

Because `record` patterns extract and convert rather than merely test, they are
**parses**: each field is a lookup, a conversion and an allocation, failure is
all-or-nothing, and a pattern that fails on its last field has already paid for
the earlier ones. **Arm order is a performance decision as well as a semantic
one.**

##### Structural patterns

`{…}` and `[…]` patterns are for `json` scrutinees only, and their nested
positions hold further structural patterns — **not** type ascriptions:

```
{user: {id: x, **}, **}          # legal — x : json
{user: {id: x::f64, **}, **}     # illegal — no type ascriptions inside
```

Navigation and conversion never happen in one pattern. They compose through a
nested `match`, which is more verbose and keeps each construct doing one thing:

```
match(doc,
    {user: {id: x, **}, **} -> match(x, n::f64 -> n, _ -> 0),
    _ -> 0)
```

Array patterns allow **at most one `*`**, in any position:

```
[first, *]           # at least one
[*, last]            # at least one
[first, *, last]     # at least two — element 0 and element n−1
[a, b]               # exactly two
[a, b, *]            # at least two
```

There is no `*name` or `**name`. A binding for the *rest* of a container is the
only pattern form that would force a copy — a middle run of elements has no byte
range that is itself a container, so binding one means building a new document,
O(n) per row per arm tried. Dropping it keeps every pattern's cost proportional
to what was written. The cost is that object-spread in a source language has no
lowering.

##### Binding copies, and matching is two-phase

A binding **copies**. Views into the source document would be cheaper — an `Arc`
clone and a range — but a small field extracted from a large payload would then
retain the whole payload for as long as the derived Z-set lives, invisibly.
Lazy materialization with escape analysis is future work.

Because bindings cost something, matching runs in two phases: **test the whole
pattern, then materialize bindings only on success.** Otherwise a pattern that
binds a large sub-document and then fails on a later position pays for a copy it
throws away, once per row per arm.

#### Arm unification

The type of a `match` is its arms' types unified. This is the existing `unify`
(`typecheck/infer.rs`) folded across the arms — the machinery is already there,
including the part that matters most: `Ty::None` is the polymorphic `NONE`, and
`unify(Known(t), Ty::None)` already yields `optional(t)`.

```
match(x, n::f64 -> n, _ -> NONE)        # optional(f64)
```

Where the fold lands on `Ty::None` — every arm is `NONE` — the type is genuinely
ambiguous and needs a typespec. Since `return match(…)` has nowhere to put one,
that means binding it to a name:

```
r :: optional(f64)
r := match(x, _ -> NONE)
return r
```

**Arm unification does not promote numerics** — and neither does anything else
any more, so there is nothing to reconcile. When this was written, arithmetic
promoted `i64 + f64` to `f64` while arm unification would not, and the plan was
to split `unify` in two. Implicit promotion has since been deleted from the
language, so the single `unify` in `typecheck/infer.rs` already has the
behaviour arms need and `match` can use it unchanged.

#### Exhaustiveness and dead arms

A trailing `_` establishes exhaustiveness. (Whether full enumeration of a finite
shape set also counts is [open](#open-questions).)

**Dead arms are allowed.** They are the point of templates — the same body is
exhaustive at one call site and not at another, and an arm that cannot match at
one instantiation is live at the next:

```
function norm(v) {
    return match(v,
        NONE      -> 0,
        x::f64    -> x,
        s::string -> length(s),
        _         -> 0)
}
```

At `v : f64` the `string` and `NONE` arms are dead; at `v : json` they are live.
Making that an error would make the template useless, so unreachable arms are
not diagnosed — with one exception.

**Notes are emitted for patterns unsatisfiable against a `json` scrutinee**, and
only there. That is the case where the author's mental model is most likely
wrong, because what a `json` can yield is deliberately narrower than the type
vocabulary:

```
match(doc, n::i64 -> n, _ -> 0)               # note: json numbers are f64
match(doc, r::record(id: i64, **) -> …, …)    # note: on the `id` field
```

The note recurses into record patterns and points at the *field*, since writing
`id: i64` out of habit is the likeliest mistake in the whole feature and a
head-only note would miss it. Notes are deduplicated by pattern span, so a
helper used at six call sites reports once.

Notes need plumbing that does not exist: `compile()` returns
`Result<Plan, Vec<Diagnostic>>`, so a diagnostic on a *successful* compile has
nowhere to go, and nothing in `src/` produces a `Severity::Note` today.

### What the pattern language costs

- `TypedExpr` gains a `Match` node with binding slots, and the evaluator gains
  the two-phase test-then-materialize rule above.
- Notes need plumbing that does not exist: `compile()` returns
  `Result<Plan, Vec<Diagnostic>>`, so a diagnostic on a *successful* compile has
  nowhere to go, and nothing in `src/` produces a `Severity::Note` today.
- The YAML harness enforces **exactly one** of `expected_output` /
  `expected_exact_output` / `expected_diagnostics` (`tests/yaml.rs`), so a case
  that compiles *and* emits a note cannot assert both. That rule has to relax,
  and `tests/cases/README.md` documents it.
- Body bindings in a `function` become load-bearing rather than optional: a
  `match` arm that needs a computed value has nowhere else to put it. They are
  listed under future work in [`overview.md`](overview.md) and bring a flat slot
  table with them.

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
   cost. This has grown a second half since `array(T)` landed as a real value:
   if `dict` is to be constructible at all it is a container of our own, with
   ordering, hashing and JSON encoding to define, and not the zero-cost
   refinement of `json` this document describes.
4. **Do statically-dead arms contribute their type to arm unification?**
   Excluding them is what makes templates work — otherwise the `norm` example
   above fails at every instantiation rather than none. But then an expression's
   type depends on which arms are statically reachable, so adding a field to a
   record can bring a dead arm to life and change a function's return type at
   that call site. Note that the example does not typecheck under the
   no-promotion rule either way: its arms are `f64` and `i64`, and arm
   unification does not promote.
5. **Exhaustiveness beyond the trailing wildcard.** A trailing `_` is
   sufficient. Whether full enumeration of a finite shape set also counts is
   undecided — without it, `match(v, NONE -> 0, x::f64 -> x)` needs an
   unreachable `_` even though it is total.
