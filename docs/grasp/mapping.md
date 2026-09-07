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
| `dict(K,V)` | — | **no target yet** |

`dict(K,V)` is specified in [`language.md`](language.md#value-types) but cannot
be emitted: grasp-dbsp has no dict type. It is listed under future work there.
Until it lands, a program using a dict is rejected by grasp with a diagnostic
saying so, rather than emitting something that will not compile.

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

Builtins pass through under the same names — `abs`, `floor`, `ceil`, `round`,
`length`, `concat`, `lower`, `upper`, `trim`, `coalesce`, `if`, `get`, `keys` —
which is why [`language.md`](language.md#builtins) offers exactly that set and no
more.

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

The transitive closure from [`language.md`](language.md#example):

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
