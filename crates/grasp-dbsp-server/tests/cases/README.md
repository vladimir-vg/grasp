# Configuration fixtures

Each file is a list of cases; each case is one configuration file and what
should become of it. The harness is `tests/config.rs`.

```yaml
- name: a sentence saying what the case shows
  config: |
    workers: 3
  expected_workers: 3
```

A case either **is accepted** — in which case `expected_name`,
`expected_workers` and `expected_storage` say what it must become, and any of
them may be left out — or **is rejected**, in which case
`expected_diagnostics` lists a substring of each diagnostic it must produce.
The count is exact as well as the content, so a case cannot pass on one
diagnostic while a second, unnoticed one is also firing.

`program` is a grasp-dbsp program to check the configuration against. Only the
keys that name something in a program need it, which today is `materialized`.

An accepted case is taken all the way to a `RunnerConfig`, so opening the
storage backend is part of what "accepted" means.

Each entry in `expected_diagnostics` stands for **one** diagnostic, matched as
a substring of the whole rendering. Two phrases of one message are not two
entries; pick the one that is distinctive enough to fail on a typo.
