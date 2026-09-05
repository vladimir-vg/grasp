# YAML end-to-end fixtures

Every `.yaml` file here holds a **list of cases**, and each case becomes one
test named `<file stem>::<index>_<name>`:

```
cargo test --test yaml              # all of them
cargo test --test yaml joins        # one file
cargo test --test yaml unknown_field
```

The harness is [`tests/yaml.rs`](../yaml.rs).

## A case

Every case has `source` and exactly one `expected_*` key. The prefix is the
convention: if it asserts something, it is called `expected_something`.

| key | asserts |
|---|---|
| `expected_exact_output` | the output deltas are exactly these |
| `expected_output` | these deltas appear; extras are ignored |
| `expected_diagnostics` | the program fails to compile, with these diagnostics |

`name` is optional and only affects the test's name.

## Output cases

`input` is a list with one entry per transaction, keyed by **table** name — the
string inside `input("...")`, which need not match the node's name.
`expected_*_output` is a list of the same length, keyed by **node** name. Use
`{}` for a transaction that produces nothing.

Which nodes are observed is derived from the keys you mention; there is no
separate declaration, because the runner selects outputs by node name anyway.

A row is `[weight, value]` for a flat stream and `[weight, key, value]` for an
indexed one — `aggregate` and `weighted_count` produce indexed streams. The
arity is what distinguishes them.

```yaml
- name: max salary per department
  source: |
    emp := input("emp")
    emp :: zset(record(id: i64, dept_id: i64, salary: i64))
    idx := map_index(emp, fun((r) -> (r.dept_id, r)))
    by_dept := aggregate(idx, max, fun((v) -> v.salary))

  input:
    - emp:
        - [1, {id: 1, dept_id: 10, salary: 100}]
        - [1, {id: 2, dept_id: 10, salary: 200}]
    - emp:
        - [-1, {id: 2, dept_id: 10, salary: 200}]

  expected_exact_output:
    - by_dept:
        - [1, 10, 200]
    - by_dept:
        - [-1, 10, 200]
        - [1, 10, 100]
```

Scalars are written as native YAML values (`id: 1`, not `id: "1"`), so a fixture
shows exactly what the JSON wire format accepts. Values go through the runner's
own codec, so a type that does not match the declared column is an error rather
than being quietly coerced.

**Prefer `expected_exact_output`.** Subset matching passes when the runner emits
rows nobody expected, which is the bug class these tests exist to catch.

## Diagnostic cases

```yaml
- name: unknown field
  source: |
    a := input("a")
    a :: zset(record(id: i64))
    b := filter(a, fun((r) -> r.nope > 1))

  expected_diagnostics:
    - severity: error
      pass: typecheck
      message: "has no field `nope`"
      line: 3
      column: 27
```

**Only the fields a case writes are checked.** Omitting `column` asserts nothing
about the column; that is what lets the diagnostic model grow — new fields, and
eventually several diagnostics per pass — without editing existing fixtures.

`message` is a substring match. Everything else is exact. `pass` is `parse`,
`typecheck` or `lower`; `severity` is `error`, `warning` or `note`.

Matching is an **exact set, order-independent**: every listed diagnostic must
appear and nothing else may, since a spurious extra error is itself a bug.

Give each case a `pass` and a `message` specific enough that a typo in the
fixture's own source — which would also fail to compile — cannot satisfy it.

## Notes

- Unknown keys are rejected, so a misspelled key fails loudly instead of
  silently asserting nothing.
- The language this exercises is documented in
  [`docs/design/language.md`](../../docs/design/language.md). Operators not yet
  implemented are listed under future work in `docs/design/overview.md`.
