# Serving a program

[`language.md`](language.md) specifies what a program means and
[`mapping.md`](mapping.md) how it becomes a circuit. Neither says how a program
is *started*, given a worker count, or reached from outside the process. That is
this document, and the crate it describes is `grasp-dbsp-server`.

A program says what it computes and deliberately not how it is run. The **host**
supplies the rest — which nodes are observed, how many workers, where storage
lives. This server is the host this workspace ships: one program, one process,
one pipeline.

Its HTTP API is a subset of [Feldera](https://github.com/feldera/feldera)'s
pipeline API, spelled the way Feldera spells it, so that `curl` scripts, the
Python SDK and `fda` work unmodified within the subset. Where a Feldera feature
is absent, the request is **refused in a sentence that says why** rather than
accepted and approximated.

## The command line

```bash
grasp-dbsp-server serve program.gdbsp --config-file pipeline.yaml --port 8080
```

| flag | meaning |
|---|---|
| `--config-file <path>` | the pipeline configuration below; omitted means `{}` |
| `--bind-address <addr>` | default `127.0.0.1` |
| `--port <n>` | default `8080`; `0` asks the operating system and prints what it got |
| `--paused` | start paused — rows are accepted and not computed, as Feldera's `--initial=paused` |

Everything that can fail is built *before* a port is bound: the program is
compiled, the configuration read and checked against it, the storage directory
opened and the circuit built. A program that does not compile, a configuration
naming a view the program lacks, or a storage directory another pipeline holds
is a diagnostic on stderr and a non-zero exit — never a server that accepts
connections it cannot answer.

```bash
grasp-dbsp-server validate program.gdbsp --config-file pipeline.yaml
```

`validate` runs exactly those checks and starts nothing. Its diagnostics are the
compiler's, rendered identically — a bad configuration key reports with a line
and a column, beside the diagnostics a bad program produces.

## The configuration file

YAML, with Feldera's keys and Feldera's types. A file whose entire content is
`{}` is a valid single-worker in-memory pipeline named `grasp`.

```yaml
name: shop
workers: 4
max_rss_mb: 2048
storage:
  min_storage_rows: 1000
storage_config:
  path: /var/lib/shop
materialized:
  - big_orders
```

| key | meaning | default |
|---|---|---|
| `name` | the name in `/v0/pipelines/{name}/…` | `grasp` |
| `workers` | `dbsp` worker threads | `1` |
| `max_rss_mb` | process memory budget, in megabytes; what makes spilling adapt | none |
| `storage` | *how* storage is used — Feldera's `StorageOptions`, verbatim | none |
| `storage_config` | *where* it lives — Feldera's `StorageConfig`, verbatim | none |
| `materialized` | views whose contents are kept, so `send_snapshot=true` has something to send | none |

### Three departures from Feldera, each deliberate

**Unknown keys are refused.** Feldera accepts and ignores them. A configuration
file is the artifact a person is most likely to get wrong, and a silently
ignored key is a setting that did not take effect and said nothing.

**`workers` defaults to 1, where Feldera's default is 8.** More than one worker
rests on the hashing invariants in [`mapping.md`](mapping.md); a default that
quietly engages them would make a placement bug somebody else's mystery. Feldera
chose 8 for production clusters, and a file that says nothing here is not one.

**Storage is off unless asked for.** Feldera's manager always supplies a
directory to go with its default storage options. There is no manager here, and
inventing a temporary directory would mean a restart silently lost whatever had
spilled.

`storage` and `storage_config` are meaningless apart, which is Feldera's own
rule, and each without the other is refused by name rather than defaulted
around. `workers: 0` is refused too — leave the key out for one.

### `materialized`, which Feldera does not have

Feldera learns materialization from SQL's `CREATE MATERIALIZED VIEW`. grasp-dbsp
has no such declaration, and this runtime has no way to read a relation's
contents at all: `step` returns deltas and nothing else. A snapshot is therefore
a fold this server keeps in memory, costing a second copy of the relation
outside the circuit's own budget. That cost is why it is opt-in and named per
view. A view listed here that the program does not declare is refused, naming
the views it does have — a renamed node leaves its configuration behind.

### Feldera keys refused by name

These exist in a Feldera configuration and have no counterpart here. Each is
refused with a sentence about *that key*, not with serde's list of the fields it
expected:

| keys | why not |
|---|---|
| `inputs`, `outputs` | configure Feldera connectors — Kafka, files, object stores. There is exactly one input transport here and one output transport, so there is nothing to configure |
| `fault_tolerance`, `checkpoint_during_suspend` | need checkpoints, and this runtime starts a circuit from nothing. Restoring one would pin the worker count it was written at and freeze `DynValue`'s archived variant order |
| `hosts`, `multihost` | a multi-host `dbsp` layout. This runtime names a worker count and nothing else |
| `clock_resolution_usecs`, `clock_timezone_offset` | pace Feldera's clock for SQL's `NOW()`. This language has no clock: a timestamp is a value a program is given, never one it reads |
| `min_batch_size_records`, `max_buffering_delay_usecs` | tune how Feldera's controller batches input before stepping. This server steps once per drain of its command queue, so a burst of requests is already one transaction |
| `program_ir`, `resources`, `logging`, `tracing`, `http_workers`, and the rest of the manager's vocabulary | pipeline-manager settings with no counterpart in a server that runs one program in one process |

## The HTTP surface

Every route answers at two spellings: bare (`/ingress/orders`) and
manager-style (`/v0/pipelines/{name}/ingress/orders`). Both reach the same
pipeline. A request naming a *different* pipeline is a 404 — what a manager
would say about a pipeline it had never heard of.

| route | method | what it does |
|---|---|---|
| `/ingress/{table}` | POST | insert rows; answers with a completion token |
| `/egress/{view}` | POST | subscribe to a view's deltas as a stream |
| `/start`, `/resume` | GET, POST | resume a paused circuit |
| `/pause` | GET, POST | stop stepping; rows are still accepted |
| `/stop` | POST | shut the circuit down |
| `/start_transaction` | POST | stop stepping on its own |
| `/commit_transaction` | POST | run everything buffered as one step |
| `/completion_status?token=…` | GET | has this ingestion been computed? |
| `/stats` | GET | state, step counters, transaction status |
| `/metadata` | GET | the tables, views and materialized views this program has |

### Ingress

```bash
curl -X POST 'localhost:8080/v0/pipelines/shop/ingress/orders?format=json&update_format=insert_delete' \
  --data-binary '{"insert": {"id": 1, "amount": 250}}'
```

`format=json` is the only format. **Feldera's default is `csv`**, so a client
that omits the parameter works there and not here — `format=csv` is refused
saying exactly that, rather than silently misreading the body.

`update_format` selects between the two JSON shapes, and defaults to
`insert_delete`:

```json
{"insert": {"id": 1, "amount": 250}}
{"delete": {"id": 1, "amount": 250}}
```

```json
{"weight": 1, "data": {"id": 1, "amount": 250}}
{"weight": -3, "data": {"id": 1, "amount": 250}}
```

The second is `update_format=weighted`, and it is the reason this codec exists:
a weight of `-3` is a legal Z-set update that Feldera's own ingestion panics on.

Rows are newline-separated, or a JSON array with `array=true`. **A bad row among
good ones ingests the good ones and reports the bad**, with a 400 carrying
`num_errors` and a per-row description. That is Feldera's behaviour and is
matched rather than improved on: a client that retried the whole batch after a
400 would double-insert.

The response is a completion token:

```json
{"token": "eyJ1IjoiMDUxZTczNjItNzU4OC00OWIyLWEzNjUtN2UzNDc2YWY3OGQyIiwiZSI6MCwiYyI6MX0"}
```

### Completion tokens

`POST /ingress` returns as soon as the rows are queued, so without a token a
client has no way to learn whether its write has been computed. That token is
the whole contract ingestion offers.

```bash
curl 'localhost:8080/v0/pipelines/shop/completion_status?token=<token>'
```

```json
{"status": "complete", "step": 1}
```

`status` is `complete` or `inprogress`; `step` is the step the ingestion landed
in, or `null` if it has not been assigned one yet.

The encoding is Feldera's byte for byte — base64url without padding of
`{"u": <incarnation>, "e": <endpoint>, "c": <watermark>}` — not because a client
parses it, but because a token that decodes with Feldera's own
`CompletionToken::decode` is evidence of compatibility that a token of our own
invention could not be.

`u` is the **incarnation**, generated at startup. A token minted before a
restart decodes fine and is refused by name, rather than being satisfied by a
step number that has come round again: this run started from nothing, so what
the token refers to was never computed here. That is the case an operator
actually hits, after restarting a server and replaying a script.

### Egress

```bash
curl -X POST 'localhost:8080/v0/pipelines/shop/egress/big_orders?format=json&send_snapshot=true'
```

A chunked stream, one JSON object per chunk, `\r\n`-separated:

```json
{"sequence_number": 0, "snapshot": true, "json_data": [{"insert": {"id": 1, "amount": 250}}]}
```

`json_data` carries the same two shapes ingress accepts, chosen by
`update_format`. `send_snapshot=true` sends the view's current contents as the
first chunk with `snapshot: true`, and requires the view to be listed in
`materialized`. The snapshot and the stream that follows it neither overlap nor
leave a gap — the subscription is established on the circuit thread, between
transactions.

A subscriber that falls more than 100 chunks behind **starts losing chunks, and
its sequence numbers gap** rather than silently renumbering, so a client
reconstructing state from a delta stream can tell that it has. Feldera's number
and Feldera's behaviour. `backpressure=true` instead stalls the circuit until
the slow subscriber catches up — which is a choice about the whole pipeline, not
about one connection.

A keepalive chunk goes out every three seconds on an idle stream, copied
deliberately: actix does not notice a departed client on a stream that is
sending nothing. It carries no `json_data` at all, and consumes a sequence
number like any other chunk:

```json
{"sequence_number": 1, "snapshot": false}
```

### Pause, resume, and transactions

A paused circuit accepts rows and does not compute them; `buffered_input_records`
in `/stats` grows and `total_completed_steps` does not.

`POST /start_transaction` is "stop stepping on your own", and
`POST /commit_transaction` runs everything buffered as one step.
Feldera's transaction model is already this runtime's execution model —
`Runner::step()` *is* one transaction — so a burst of concurrent pushes is
already grouped into one, and an explicit transaction is the knob for grouping
more. Transactions do not nest: opening one while one is open is a 409, and
committing with nothing open is a diagnostic rather than a panic.

## Errors

Feldera's envelope, because a drop-in server that invents its own is not one — a
client's error handling is as much part of the API as its success path:

```json
{"message": "this pipeline has no table named `nosuch`", "error_code": "UnknownInputTable", "details": {}}
```

| `error_code` | status | when |
|---|---|---|
| `UnknownInputTable` | 404 | no such table |
| `UnknownOutputTable` | 404 | no such view |
| `UnknownPipelineName` | 404 | a request for a pipeline this process does not serve |
| `InvalidParam` | 400 | a query parameter, format or token this server does not implement — and anything else the runtime refuses, such as committing with no transaction open |
| `ParseErrors` | 400 | rows that did not parse; `details` carries `num_errors` and each failure |
| `TransactionInProgress` | 409 | a transaction is already open |
| `Terminating` | 410 | the circuit is no longer running |
