# grasp — Types

The type vocabulary, when one type may stand where another is expected, and
where a check at runtime substitutes for a proof at compile time.

How types are *found* is [`inference.md`](inference.md); the grammar of a type
is in [`syntax.md`](syntax.md#grammar).

## Value types

| type | meaning |
|---|---|
| `boolean` | `true` or `false` |
| `i64` | signed 64-bit integer |
| `f64` | 64-bit IEEE float |
| `string` | UTF-8 text |
| `optional(T)` | a `T`, or absent |
| `record(f: T, …)` | named fields, each with its own type |
| `array(T)` | a sequence of one element type |
| `dict(K,V)` | a key-value map, `K` a scalar |
| `json` | a document of any shape |

That is the whole vocabulary, and it is exactly what
[grasp-dbsp](../grasp-dbsp/language.md#value-types) can carry. The wider set the
Erlang implementation offers — `dynamic`, the narrower integers, `f32`,
arbitrary-precision `integer` and `numeric`, `bytes`, `bits`, the temporal
types, general `enum` — is listed under
[future work](overview.md#future-work), each blocked on a grasp-dbsp value type.

**Scalars** are `boolean`, `i64`, `f64` and `string`. The word matters in two
rules below: what may key a dict, and what may be compared.

## Relation types

```
relation(col: T, …)
```

A relation is a set of tuples with named, typed columns. It is a different kind
of thing from a value type: relations are what rules define, and a relation
cannot appear inside a value. There is no `array(relation(...))`.

A `:: relation(...)` spec is required for a relation declared `<- input`, whose
columns nothing else can determine, and optional elsewhere.

## Type identity

Two types are the same type when they are structurally equal, with three
qualifications.

**Record fields are a set.** `record(a: i64, b: string)` and
`record(b: string, a: i64)` are one type. Field order is not part of identity,
so a program that builds the same record on two code paths need not agree on the
order it names them in.

**`optional` does not nest.** `optional(optional(T))` is not a distinct type —
there is one absence, so a doubly-optional type has no value the singly-optional
one lacks. Writing it is an error rather than a silent collapse, because it
usually means a mistake about which layer was already optional.

**A dict key is a scalar.** `dict(K,V)` requires `K` to be `boolean`, `i64`,
`f64` or `string`. This is a JSON constraint rather than a representational one:
a dict is an object on the wire and an object's keys are strings, so a key type
must have one string spelling its own type reads back. No composite type does.

> ``a dict key must be `boolean`, `i64`, `f64` or `string`, found `record(...)` ``

A dict literal written with `:` — `{name: v}` — has a string key by
construction, so it constrains `K` to `string`. Mixing that with a non-string
`=>` key in one literal is the ordinary key-type conflict, and reported as one.

> ``this dict is keyed by `string`, but `1` is an `i64` ``

## Assignability

`S` is **assignable** to `T` — written `S → T` — when a value of type `S` may
stand where a `T` is expected. This is what the compiler checks when a rule's
inferred column type meets a declared one, when an argument meets a parameter,
and when several rules define one relation.

**There is no implicit conversion.** Assignability never changes a value's
representation; it only says a value already *is* an acceptable `T`. Anything
that would change the value is a `cast` or a builtin, written out.

### Scalars

| `S` | `T` | |
|---|---|---|
| `T` | `T` | yes — identity |
| `i64` | `f64` | **no** |
| `f64` | `i64` | **no** |

Numbers do not widen. `i64` and `f64` do not meet, in either direction: the
first loses nothing but the second loses precision, and having one direction
work silently is how a program acquires a rounding it never asked for. Convert
with a builtin.

An untyped numeric literal is not covered here — it has no type yet, and
[`inference.md`](inference.md#phase-3-literals-take-their-type-from-context)
resolves it against context.

### `optional`

| `S` | `T` | |
|---|---|---|
| `T` | `optional(T)` | yes — a value is an acceptable optional |
| `S` | `optional(U)` | yes if `S → U` |
| `optional(S)` | `optional(U)` | yes if `S → U` |
| `optional(T)` | `T` | **no** — but `v :: T` filters, below |
| `NONE` | `optional(T)` | yes, for any `T` |

`NONE` has no type of its own; it takes `optional(T)` from wherever it sits.
With nothing to take one from it is an error, not a guess.

### Compound types

All three are covariant, and none admits a change of shape.

```
array(A)     → array(B)        if A → B
dict(K1,V1)  → dict(K2,V2)     if K1 → K2 and V1 → V2
record(S)    → record(T)       if the field names are equal as sets,
                               and each S field → the T field of that name
```

A record with an extra or a missing field is never assignable — there is no
width subtyping. That is deliberate: a rule head must name every column of its
relation, so a missing field is a mistake the checker can see rather than a
value silently carrying less than it claimed.

> ``record(a: i64) is not assignable to record(a: i64, b: string): missing field `b` ``

### `json`

A `json` value is a document. Assignability into it asks whether the source is
already a JSON value:

| `S` | `→ json` | |
|---|---|---|
| `string` | yes | |
| `f64` | yes | JSON numbers are IEEE doubles |
| `boolean` | yes | |
| `array(json)` | yes | |
| `dict(string, json)` | yes | an object |
| `i64` | **no** | JSON has one number type, and it is not this one |
| `record(...)` | **no** | a record has a fixed shape; a document does not |
| `optional(T)` | **no** | absence is not a JSON value; a document's `null` is |

`i64 → json` is refused rather than quietly widened to `f64`, because a 64-bit
key or identifier does not survive the trip and finding that out at runtime is
worse than being asked. `cast(x, json)` does it when that is what you meant.

Nothing is assignable **out** of `json` — a document need not hold the shape
asked of it, so every extraction is fallible and goes through `cast` or a
runtime filter.

## Runtime filters

Where `S` is not assignable to `T` but a check on the value could settle it, the
body assertion `v :: T` inserts that check instead of rejecting the program. A
row whose value does not match is **discarded**, and `v` is narrowed to `T` for
the rest of the rule.

| `S` | `:: T` | drops the row when |
|---|---|---|
| `optional(T)` | `T` | the value is absent |
| `optional(A)` | `optional(B)` | present and not a `B` |
| `json` | `T` | the document does not hold a `T` |
| `array(A)` | `array(B)` | any element is not a `B` |
| `dict(K,A)` | `dict(K,B)` | any value is not a `B` |

`optional(T) :: T` is the common one — the way to drop absent rows before a join
when you do not want absence to participate.

```grasp
named(name: n) <-
    person(name: n)      # n : optional(string)
    n :: string          # drop the rows with no name; n : string after
```

A filter is only inserted where the table above applies. Where no check could
help — `i64 :: string` — it is a compile error, because a filter that can never
pass is a silently empty relation.

> ``no `i64` value is a `string`: this assertion would discard every row``

## Comparison and ordering

Equality (`=`, `!=`) is defined on every type, structurally.

Ordering (`<`, `<=`, `>`, `>=`) is defined on **scalars only**. Comparing
records, arrays, dicts or documents is an error: an order over them would have
to be invented, and any invention would be arbitrary in a way that silently
decides `min` and `max`.

> ``ordering is not defined on `record(...)`; compare its fields``

`NONE` may be compared for equality with anything — `n != NONE` is how a program
asks whether a value is present — but not ordered.

## Diagnostics

| rejection | when |
|---|---|
| ``X is not assignable to Y`` | the tables above have no rule |
| ``missing field `f` `` | a record type lacks a field the target names |
| ``a dict key must be a scalar`` | `dict(K,V)` with composite `K` |
| ```optional` does not nest`` | `optional(optional(T))` written |
| ``ordering is not defined on X`` | `<` and friends on a composite |
| ``this assertion would discard every row`` | `v :: T` where no value of `S` can be a `T` |
| ``cannot infer a type for `NONE` `` | `NONE` with no context to take one from |
