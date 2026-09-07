# grasp — Compilation

How a grasp program becomes a plan. This document covers everything up to the
point where grasp-dbsp is emitted; emission itself is
[`mapping.md`](mapping.md).

```
text → AST → typed AST → join graph → computation DAG → SCC graph → grasp-dbsp
                            (per rule)     (per rule)     (whole program)
```

The middle three stages are where the interesting decisions are made, and none
of them knows what the backend is.

## Type checking

Inference runs before anything else, and produces column types for every
relation and a type for every variable in every rule. Four phases.

**1. Per-rule unification.** Within one rule, types flow from atoms to
variables, from variables through expressions, and from the result back to the
head's columns. An atom binds its variables to its relation's column types; a
match binds its variable to its expression's type; a filter requires `boolean`.

**2. Cross-rule unification.** A relation defined by several rules takes the
type each rule gives it, and they must agree. Where a column is produced at two
types that are both valid, it widens to accept either.

**3. Concrete assignment.** Untyped numeric literals resolve against their
context. A variable still unconstrained after this is an error naming the
variable, not a silently-defaulted `i64`.

**4. Overload resolution and filter insertion.** Builtin calls resolve to one
signature. Where a type is not assignable but a runtime check could rescue it —
`optional(T)` used as `T` — the compiler inserts a filter that drops the rows
that do not match and narrows the variable, rather than rejecting the program.

Iteration is a bottom-up fixpoint: relations whose bodies are fully known give
types to their heads, which unlocks the next round. Recursive relations settle
in this loop like any other — a recursive rule's head type is constrained by its
non-recursive rules first.

Types are erased after this. Nothing downstream carries them except as the
record shapes the emitted program declares.

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

Atoms sharing no variable have no edge. A plan needing a cross product therefore
cannot be built as a spanning tree, which is the point: the optimizer will not
reach for one unless the rule leaves it no choice.

### Join spanning tree

A **join spanning tree** is a maximum-weight spanning tree of that graph —
Prim's algorithm, always taking the heaviest edge from the visited set. For an
acyclic rule it coincides with a classical join tree; for a cyclic one it is the
best tree-shaped approximation, cutting the lightest edges to break cycles.

The optimizer tries **every atom as root**. For each, a post-order traversal of
the rooted tree gives an evaluation order: children before parents, each parent
joining its children's results with its own stream. Orders that violate the
partial order are discarded.

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

### Placing dependent nodes

Filters, matches, negated atoms and aggregates are placed at the **earliest**
point where all their inputs are in scope. Early filtering shrinks everything
downstream, and there is never a reason to wait.

Aggregates are the exception to "earliest": an aggregate consumes a whole group,
so it goes after everything contributing to its group.

### Projection

Once a variable is needed by no remaining node and is not in the head, it is
dropped. These points are computed from the live-variable analysis, so a
projection costs nothing extra to find.

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

### Invariants

1. **One producer per variable.** Each variable is introduced by exactly one
   node. Variables are never redefined.
2. **Inputs precede use.** Every variable a node consumes is in the schema of
   some ancestor.
3. **Acyclic.** Recursion is a property of the program, not of a rule.
4. **Key validity.** For `join` and `antijoin`, every key variable is in both
   input schemas.
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
