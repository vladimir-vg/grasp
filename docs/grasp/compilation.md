# grasp — Compilation

How a grasp program becomes a plan. This document covers everything up to the
point where grasp-dbsp is emitted; emission itself is
[`mapping.md`](mapping.md).

```
text → AST → core → typed core → join graph → computation DAG → SCC graph → grasp-dbsp
             desugar  infer       (per rule)     (per rule)      (whole program)
```

The middle three stages are where the interesting decisions are made, and none
of them knows what the backend is.

## Before the graph

Two passes run first, and neither is described here.

- **Desugaring** rewrites the surface forms that stand for others — `x:`, `.f`,
  `++`, and the destructure patterns with their size checks. See
  [`semantics.md`](semantics.md#desugaring). Everything below sees the smaller
  core.
- **Type inference** gives every variable and every relation column a type, and
  performs the safety check. See [`inference.md`](inference.md). Types are
  erased afterwards, so nothing below carries one.

## Join graph

One per rule. It represents the body **without any evaluation order** — that is
the optimizer's job, and keeping the two apart is what lets the optimizer
reorder freely.

### Atoms are nodes, not relations

The graph is built from **occurrences**. Each mention of a relation in the body
is its own node with its own identity and its own variable bindings. The
relation name is a property of the node, not the node itself.

```grasp
two_hop(src: x, dst: z) <-
    edge(src: x, dst: y)
    edge(src: y, dst: z)
```

Two nodes, both of `edge`. They share `y`, so there is an edge between them —
a self-join, needing no special case anywhere.

**Variables are not nodes.** They are the names on the wires: an edge between
two atom nodes exists exactly when they share a variable, and its weight is how
many they share.

### Node kinds

| kind | binds | consumes |
|---|---|---|
| positive atom | its columns' variables | its literals constrain |
| negated atom | nothing | every variable it mentions |
| filter | nothing | the variables in the predicate |
| match | its left-hand side | the variables in the expression |
| aggregate | its result variable | the aggregated expression's variables |

Only positive atoms and matches produce variables. Everything else is a
**dependent node**: it declares what it consumes and must be placed somewhere
those variables are in scope.

### Competing producers

A variable may have more than one producer:

```grasp
head(val: x) <-
    r1(a: a, b: b)
    x := a * 2
    x := b + 1
```

`x` is produced three ways. The join graph records all of them and chooses
none. The optimizer later designates one the primary computation and turns the
rest into equality constraints (`x = b + 1`) — which is what they mean.

### Dependency partial order

`N₁ < N₂` when `N₂` consumes a variable that only `N₁` produces. This is the
only ordering the join graph carries, and it is partial: nodes with no
dependency between them may be evaluated in either order.

A variable appearing in the head with no producer in the body is an unbound
variable — an error.

## Optimizer

Turns the unordered join graph into an ordered computation DAG.

### The weighted join graph

Take the subgraph induced by **positive atoms only**. Nodes are atom
occurrences; an edge joins two atoms that share at least one variable, weighted
by how many.

Atoms sharing no variable have no edge, so the graph may be **disconnected**. A
component is a set of atoms that can be joined; two components can only be
combined by a cross product, and the shape of the graph is what says so.

An atom over a relation with no columns binds no variable, so it is always a
component of its own. That is not a degenerate case to be tolerated — guarding a
rule with a proposition is what such a relation is for.

### Join spanning forest

A **join spanning tree** is a maximum-weight spanning tree of one component —
Prim's algorithm, always taking the heaviest edge from the visited set. For an
acyclic rule it coincides with a classical join tree; for a cyclic one it is the
best tree-shaped approximation, cutting the lightest edges to break cycles.

Nothing is lost by the cut. A join takes **every** variable its two sides share,
not only the ones on the tree edge that brought them together, so an equality
the tree could not carry is still enforced where the two atoms finally meet.
That is why a cyclic rule needs no repair pass: the tree decides the *order*,
and the join conditions follow from the schemas.

The optimizer tries **every atom in the component as root**. For each, a
post-order traversal of the rooted tree gives an evaluation order: children
before parents, each parent joining its children's results with its own stream.

The parent joins them **one at a time**, folding each child's result into what
it has so far, rather than holding all of its children and joining at the end.
Both evaluate the same tree; the fold is what keeps exactly one stream live at
every step, which is what the cost model below counts. So the order is a stack
machine: an atom pushes a stream, a join pops two and pushes one, and a
dependent node rewrites the top.
The component's own dependent nodes are placed into that order before it is
scored — a match produces a variable, so a component's peak is not its own
number until its matches are in it — and an order violating the dependency
partial order is discarded, which skips that rooting rather than rejecting the
rule.

Across components it is a **forest**: each is planned and scored on its own, and
the plans are then concatenated with consecutive components crossed.

**A cross product is not a choice the optimizer makes.** Two atoms sharing a
variable are in one component and will be joined on it; two sharing none cannot
be joined at all. A rule with `k` components therefore crosses `k − 1` times in
*every* candidate order. The old rule got its "no accidental cross product"
guarantee by leaving such a plan unbuildable; this is the stronger version of
the same guarantee, because the count is fixed by the rule rather than left to
the plan.

### Crossing components

Two components share no variable, so there is no key to join them on. They are
crossed instead, and a cross product is a `join` on the **unit key** — both
sides indexed on nothing, so every row of one meets every row of the other:

```
n1: map_index(A, key=[], val=[…])
n2: map_index(B, key=[], val=[…])
n3: join(n1, n2, on=[])             # → schema(A) ∪ schema(B)
```

This needs no new node kind and no new operator. [Invariant
4](#invariants) — every key variable of a `join` is in both input schemas —
holds vacuously of an empty key, which is why the unit key is an empty key list
rather than a constant column something would have to project away again.

### Ordering the components

Crossing multiplies, so the order decides the peak. Two numbers per component
say what it costs, and both fall out of the live-variable analysis the cost
model already runs:

- its **peak** — the cost of its own plan;
- its **exit width** — how many of its variables are still live when it ends:
  those in the head, and those a node placed after the cross consumes. No other
  component can need one, since components share no variables.

Ordering `c₁ … c_k` costs `max over j of (exit(c₁) + … + exit(c_{j−1}) + peak(c_j))`,
because everything an earlier component left behind is still live while a later
one runs. That is not a second cost function — it is what the existing analysis
reports for the concatenated order.

**Components are ordered by descending headroom**, where headroom is
`peak − exit`: how far a component swells above what it leaves behind. One that
swells and leaves little should run while there is least beside it; one that only
accumulates should run last. That is optimal by exchange — putting `x` before `y`
costs `max(peak(x), exit(x) + peak(y))` and the swap costs
`max(peak(y), exit(y) + peak(x))`, and the first is no larger exactly when `x`
has the greater headroom. Since each component's own plan already minimises its
own peak, it minimises every term, so the forest plan is optimal under this cost
model just as the single tree was.

Two things sit on top of that. **A component with exit width 0 goes first**: it
leaves nothing behind, so its position is free on the peak, and running it early
is right for the same reason running a filter early is. An atom over a relation
with no columns is always such a component. And **ties break by ascending exit
width, then by structural key** — the first is free, and the second is what makes
the choice a function of the program rather than of how it was typed.

A component can depend on another: a match can carry a variable out of one
component and into another's argument. The dependency partial order is lifted
onto components and the choice is greedy among those whose predecessors are
placed; a cycle in the lifted order is the same rejection a rooting failure is.

### Cost model

Candidate orders are scored by the **maximum number of distinct variables in
scope at any step**. This bounds the size of the worst intermediate relation,
which is what actually determines whether a rule is affordable.

Live variables are tracked through the traversal: a variable is born when the
atom or match producing it is visited, stays alive while any unvisited node or
the head still needs it, and is projected away once nothing does. The lowest
peak wins.

The model is **structural** — it uses no cardinality estimates and no
statistics. It cannot know that one relation is a thousand times larger than
another. What it does know is which plans blow up regardless of the data, and it
is deterministic, which means a program compiles to the same circuit every time.

### Determinism

The cost model settles most choices. Where it does not, the tie must still be
settled the same way every time, and [`syntax.md`](syntax.md#ast) says how:
nothing downstream may key on source **position**, so that reformatting a
program — or writing its statements in another order — cannot change what it
compiles to. "Whichever came first" is exactly what that forbids, which rules
out iteration order everywhere in this pass.

Every tie is broken by a **structural key**: a canonical rendering of a node's
own content, taken from the core form after desugaring.

| node | key |
|---|---|
| positive atom | `r(c₁: a₁, …)`, columns sorted by name; each `aᵢ` a variable name, `_`, or a literal's canonical text |
| negated atom | `not`, then the atom's key |
| filter | the canonical printing of its expression |
| match | the pattern's key, `:=`, and the canonical printing of its right-hand side |
| aggregate | the variable, `:=`, the aggregator and its argument |

An order's key is the sequence of its nodes' keys; a component's is that
sequence sorted; a **rule's** is its order's key followed by its head's, the head
keyed as an atom is. A rule needs one because several rules for one relation are
summed, and the operands of that sum have to be in an order the program decides
rather than the file does — two rules can share a body and differ only in the
head, so the head has to be in the key. Both compare lexicographically, smallest first. Keys are built
from relation, column and variable names, literal values and operator
spellings — what a program *means* — and never from a line, a column, or which
statement was written first.

The key need not be injective, and that is the point: two candidates with equal
keys emit identical grasp-dbsp, because emission reads a node's content and its
position in the order and nothing else. So the key is total on everything
observable, which is all determinism needs.

Four choices take it: the heavier edge when Prim's algorithm finds two of equal
weight; the winning rooting when two rootings cost the same; the component order
when two have equal headroom and exit width; and the order of several dependent
nodes of one kind that become ready at the same point.

### Placing dependent nodes

Filters, matches, negated atoms and aggregates are not in the spanning tree —
only positive atoms are. Each is placed at the **earliest** point where all its
inputs are bound:

```
for each dependent node N:
    k = the last position in the post-order that produces any input of N
    insert N immediately after k
```

Early is always right: a filter that runs sooner shrinks everything downstream,
and there is never a reason to carry a row that is going to be discarded.

The rule is unchanged by the forest, and has to be — the order is one sequence
whether it came from one tree or from five, and "earliest" is a position in it.
What the forest adds is only where that position can fall. A node whose inputs
all come from one component lands inside that component, before any cross, which
is what lets the component be scored on its own; one whose inputs span several
lands immediately after the cross that binds its last input, because that is the
first point at which they are in one place.

A node consuming **no** variable has no such position, and takes the first. A
negated atom over a relation with no columns is the only one there is: a plan
must have a stream before it can subtract from one.

When several become ready at the same point, they are ordered **negated atoms,
then filters, then matches, then aggregates** — most selective first, and
aggregates last because an aggregate consumes a whole group and must come after
everything contributing to it. Several of one kind are ordered by
[structural key](#determinism).

### Resolving competing producers

The join graph recorded every producer of a variable and chose none
([above](#competing-producers)). The optimizer picks one:

- The **primary** producer is whichever comes first in the post-order.
- Every other producer becomes an **equality filter** — `x = b + 1` for a
  secondary `x := b + 1` — placed where `x` and that expression's inputs are all
  available.

This is what those rules already meant. Writing `x` twice says both computations
agree; one of them supplies the value and the rest check it.

```grasp
head(val: x) <-
    r1(a: a, b: b)
    x := a * 2
    x := b + 1
```

becomes `r1` → `x := a * 2` → `filter x = b + 1` → project.

### Projection

Once a variable is needed by no remaining node and is not in the head, it is
dropped. These points fall out of the live-variable analysis the cost model
already runs, so finding them costs nothing extra.

### The whole algorithm

```
optimize(join_graph):
    components = the connected components of the weighted graph
    for each dependent node N:
        N.owner = the component producing every input of N, if one does,
                  otherwise `spanning`

    for each component C:
        C.plan = plan(C)
        C.peak = cost(C.plan)
        C.exit = |variables C produces that the head or a `spanning` node needs|

    order = []
    for each C in component_order(components):
        if order is not empty: order += cross(order, C)
        order += C.plan

    place the `spanning` dependent nodes into order
    resolve competing producers
    insert projections
    return order as a computation DAG

plan(C):
    best = none
    for each positive atom R of C:
        T = maximum spanning tree of C, rooted at R
        p = post-order traversal of T
        if p violates the dependency partial order: skip
        place C's own dependent nodes into p
        if (cost(p), key(p)) < (cost(best), key(best)): best = p
    if best is none: reject the rule
    return best

component_order(components):
    precedence = the dependency partial order, lifted onto components
    if precedence has a cycle: reject the rule
    order = []
    while some component is unplaced:
        ready = the unplaced components whose predecessors are all placed
        append the smallest of `ready` under (exit > 0, -(peak - exit), exit, key)
    return order
```

Every candidate is scored and the cheapest wins. A rule whose dependencies
genuinely contradict each other is rejected — no valid rooting for some
component, or a cycle in the lifted order — rather than evaluated in some order
that happens to work. A *disconnected* graph is not that case and never was one
worth rejecting: a forest is a plan.

### Grounding a body with no atom

A body of nothing but matches has an empty join graph, and there is nothing to
root. **The graph builder gives it one node**: an occurrence of the **unit
relation**, which has no columns and holds exactly one tuple.

It belongs there rather than in desugaring or in the optimizer, for two reasons.
Nothing a program writes *means* `unit()`, so it is not a surface form standing
for another. And it is not a relation: it never enters the program's namespace,
so name resolution, the spec check, SCC analysis and stratification never see a
synthetic relation each would have to remember to ignore. It is one node of a
kind the graph already has.

```grasp
answer(v: n) <-
    n := 6 * 7
```

The rule then plans like any other — one component, one atom, the matches placed
as dependent nodes — and the cost model, the placement rule and projection are
all untouched. A fact is the same shape with the computation moved into the head.

The unit relation is not writable: it has no name in grasp, and it is the only
atom the compiler ever adds to a body.

## Computation DAG

The optimizer's output: one per rule, ordered, acyclic, single-sink. Nodes are
column-level operations and edges carry sets of tuples with named columns.

**The node vocabulary is grasp-dbsp's.** The Erlang implementation named these
after the relational operations they perform; here they are named after the
operator each becomes, so a reader does not learn two names for one thing.
[`mapping.md`](mapping.md) has the old-to-new table.

| node | what it does | output schema |
|---|---|---|
| `map_index(atom, key, val)` | entry: index a relation on join keys | `key ∪ val` |
| `join(L, R, on)` | equi-join two sub-DAGs | `schema(L) ∪ schema(R)` |
| `antijoin(L, R, on)` | keep rows of `L` with no match in `R` | `schema(L)` |
| `filter(pred, over)` | drop rows failing the predicate | unchanged |
| `map(f, in, out)` | compute `f` and bind it to `out` | input ∪ `{out}` |
| `flat_map(f, in, out)` | one row in, many out | `out` |
| `aggregate(agg, col, group, out)` | group and fold | `group ∪ {out}` |
| `distinct(in)` | collapse every weight to one | unchanged |

**`map` is also projection.** The Erlang design had a separate `project` node
for dropping columns; both it and `map` lower to grasp-dbsp's one `map`
operator, so there is one node kind here. A `map` carries its output schema, and
a projection is a `map` that binds nothing and narrows. The sink of every
computation DAG is therefore a `map` producing the head's columns.

**`map_index` is the only entry.** It is what the old design called `arrange`.
It has no inputs from other nodes — it reads a relation. Which relation, and
where the data comes from, depends on stratum and is resolved during emission.

Several `map_index` nodes may read the same relation with different keys. That
is what a self-join needs, and what a recursive rule mentioning `path` twice on
different columns needs.

### What each body statement becomes

Every form in a rule body becomes DAG nodes. Desugaring has already run, so the
patterns arrive as the bindings and filters
[`semantics.md`](semantics.md#desugaring) expands them into — the rows below are
what those, and the forms that do not desugar, compile to.

| body statement | nodes |
|---|---|
| positive atom, a leaf of the tree | `map_index` |
| positive atom, joined to its parent | `map_index` then `join` |
| negated atom | `map_index` on each side, then `antijoin`, then `map` to flatten |
| filter | `filter` |
| `v := expr` | `map` binding `v` |
| `v := agg<e>` | `map_index` on the group, `aggregate`, `map` to flatten |
| `(v) := *arr` | `flat_map` over the array |
| `(k, v) := **d` | `flat_map` over `entries(d)` |
| `v :: T` | `filter` — the runtime check, where one was inserted |
| head | `map` projecting the head's columns |

Two of these are worth spelling out.

**An aggregate is four nodes, not one.** It ranges over the body's satisfying
assignments, so the assignment is deduplicated first; grouping is the index key,
so the group columns are indexed next, folded, then flattened back:

```grasp
payroll(dept: d, total: s) <-
    emp(dept: d, sal: r)
    s := sum<r>
```

```
n1: distinct(emp)                                # over every variable bound
n2: map_index(n1, key=[d], val=[r])
n3: aggregate(sum, col=r, group=[d], out=s)
n4: map(→ [d, s])
```

The `distinct` is why nothing is projected away before an aggregate: liveness
keeps every variable the body binds alive until `n1`, because two assignments
differing only in a variable nothing reads are still two, and collapsing them
first would lose the multiplicity a weight-scaled fold depends on. It also means
the cost model has nothing to say about a rule with an aggregate — every rooting
carries the same variables and so has the same peak — and the rooting falls to
the [structural key](#determinism).

**A dict unnest is a `flat_map` over its entries**, which is why entries come
out in key order — a relation is a set, and the rows produced must not depend on
how the dict was built:

```grasp
tagged(name: n, tag: k) <-
    person(name: n, tags: d)
    (k, _v) := **d
```

```
n1: map_index(person, key=[], val=[n, d])
n2: flat_map(entries(d), in=[d], out=[k, _v])
n3: map(→ [n, k])
```

A dict lookup — from a `{a: x} := d` pattern, or written directly — is an
ordinary `map` computing `get(d, "a")`, followed by the `filter` its
`x :: V` assertion became.

### Invariants

1. **One producer per variable.** Each variable is introduced by exactly one
   node. Variables are never redefined.
2. **Inputs precede use.** Every variable a node consumes is in the schema of
   some ancestor.
3. **Acyclic.** Recursion is a property of the program, not of a rule.
4. **Key validity.** For `join` and `antijoin`, every key variable is in both
   input schemas. An empty key satisfies this vacuously, which is what makes a
   cross product a `join` rather than an operator of its own.
5. **Group validity.** For `aggregate`, the group keys and aggregated column are
   in the input schema, and the output variable is not.

## SCC analysis

The per-rule DAGs are assembled into one program-level graph, and relations are
partitioned into strongly connected components of the dependency graph.

| kind | meaning |
|---|---|
| input table | comes from outside; not computed by rules |
| derived, non-recursive | computed, in no cycle |
| derived, recursive | computed by rules forming a cycle |

Components are topologically ordered into **strata**. Negated atoms and
aggregates must reference a strictly lower stratum; if one cannot, the program
is rejected with a diagnostic naming the cycle. This is what makes negation and
aggregation well-defined rather than merely usually-fine.

A recursive component becomes one fixpoint. Everything else is a straight-line
piece of the circuit.

### What the backend already does

Two jobs the Erlang implementation did here do not exist in this pipeline.

**Deduplicating entries.** That design deduplicated `arrange` nodes across
rules, so two rules indexing the same relation the same way shared one. This
pipeline does not: grasp-dbsp content-addresses its nodes, so identical
`map_index` calls collapse into one node on their own. Emitting the redundant
form is free, and the SCC graph is simpler for not tracking it.

**Incrementalization.** That design ran a pass inserting `integrate` before
every non-linear operator and `differentiate` after it. `dbsp` does this, so the
pass is gone entirely.

### Fixpoint shape

A recursive component compiles to one grasp-dbsp `fixpoint`, and its constraints
are worth stating because one of them reads more strictly than it is:

- **Components are independent.** Sibling fixpoints in one program are fine, so
  each recursive component gets its own.
- **Members may have different column types.** Mutually recursive relations of
  different arity compile into one fixpoint without encoding tricks.
- **Members must share a batch shape.** grasp-dbsp requires every recursive
  stream in one fixpoint to be the same *shape* — all `zset` or all
  `indexed_zset` — which is what its "same shape" rule means. Since a relation
  is a `zset`, this is satisfied by construction. The cost is that a recursive
  relation cannot be kept pre-indexed across rounds: it is re-indexed by a
  `map_index` inside the body each round.
- **Fixpoints do not nest.** grasp-dbsp rejects a fixpoint inside a fixpoint.
  Stratification means a recursive component never contains another, so this
  cannot arise from a well-formed program — but a compiler bug that produced one
  is caught rather than miscompiled.

## Worked example

```grasp
path(src: x, dst: y) <- edge(src: x, dst: y)
path(src: x, dst: y) <-
    path(src: x, dst: z)
    edge(src: z, dst: y)
```

**SCC analysis.** `edge` is an input table. `{path}` is one recursive component.

**Rule 1** has one atom, so there is nothing to order:

```
n1: map_index(edge, key=[], val=[x, y])
n2: map(→ [x, y])
```

**Rule 2** has two atoms sharing `z`. The weighted join graph is a single edge
of weight 1, so both rootings give the same tree and the same cost:

```
n3: map_index(path, key=[z], val=[x])     # recursive — internal
n4: map_index(edge, key=[z], val=[y])     # input table — external
n5: join(n3, n4, on=[z])                  # → {x, y}
n6: map(→ [x, y])
```

**Program level.** The two rules' outputs are merged and deduplicated, and the
whole component becomes one fixpoint whose recursive member is `path`. What that
looks like as grasp-dbsp is in [`mapping.md`](mapping.md#worked-example).
