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
not.

An external relation becomes an `input`:

```grasp
edge :: relation(src: i64, dst: i64)
edge(src:, dst:) <- input
```

```
edge :: zset(record(src: i64, dst: i64))
edge := input("edge")
```

The typespec must be written, not inferred: grasp-dbsp takes an `input` node's
schema from its `::` annotation, and that is also why the input cannot be
inlined into another call.

## Facts

The facts of one relation collect into a single `constant`:

```grasp
edge :: relation(src: i64, dst: i64)
edge(src: 1, dst: 2)
edge(src: 2, dst: 3)
```

```
edge :: zset(record(src: i64, dst: i64))
edge := constant([record(src: 1, dst: 2), record(src: 2, dst: 3)])
```

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
| `[a, b]` | `[a, b]` |
| `{k => v}` | `{k => v}` |
| `record(f: e)` | `record(f: e)` |
| `NONE` | `NONE` |

The three literal forms are identical in both languages, which is not an
accident — they were named that way so an expression survives lowering as
itself, and only `=` needs rewriting.

grasp's second dict spelling, `{a: v}`, does not appear here because it is gone
by this point: [desugaring](semantics.md#desugaring) rewrites it to
`{"a" => v}`. Both grasp spellings therefore arrive as the one grasp-dbsp form,
which is the point of putting the sugar on the grasp side — grasp-dbsp keeps
*exactly one way to write each thing*, and grasp is what people write.

Builtins pass through under the same names — `abs`, `floor`, `ceil`, `round`,
`length`, `concat`, `lower`, `upper`, `trim`, `coalesce`, `if`, `get`, `keys`,
`entries` — which is why [`semantics.md`](semantics.md#builtins) offers exactly
that set and no more.

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
| `{a: x} := d` destructure | `get(d, "a")` per key, guarded by `length(d) = N` |
| `(k, v) := **d` unnest | `flat_map` over `entries(d)` |

`entries(d)` yields `array(record(key: K, value: V))`, sorted by key, which is
what makes the unnest deterministic. Its inverse is the other form of the dict
literal, `dict(a)`, for building a dict whose size follows the data.

`{k: v, **rest} := d` has no lowering yet: subtracting the named keys needs a
`without_keys` builtin grasp-dbsp does not have. See
[`overview.md`](overview.md#future-work).

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
| `(v) := *arr` | `flat_map` | `flat_map(s, function((row) -> row.arr))` |
| `(k, v) := **d` | `flat_map` | `flat_map(s, function((row) -> entries(row.d)))` |
| `v :: T` | `filter` | `filter` on the runtime check |
| the head | `map` | `map(s, function((row) -> record(…)))` |

Nothing in that table is a special case: each is the operator its DAG node named,
which is why the node vocabulary was renamed to grasp-dbsp's in the first place.

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
by_dept := map_index(emp, function((r) -> record(key: r.dept, value: record(sal: r.sal))))
totals  := aggregate(by_dept, sum, function((v) -> v.sal))
payroll := map(totals, function((k, total) -> record(dept: k, total: total)))
```

grasp's five aggregators — `sum`, `count`, `min`, `max`, `avg` — are
grasp-dbsp's five, under the same names.

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
edge :: zset(record(src: i64, dst: i64))
edge := input("edge")

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
