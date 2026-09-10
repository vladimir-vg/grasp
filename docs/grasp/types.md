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
| `date` | a calendar date — no time, no zone |
| `time` | a time of day — no date, no zone |
| `timestamp` | an instant, always UTC |
| `interval` | a span of time, in microseconds |

A **type variable** — `T`, `K`, `V` — is not in this table, being not a type but
a stand-in for one. It is legal in a
[function typespec](syntax.md#typespecs) and nowhere else: no value ever has
that type, and nothing here — identity, assignability, narrowing — has anything
to say about one.

That is the whole vocabulary, and it is exactly what
[grasp-dbsp](../grasp-dbsp/language.md#value-types) can carry. The wider set the
Erlang implementation offers — `dynamic`, the narrower integers, `f32`,
arbitrary-precision `integer` and `numeric`, `bytes`, `bits`, the temporal
types, general `enum` — is listed under
[future work](overview.md#future-work), each blocked on a grasp-dbsp value type.

**Scalars** are `boolean`, `i64`, `f64`, `string`, `date`, `time`, `timestamp`
and `interval`. The word matters in two rules below: what may key a dict, and what
may be compared.

The **temporal** three are scalars because each orders as the instant it names,
and that one fact is what makes all four uses of the word work at once —
ordering, `min`/`max`, keying a dict, and `dict:from_entries`. They are **not
numbers**: `d + 1` is the error `"x" + 1` already is. What they take instead is
an `interval`, under [Arithmetic](#arithmetic) below.

An **`interval`** is a span of time, in microseconds. One integer, because with
no timezone a day is exactly 86400 seconds and every unit below a month converts
exactly — so `temporal:interval(days: 1)` and `temporal:interval(hours: 24)` are
one value. That is also why it is a scalar where a type carrying months beside
them could not have been: 1 month against 31 days has no answer, and an order
over it would have to be invented.

Months are the one quantity that does not convert, and grasp does not measure
them: a month has no length until it lands on a calendar, so it belongs to
neither this type nor any other here. `1 month = 30 days` is not a rule grasp
invents to have one. **There is no month arithmetic**, and a program that wants
"the same day next month" does not have it.

**A `timestamp` is always UTC, and there are no timezones in grasp.** That is a
decision and not a gap, and two things rest on it. A day is exactly 86400
seconds, which is why an `interval` can be one integer rather than a day
component beside a time one. And an instant is one number, so it orders, keys a
dict and compares structurally like every other value here — a zone-carrying
instant would be the first type in the language where two equal values were not
identical, or two identical ones did not sort together.

A program that needs a local reading carries the zone as data, in a column, and
converts where it displays. That is the Datalog answer: a zone is something a
row *has*, not something a type hides.

Values are built with `temporal:`, never with a literal — grasp has no temporal
literal syntax, and the library is where a value comes from.

## Relation types

```
relation(col: T, …)
```

A relation is a set of tuples with named, typed columns. It is a different kind
of thing from a value type: relations are what rules define, and a relation
cannot appear inside a value. There is no `array(relation(...))`.

A `:: relation(...)` spec is required for a relation declared `<- input`, whose
columns nothing else can determine, and optional elsewhere.

"Optional" means the rules defining a relation can give it a type, not that a
relation may go undeclared. **Every relation a program mentions must have a spec
or a definition** — a rule with it as head, a fact, or `<- input`. A relation
that appears only in a body is one nothing gives a type to, and
[`inference.md`](inference.md#diagnostics) reports it rather than treating it as
empty, because a mistyped name is far more likely than a deliberately empty
relation.

> ``relation `r` is not defined and has no typespec``

### A relation with no columns

`relation()` has no columns, so it has exactly one possible tuple — the empty
one. Being a set, it holds that tuple or it holds nothing: it is a
**proposition**, true or false, and `not enabled()` is how one is usually read.

```grasp
ready :: relation()
ready() <- config(mode: "on")
```

A proposition cannot be declared `<- input`. A relation is a set, and pushing
rows into one with no columns could only ever count them — every row is the same
tuple — which is not what `relation()` means. Derive it from a rule instead, over
a relation that does have columns.

> ``relation `ready` has no columns and cannot be an input``

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
that would change the value is a builtin, written out — and grasp has no general
conversion, so a change of type that no builtin performs is one the language
cannot express yet.

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
worse than being asked. There is no conversion that does it on purpose either —
grasp-dbsp has one and grasp has not been given it, which is the sort of thing
[`overview.md`](overview.md#future-work) collects rather than the sort inference
can settle.

Nothing is assignable **out** of `json` — a document need not hold the shape
asked of it, so every extraction is fallible and goes through a
[runtime filter](#runtime-filters), which drops the rows whose document did not
hold what was asked.

## Runtime filters

Where `S` is not assignable to `T` but a check on the value could settle it, the
body assertion `v :: T` inserts that check instead of rejecting the program. A
row whose value does not match is **discarded**, and `v` is a `T` for the whole
rule — a body is a set, so an assertion is a claim about the rule rather than
about what follows it, and writing it above or below what it narrows is the
same program.

| `S` | `:: T` | drops the row when |
|---|---|---|
| `optional(T)` | `T` | the value is absent |
| `optional(A)` | `optional(B)` | present and not a `B` |
| `json` | `T` | the document does not hold a `T` |
| `json` | `date` / `time` / `timestamp` / `interval` | the document does not hold one, written |
| `array(A)` | `array(B)` | any element is not a `B` |
| `dict(K,A)` | `dict(K,B)` | any value is not a `B` |

`optional(T) :: T` is the common one — the way to drop absent rows before a join
when you do not want absence to participate.

`optional(A) :: optional(B)` is the one that does not: the wrapper survives, so
an absent value comes through absent and only a *present* one that fails is
dropped. It is the check you want where absence is a legitimate answer and a
malformed value is not.

The last three rows all check **under** something, and each admits only a
whole-value check beneath it. `array(json) :: array(i64)` works and
`array(array(json)) :: array(array(i64))` does not, because the second is two of
these composed and the compiler writes one level. It says so rather than
rejecting the program.

A container check counts the parts that would survive against the parts there
are, so an **empty** array or dict passes: there is no element to fail. And the
row is dropped whole — a narrowing is not a filter over the elements.

```grasp
named(name: n) <-
    person(name: n)      # n : optional(string)
    n :: string          # drop the rows with no name; n : string after
```

A filter is only inserted where the table above applies. Where no check could
help — `i64 :: string` — it is a compile error, because a filter that can never
pass is a silently empty relation.

> ``no `i64` value is a `string`: this assertion would discard every row``

## Arithmetic

Two halves: numbers, and temporal values.

### Numbers

`+`, `-` and `*` take two operands of one numeric type and give that type.
There is no implicit conversion, so `i64` and `f64` never meet: mixing them is
an error naming both, and a literal beside a typed operand takes that operand's
type.

`/` and `%` yield that numeric type too. A zero divisor has no answer, and a
rule derives no row where a step of its body has none — so the rows with a zero
divisor are simply not there, and `q` is an ordinary `i64`:

```grasp
q := a / b          # i64; rows where b is zero are not derived
```

[`semantics.md`](semantics.md#a-body-must-have-an-answer) has the rule this is
an instance of. grasp-dbsp types division `optional(T)` instead, because a
compilation target says what can be missing rather than dropping it;
[`mapping.md`](mapping.md#narrowing-and-dropping) is where the two meet.

### Temporal values

`+` and `-` also take a temporal value and a span, and every case is **total** —
there is no dropped row anywhere in this table.

| | result | |
|---|---|---|
| `date - date`, `time - time`, `timestamp - timestamp` | `interval` | how far apart |
| `timestamp ± interval` | `timestamp` | exact |
| `time ± interval` | `time` | wraps at midnight |
| `date ± interval` | `date` | **truncates to whole days** |
| `interval ± interval` | `interval` | exact |

`interval + x` reads as well as `x + interval` and means the same thing.
Subtraction does not commute: a moment less a duration is a moment, and a
duration less a moment is nothing, so only the first order is a program.

`*`, `/` and `%` stay numbers-only. Scaling a span — `iv * 3` — is
[future work](overview.md#future-work).

**A date has no sub-day resolution**, so a shift truncates toward zero: thirty
hours moves it a day, one hour leaves it alone, and so does minus one hour. That
is the one lossy rule in the language, and it is what keeps every case here
total. Two consequences follow, and they are worth knowing rather than meeting:

```grasp
half := temporal:interval(hours: 12)
(d + half) - half        # d — but only because both truncated to nothing
(d + half) + half        # d, while d + temporal:interval(days: 1) is the next day
```

It is **not invertible**, and **not associative over addition**. Neither holds
for `timestamp`, which has the resolution to be exact.

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
| ``ordering is not defined on X`` | `<` and friends on a composite, or `min`/`max` folding one |
| ``X needs a numeric argument`` | `sum` or `avg` folding something that is not a number |
| ``this assertion would discard every row`` | `v :: T` where no value of `S` can be a `T` |
| ``cannot infer a type for `NONE` `` | `NONE` with no context to take one from |
