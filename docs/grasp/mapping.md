# grasp — Mapping onto grasp-dbsp

How a checked grasp program becomes a
[grasp-dbsp](../grasp-dbsp/language.md) program. This is the last stage of the
pipeline in [`compilation.md`](compilation.md), and the only one that knows what
the backend is.

Everything here is emission. grasp-dbsp then runs its own pipeline — parse,
typecheck, content-address, lower onto `dbsp` — described in
[`../grasp-dbsp/mapping.md`](../grasp-dbsp/mapping.md). grasp does not reach past
the text it emits.

## Relations

A grasp relation is a flat stream of records:

```
relation(col₁: T₁, …)   →   zset(record(col₁: T₁, …))
```

Indexed streams — `indexed_zset(K,V)` — never correspond to a grasp relation.
They exist only inside a rule, between a `map_index` and the join or aggregate
that consumes it, and every rule ends by flattening back to a `zset`.

A relation with no columns falls out of the same rule: `relation()` is
`zset(record())`, whose one possible row is the empty record. grasp-dbsp gives
that row a `distinct` like any other relation's, so the stream holds the empty
tuple or nothing, which is what a
[proposition](../grasp/types.md#a-relation-with-no-columns) means.

**Two naming rules everything below depends on.** The node named `r` in the
emitted program *is* grasp relation `r`'s stream, and an external relation's
`input(...)` table string is the relation name. Both are visible in every
example here; they are stated because a reader has to be able to rely on them —
anything driving a compiled program keys on them.

They have a consequence for everything else the emitter names. **An intermediate
node may not take a relation's name.** Most intermediates need no name at all —
grasp-dbsp operator calls nest, and a stream used once can simply be written
where it is used — but a few must have one: a stream used twice, and anything
`input` or `constant`, neither of which nests. Those are named after the relation
whose rule they serve and made unique against the program's relation names.
Which suffix is used is the emitter's business; that no intermediate collides is
not — though it must be a function of the *plan*, not of source order, or two
programs that mean the same thing would emit different text.

**A relation whose name grasp-dbsp cannot spell is mangled.** grasp admits a
namespace-qualified relation name — `mine:edges` — and permits names grasp-dbsp
reserves, like `map` or `join`; grasp-dbsp identifiers are
`[A-Za-z_][A-Za-z0-9_]*` and its reserved words are its own. So the node name is
derived from the relation name rather than always equal to it: `:` becomes `__`,
a reserved word takes a suffix, and the result is made unique against every other
node name the same way an intermediate is. Only the *node* name moves. The
`input(...)` table string is quoted and stays the relation name exactly, which is
what anything driving a compiled program keys on.

An external relation becomes an `input`:

```grasp
edge :: relation(src: i64, dst: i64)
edge(src:, dst:) <- input
```

```
edge_1 :: zset(record(src: i64, dst: i64))
edge_1 := input("edge")
edge   := distinct(edge_1)
```

The typespec must be written, not inferred: grasp-dbsp takes an `input` node's
schema from its `::` annotation, and that is also why the input cannot be
inlined into another call.

**The `distinct` is what makes an input relation a set.** The outside world
writes a Z-set — a row arrives with a weight, and weights accumulate across
transactions — while grasp says a relation is the rows whose accumulated weight
is positive ([`semantics.md`](semantics.md#time)). So the *raw* stream takes an
intermediate name and the relation keeps its own; the naming rule above is
unchanged, but it is the `input` node that moves rather than the relation.

It has a price worth stating: `distinct` is not linear, so every input relation
now carries an integral of its own. An input feeding only linear operators used
to be stateless and is not any more.

A relation with a typespec and **no producer** — no rule, no fact, no `<- input`
— is empty for the life of the program, and emits as a standalone `empty()`:

```grasp
s :: relation(x: i64)
```

```
s :: zset(record(x: i64))
s := empty()
```

A node still has to exist, because the naming rule above says the node named `s`
*is* relation `s`'s stream and a rule body may reference it. The typespec is
required rather than tidy: `empty()` takes its type from where it sits, and
standing alone the only thing that can supply one is the `::`. `constant([])` is
not the alternative — grasp-dbsp rejects it, since there is one way to write
each thing.

## Facts

The facts of one relation collect into a single `constant`:

```grasp
edge :: relation(src: i64, dst: i64)
edge(src: 1, dst: 2)
edge(src: 2, dst: 3)
```

```
edge_1 :: zset(record(src: i64, dst: i64))
edge_1 := constant([record(src: 1, dst: 2), record(src: 2, dst: 3)])
edge   := distinct(edge_1)
```

**The `distinct` is not decoration.** `constant` consolidates identical rows by
*summing* their weights, so a fact written twice — or written once, and once
again as a closed expression that evaluates the same — arrives at weight 2. A
relation is a set and facts are program text, so asserting a member twice is
asserting it once, and `distinct` is what says so. The `constant` therefore
takes an intermediate name whether or not any rule also defines the relation,
which is the same shape either way.

A fact's arguments are closed expressions — [`semantics.md`](semantics.md#facts)
says why they must be — so they translate across unchanged and grasp-dbsp
evaluates them when the program is checked. Nothing here needs the compiler to
be able to *evaluate* grasp.

`constant` delivers its rows in the first transaction and is zero afterwards, so
the relation is those rows at every transaction. That is the same shape as an
`input` relation whose rows were pushed once, which is why nothing downstream of
a fact relation has to know it was one.

A **rule with no positive atom** — a body of nothing but matches — is grounded on
the unit relation, which is a `constant` holding the one empty row and is then
its join graph's root:

```grasp
answer :: relation(v: i64)

answer(v: n) <-
    n := 6 * 7
```

```
answer_g :: zset(record())
answer_g := constant([record()])
answer_1 := map(answer_g, function((r) -> record(n: 6 * 7)))
answer   := distinct(map(answer_1, function((r) -> record(v: r.n))))
```

The grounding node has to be named, because `constant` does not nest — so it is
named after the relation it serves and made unique, per the rule
[above](#relations). Note that `6 * 7` crosses unevaluated: emission translates
expressions, it does not fold them, and grasp-dbsp evaluates a closed expression
in a `constant` or a `map` alike.

The facts of one relation could be emitted the same way — one grounded rule
each, summed — and the single `constant` above is an emission choice over that,
building the same relation with fewer nodes. Both are correct; they are not the
same *text*, so nothing should assert that a fact and its rule form emit
identically.

## Types

Every grasp type in this cut maps to exactly one grasp-dbsp type, with the same
name. That is not a coincidence — it is why the names were chosen.

| grasp | grasp-dbsp | note |
|---|---|---|
| `boolean` | `bool` | |
| `i64` | `i64` | |
| `f64` | `f64` | |
| `string` | `string` | |
| `optional(T)` | `optional(T)` | neither nests |
| `record(f: T, …)` | `record(f: T, …)` | field order is not part of identity in either |
| `array(T)` | `array(T)` | |
| `json` | `json` | |
| `dict(K,V)` | `dict(K,V)` | `K` is a scalar in both |

The types grasp does not have yet — `dynamic`, the narrower integers, `f32`,
`numeric`, `bytes`, `bits`, the temporal types, general `enum` — are absent for
the same reason: there is nothing underneath them. Each arrives here when it
arrives there. See [`overview.md`](overview.md#future-work).

## Expressions

Rule-body expressions become grasp-dbsp function bodies. The operators
correspond directly, with one spelling difference:

| grasp | grasp-dbsp |
|---|---|
| `+`, `-`, `*`, `/`, `%` | same |
| `<`, `<=`, `>`, `>=`, `!=` | same |
| `=` | `==` |
| `and`, `or`, `not` | same |
| `++` | `concat(a, b)` |
| `s.field` | `s.field` |
| `record:get(s, "f")` | `s.f` |
| `dict:get(d, k)` | `get(d, k)` |
| `boolean:not(e)` | `not e` |
| `[a, b]` | `[a, b]` |
| `{k => v}` | `{k => v}` |
| `record(f: e)` | `record(f: e)` |
| `NONE` | `NONE` |

The three literal forms are identical in both languages, which is not an
accident — they were named that way so an expression survives lowering as
itself, and only `=` needs rewriting.

The three middle rows are **desugaring run backwards**. grasp-dbsp has no
`record:get`, no `dict:get` and no `boolean:not` — those names exist only in
grasp's reserved namespaces, as the expansions
[`semantics.md`](semantics.md#desugaring) gives `s.f`, a dict pattern and `not`.
So the emitter puts back what desugaring took apart. A program that spells one
by hand is writing the expansion itself and reaches the same text, which is what
lets a sugar and its expansion be asserted equivalent.

grasp's second dict spelling, `{a: v}`, does not appear here because it is gone
by this point: [desugaring](semantics.md#desugaring) rewrites it to
`{"a" => v}`. Both grasp spellings therefore arrive as the one grasp-dbsp form,
which is the point of putting the sugar on the grasp side — grasp-dbsp keeps
*exactly one way to write each thing*, and grasp is what people write.

Builtins pass through under the same names — `abs`, `floor`, `ceil`, `round`,
`length`, `concat`, `lower`, `upper`, `trim`, `if`, `keys`, `entries`.

grasp-dbsp has three more that [`semantics.md`](semantics.md#builtins) does not
offer — `get`, `cast` and `coalesce` — and grasp emits all three without
providing any of them: `dict:get` for a dict pattern, and `cast` and `coalesce`
in the [narrowing](#narrowing-and-dropping) a runtime filter expands into. A
builtin the compiler writes is not a builtin the language has.

## Computation DAG nodes

Each node of a rule's computation DAG becomes one operator call.

| DAG node | grasp-dbsp |
|---|---|
| `map_index(atom, key, val)` | `map_index(s, function((r) -> record(key: …, value: …)))` |
| `join(L, R, on)` | `join(l, r, function((k, a, b) -> …))` |
| `antijoin(L, R, on)` | `antijoin(l, r)` |
| `filter(pred, over)` | `filter(s, function((r) -> …))` |
| `map(f, in, out)` | `map(s, function((r) -> …))` |
| `flat_map(f, in, out)` | `flat_map(s, function((r) -> …))` |
| `aggregate(agg, col, group, out)` | `aggregate(s, agg, function((v) -> …))` |

Two shape rules govern the surrounding code:

- **`join`, `join_index` and `antijoin` need both sides indexed** on the same key
  type. That is what the entry `map_index` nodes are for.
- **`antijoin` returns an indexed stream**, and takes no function, so a `map`
  follows it to project and flatten.

**Fusing the index into the join.** A `join` whose result is immediately
re-indexed for another join emits as one `join_index` rather than a `join`
followed by a `map_index`. Same for `flat_map` into `flat_map_index`. This is an
emission choice, not a DAG node kind — [`compilation.md`](compilation.md) keeps
the DAG in terms of `join` and `map_index` alone.

## Dicts

A dict literal survives lowering **unchanged**: grasp and grasp-dbsp spell it
the same, `{k => v}`, and the entry sorting and deduplication both languages
promise is one mechanism rather than two.

| grasp | grasp-dbsp |
|---|---|
| `{k => v, …}` | `{k => v, …}` — identical |
| `d[k]` lookup | `get(d, k)` → `optional(V)` |
| `keys(d)` | `keys(d)` → `array(K)` |
| `length(d)` | `length(d)` |
| `{a:} := d` destructure | `get(d, "a")` per key, guarded by `length(d) = N` |
| `{a:, **e} := d` remainder | `dict(filter_array(entries(d), function((e) -> e.key != "a")))` |
| `[x, y] := arr` destructure | `get(arr, 0)` per position, guarded by `length(arr) = N` |
| `[x, *r] := arr` remainder | `filter_array(arr, function((e, i) -> i >= N))` |
| `(k, v) := **d` unnest | `flat_map` over `entries(d)` |

`entries(d)` yields `array(record(key: K, value: V))`, sorted by key, which is
what makes the unnest deterministic. Its inverse is the other form of the dict
literal, `dict(a)`, for building a dict whose size follows the data.

`{k:, **rest} := d` subtracts the named keys, which is a `filter_array` over the
dict's entries rebuilt with `dict`. The pattern's keys are literal, so the
predicate is a conjunction the emitter writes out and no membership builtin is
needed. A **record** remainder lowers differently, its fields being known before
the program runs: `record(a:, **e) := r` builds `e` as a `record(…)` literal of
`get`s over the fields the pattern did not name, which is why the two spellings
are not one feature.

## Body statements, end to end

Every form a rule body can take, and the grasp-dbsp it becomes. The middle
column is the computation DAG node
[`compilation.md`](compilation.md#what-each-body-statement-becomes) produces; the
right is what this pass emits for it.

| grasp | DAG node | grasp-dbsp |
|---|---|---|
| `r(col: x)` as a leaf | `map_index` | `map_index(r, function((row) -> record(key: …, value: …)))` |
| `r(col: x)` joined | `join` | `join(l, r, function((k, a, b) -> …))` |
| `not r(col: x)` | `antijoin` | `antijoin(l, r)` then `map` to flatten |
| `a > 20` | `filter` | `filter(s, function((row) -> row.a > 20))` |
| `v := e` | `map` | `map(s, function((row) -> record(…, v: e)))` |
| `s := sum<r>` | `aggregate` | `map_index`, `aggregate(s, sum, f)`, `map` |
| `(v) := *arr` | `flat_map` | `flat_map(s, function((row) -> map_array(row.arr, …)))` |
| `(k, v) := **d` | `flat_map` | `flat_map(s, function((row) -> map_array(entries(row.d), …)))` |
| `v :: T` | `filter` | `filter` on the runtime check |
| the head | `map` | `map(s, function((row) -> record(…)))` |

Nothing in that table is a special case: each is the operator its DAG node named,
which is why the node vocabulary was renamed to grasp-dbsp's in the first place.

**An unnest carries the rest of the row, and `map_array` is what carries it.**
`flat_map` emits one row per element of the array its function returns, so the
row it emits *is* an element, and everything else the rule had bound would be
lost. `map_array` builds the rows to fan out to instead:

```grasp
tagged(name: n, tag: k) <-
    person(name: n, tags: d)
    (k, _v) := **d
```

```
tagged := flat_map(rows, function((row) ->
    map_array(entries(row.d), function((e) -> record(n: row.n, k: e.key)))))
```

Both binders are in scope: `n` comes off the row and `k` off the element. A dict
goes through `entries`, which already yields `array(record(key, value))`, so the
two kinds of unnest take one shape and differ only in what an element's parts
are called. This is the one thing in grasp that grasp-dbsp could not express
until it had [`map_array`](../grasp-dbsp/language.md#map_array-and-filter_array).

### Narrowing and dropping

**A narrowing filter is three operators, not one.** grasp-dbsp's typing is not
flow-sensitive and its `filter` cannot change a row type, so `v :: T` emits as:

1. a `map` binding `v` at the `optional(T)` the check produces — the
   expression's own type for an `optional(T) :: T`, `cast(v, optional(T))` for a
   `json`, `array` or `dict` one;
2. `filter(s, function((r) -> r.v != NONE))`, dropping the rows that failed;
3. a `map` rebinding the column as `coalesce(r.v, d)` for any definite `d : T`.

`coalesce` is grasp-dbsp's **only** narrowing from `optional(T)` to `T` — `cast`
refuses an optional source and says so, and `if` widens its arms rather than
narrowing them. The default `d` is unreachable: the filter has already dropped
every row that could take it. It is written because grasp-dbsp's checker cannot
see that, not because a value was chosen — so an emitter needs some definite
value of every type, which is `0`, `0.0`, `""`, `false`, `cast([], array(T))`,
`cast({}, dict(K,V))`, or a record built from those.

**A narrowing that keeps its wrapper is two operators, and different in kind.**
`optional(A) :: optional(B)` leaves an `optional(B)`, so there is nothing to
`coalesce` to: absence is one of the answers rather than one of the rows to drop.
That also rules out `r.v != NONE` as the test, because a `cast` maps absence and
a failed extraction alike to `NONE` and only the first is to be kept. So the
filter reads the value on both sides of the conversion:

1. `filter(s, function((r) -> (r.v == NONE or cast(r.v, optional(B)) != NONE)))`;
2. a `map` rebinding the column as `cast(r.v, optional(B))`.

The conversion is written twice rather than bound to a field first — the filter
needs the value before it as well as after, and a bound field would need a name
invented against the row's schema. Every expression in grasp-dbsp is total, so
the second evaluation is a cost and never a difference.

**A narrowing over a container tests every part, and rebuilds it.**
`array(A) :: array(B)` and `dict(K,A) :: dict(K,B)` are two operators again, and
neither touches the value as a whole:

1. `filter(s, function((r) -> length(filter_array(r.v, function((e) -> cast(e, optional(B)) != NONE))) == length(r.v)))`;
2. a `map` rebinding the column as `map_array(r.v, function((e) -> coalesce(cast(e, optional(B)), d)))`.

Counting the survivors against the elements is what "any element is not a `B`"
means, and it is why an empty array passes rather than failing for having
nothing. A dict goes through `entries` and back through `dict`, its keys
untouched; that is the only difference between the two rows.

**What is still missing is the two composed.**
`optional(array(A)) :: optional(array(B))` is a check under a wrapper under a
wrapper, and each of the three "under" rows admits only a whole-value check
beneath it. The compiler says `not implemented: narrowing under two wrappers`
rather than emitting the one-level form, which would try to convert a whole
`array(json)` and be refused by grasp-dbsp.

**Division needs no rule of its own.** `/` and `%` are `optional(T)` in
grasp-dbsp and `T` in grasp, so every `q := a / b` carries an implicit `q :: T`
and emits by exactly the rule above.

Testing the divisor instead — `filter(s, function((r) -> r.b != 0))` before the
division — would be the wrong shape twice over. It does not avoid the
`coalesce`, because the typing is not flow-sensitive and `/` is optional
whatever guards it; and it tests a proxy for what grasp actually said, which is
that the *division* has no answer. That proxy happens to be exact today, but it
is a fact about grasp-dbsp's `/` that grasp's emitter would then depend on, and
it does not generalise to the rest of the table — a `json :: T` has no divisor
to test.

When the narrowing is the last thing before the head, the third `map` folds into
the head's projection, the same fusion the join and index calls get above.

**A cross product is a join on the unit key.** When the optimizer has to combine
two components that share no variable
([`compilation.md`](compilation.md#join-spanning-forest)), both sides are indexed
on `record()` — one key, so every row meets every row — and joined:

```grasp
pair(x: x, y: y) <-
    a(x: x)
    b(y: y)
```

```
pair_a := map_index(a, function((r) -> record(key: record(), value: r)))
pair_b := map_index(b, function((r) -> record(key: record(), value: r)))
pair   := distinct(join(pair_a, pair_b, function((k, l, r) -> record(x: l.x, y: r.y))))
```

There is no `cross` operator and there does not need to be one. A guard reads the
same way: an atom over a relation with no columns is a component whose rows are
already `record()`, so joining it costs an index and nothing else.

## Rules and unions

Several rules for one relation are summed and then deduplicated:

```
r := distinct(plus(rule₁, rule₂))
```

The `distinct` is **required, not tidiness**. grasp-dbsp's `plus` adds weights —
it is bag union, so a tuple derived by both rules would come out with weight 2.
Datalog relations are sets, and `distinct` is what makes them so.

For more than two rules, `sum(rule₁, rule₂, …)` is n-ary and saves the nesting.

**`distinct` wraps whatever gives a relation its rows.** Facts, rules, or both —
a relation defined by facts *and* rules is one `distinct` over the sum of the two
([above](#facts)) — and an [`input`](#external-relations), where the weights come
from outside. The one exception is a recursive stream, which grasp-dbsp
deduplicates every round already ([below](#recursion)).

## Recursion

One strongly connected component becomes one `circuit` definition instantiated
by one `fixpoint`. The component's relations are the recursive parameters; the
relations it reads from lower strata are ordinary parameters.

Three rules govern emission, and all three are easy to get wrong:

**1. Always write the recursive stream's typespec.** grasp-dbsp can sometimes
infer a recursive stream's type, but only through operators whose result type is
one of their operands. A relation defined as `plus` of two rule outputs, where
either output ends in a `map`, is not inferable — and that is the ordinary shape
of a Datalog rule. The emitter knows the relation's type from typechecking, so it
writes it:

```
path :: zset(record(src: i64, dst: i64))
```

Not writing it produces a compile error from grasp-dbsp, not a wrong answer. But
it is a needless one, and the compiler always has the type to hand.

**2. Do not emit `distinct` on a recursive stream.** grasp-dbsp applies it to
every recursive stream on every round — that is what makes the iteration
terminate. A hand-written one lowers a redundant second `distinct`. So the
union-of-rules rule above has an exception: inside a fixpoint body, a recursive
relation is `plus(rule₁, rule₂)` with no `distinct`.

**3. Recursive streams start empty.** The call site passes `empty()`; the base
case belongs in the body, as one of the summed rules.

A fact-defined relation that a recursive component reads is the one thing that
cannot move into the body: grasp-dbsp rejects `constant` inside a `fixpoint`,
where a source would fire once per iteration rather than once per transaction.
Emit it outside and pass it in as an ordinary parameter — which is exactly what
an input relation already does, so this needs no special case.

Only recursive members leave the fixpoint, read off as `fp.name`.

The constraints on what may share a fixpoint — sibling fixpoints, differing
column types, batch shape, no nesting — are in
[`compilation.md`](compilation.md#fixpoint-shape).

## Stratified negation

A negated atom becomes `antijoin`, which is why negation must be stratified: the
antijoin's right side has to be a finished relation, and a lower stratum is
exactly what guarantees that.

```grasp
leaf(name: n) <-
    node(name: n)
    not parent(child: n, parent: _)
```

```
n_idx := map_index(node,   function((r) -> record(key: r.name,  value: record(name: r.name))))
p_idx := map_index(parent, function((r) -> record(key: r.child, value: record(child: r.child))))
leaf  := distinct(map(antijoin(n_idx, p_idx), function((k, v) -> record(name: v.name))))
```

The `map` after the `antijoin` is not optional: `antijoin` yields an indexed
stream, and a relation is flat.

## Aggregation

Grouping is the index key, so an aggregate is always three operators: index by
the group, fold, flatten.

```grasp
payroll(dept: d, total: s) <-
    emp(dept: d, sal: r)
    s := sum<r>
```

```
rows    := distinct(map(emp, function((r) -> record(dept: r.dept, sal: r.sal))))
by_dept := map_index(rows, function((r) -> record(key: record(dept: r.dept), value: record(sal: r.sal))))
totals  := aggregate(by_dept, sum, function((v) -> v.sal))
payroll := distinct(map(totals, function((k, total) -> record(dept: k.dept, total: total))))
```

**The `distinct` before the index is not tidiness either**, and it is the reason
an aggregate is four operators rather than three. An aggregate is weight-scaled:
a row of weight `w` contributes `w` times. That is right — and it is right
*because* every other operator preserves the invariant it needs, that a row's
weight is the number of satisfying assignments it stands for. Two operators
establish that invariant only where the row is an injective encoding of the
assignment: an atom that omits a column produces one row per witness rather than
per assignment, and an unnest over equal elements produces several rows for one
binding. So the whole assignment is deduplicated before it is folded, which is
also why nothing is projected away before an aggregate.

The trailing `distinct` is the ordinary relation-level one, from
[above](#rules-and-unions).

**`count` is the one that needs translating.** grasp's counts the *assignments*
in the group; grasp-dbsp's counts the rows whose projection is not absent. So it
is emitted over a projection that is never absent — `function((v) -> 0)` — and
the two then mean the same thing. It is also why `count<>` takes no argument in
grasp: there is nothing for one to be.

The other four are grasp-dbsp's under the same names *and* the same types, `avg`
included. Each gives back its argument's type, absent exactly where the argument
could be — so nothing is narrowed on the way out, and a group whose values are
all absent reports in both languages rather than being dropped by one of them.

## Worked example

The transitive closure from [`semantics.md`](semantics.md#example):

```grasp
edge :: relation(src: i64, dst: i64)
edge(src:, dst:) <- input

path(src: x, dst: y) <- edge(src: x, dst: y)
path(src: x, dst: y) <-
    path(src: x, dst: z)
    edge(src: z, dst: y)
```

`edge` is an input table; `{path}` is one recursive component, so one `circuit`
and one `fixpoint`:

```
edge_1 :: zset(record(src: i64, dst: i64))
edge_1 := input("edge")
edge   := distinct(edge_1)

circuit path_scc(edge: e, path: p) {
    path  :: zset(record(src: i64, dst: i64))
    r1    := map(e, function((r) -> record(src: r.src, dst: r.dst)))
    p_idx := map_index(p, function((r) -> record(key: r.dst, value: record(src: r.src))))
    e_idx := map_index(e, function((r) -> record(key: r.src, value: record(dst: r.dst))))
    r2    := join(p_idx, e_idx, function((k, a, b) -> record(src: a.src, dst: b.dst)))
    path  := plus(r1, r2)
}

fp   := fixpoint(path_scc(edge: edge, path: empty()))
path := fp.path
```

Reading it against the rules above: `path` carries its typespec (rule 1); the
two rule outputs are `plus`ed with no `distinct` (rule 2); the call site passes
`empty()` and the base case `r1` sits in the body (rule 3). `r1` is the
projection map for rule 1 — identity here, because the head columns are the
relation's columns. The two `map_index` calls key `path` by its far end and
`edge` by its near end, so a path and an outgoing edge meet on the shared node.

`edge` is passed once though two nodes read it: grasp-dbsp content-addresses
nodes, so the emitter never needs to arrange sharing itself.

Over edges `1→2→3→4` this yields exactly the six reachable pairs — `(1,2)`,
`(1,3)`, `(1,4)`, `(2,3)`, `(2,4)`, `(3,4)`, each with weight 1.

## Naming

grasp is ported from an Erlang implementation whose documents use different
names for several shared concepts. Where the two differ, grasp takes the
grasp-dbsp name, so that one concept has one spelling across the boundary.

| Erlang Grasp | grasp | why |
|---|---|---|
| `arrange(atom, key, val)` | `map_index` | the operator it becomes |
| `project(vars)` | `map` | grasp-dbsp has one operator for both |
| `map(K,V)` (type) | `dict(K,V)` | `map` is an operator name |
| `struct(...)` | `record(...)` | grasp-dbsp's type name |
| `struct:` namespace | `record:` | follows the type |
| `ABSENT` | `NONE` | grasp-dbsp's literal |
| `==` and `=` mixed | `=` | that corpus used both; this settles it |
| EDB / IDB | input table / derived table | that corpus had already moved |

## Where this dialect diverges

Names are the small half. These are the places grasp *means* something different
from the Erlang dialect it is ported from, each a decision rather than an
omission — what is merely missing is [future
work](overview.md#future-work) instead.

**Numbers do not widen.** That dialect defines a widening lattice, so an `i64`
and an `f64` meet at `f64` and an expression mixing them has an answer. Here
they do not meet in either direction: mixing them names both types and stops.
[`types.md`](types.md#arithmetic) states the rule; the reason to prefer it is
that a widening lattice decides silently, and the decision it makes — which
operand loses precision — is the one a reader most wants written down.

**Division is not `optional`.** There, `/` yields `optional(T)` and a zero
divisor is an absent value the program goes on to handle. Here a rule derives no
row where a step of its body has no answer, so the rows with a zero divisor are
simply not there and `a / b` is an ordinary `T`. grasp-dbsp agrees with that
dialect rather than with grasp, which is why this file has a
[rule for reconciling them](#narrowing-and-dropping) — and why `optional`
division briefly got into grasp by being copied from the target instead of
designed.

**`f64` arithmetic is arithmetic.** There, every `f64` operation produces a
`result_equivalent_closure` to cage the machine-dependence of IEEE arithmetic,
resolvable only through an explicit eval against a shared cache. That rests on
the storage service backing [reference
types](overview.md#future-work), which this workspace does not have — so `f64`
here is an ordinary value and an `f64` result is an ordinary result.

## What the backend removes

A reader coming from the Erlang design documents will find whole subsystems
missing here. Most are gone because grasp-dbsp and `dbsp` already do the job:

- **Recursion machinery.** The per-relation `Rec` processes, the per-SCC
  coordinator, barrier broadcast, iteration rounds and the epoch lifecycle are
  all one word: `fixpoint`.
- **The incrementalization pass.** Nothing inserts `integrate` and
  `differentiate` around non-linear operators; `dbsp` does that.
- **Arrange deduplication.** Content addressing makes it automatic — see
  [`compilation.md`](compilation.md#what-the-backend-already-does).
- **The process model.** One process per operator, credit-based backpressure,
  decentralized watermarks, the supervision tree. `dbsp` executes circuits.

The rest is missing because it is platform rather than language, and this
workspace has no platform: the record store, services, subcircuits and the
in/out-circuit split, checkpoints, commits, branchpoints, hot-reload, and both
provenance systems. [`overview.md`](overview.md#relationship-to-other-projects)
says what that implementation is and where it still lives.
