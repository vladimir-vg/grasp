# Raft leader election in grasp

Each Raft node runs its own `grasp-dbsp-server` beside a small Python process.
Every decision a node makes is a rule in [`raft.grasp`](raft.grasp): when to
campaign, whom to vote for, whether it has won, what to send. The Python only
moves records. It delivers each message the program derives into the
recipient's server, and fires two timers into its own.

```bash
python3 examples/raft/launch.py
```

The launcher builds `grasp` and `grasp-dbsp-server`, compiles `raft.grasp`, and
starts three nodes on ports 18201–18203. Then it puts the cluster through one
failure:

1. It waits for a leader.
2. It kills the leader and waits for a new leader in a later term.
3. It replaces the dead node with a new one under a new id.
4. It waits for the newcomer to follow the leader.

Throughout, it watches every node and checks that no term ever had two leaders
and no node ever voted twice in one term. It exits non-zero if either happened.
It needs `python3` with `requests`.

## How it works

**A node's state is its history.** Nothing in the program is carried from one
transaction to the next. A node's state is derived from its `inbox`, every
message it has ever received, whose offsets the runtime fills in arrival order:

```grasp
inbox(partition:, offset:, kind:, from:, term:, tick:) <- input partition_as: "partition", offset_as: "offset"
```

**Every decision is made "as of" an offset.** `term_before` is the highest term
among messages with a lower offset, which is the term the node knew when a
message arrived. So a vote request is judged against what the node knew then,
and a later message cannot reach back and change the judgement.

**A vote, once given, stays given.** The vote in a term is the earliest eligible
claim:

```grasp
voted(term: t, cand: argmin<c, by: o>) <-
    eligible(offset: o, term: t, cand: c)
```

A claim arriving later always has a later offset, so it can never become the
earliest. That's the promise Raft needs a voter to keep, and here arrival order
keeps it, with no stored state.

**A node's own decisions come back to it as messages.** A candidate sends itself
a `campaign`, beside the `request_vote`s to its peers. Its vote for itself then
competes with incoming requests under the same `argmin`. And "the highest term
so far" never has to include something the program itself derived, which would
be recursion through an aggregate.

**Watching uses snapshots.** The launcher follows three views on every node:
`won`, `voted` and `leader_known`. An egress stream carries only what changes
after it opens, and a row derived before the watcher connects doesn't change
again. A newly started node, for instance, can recognize the leader in the
moment before anyone is watching. So [`pipeline.yaml`](pipeline.yaml)
materializes those views, and the launcher asks for their current contents
first (`send_snapshot=true`). The first version didn't, and reported a node that
was following perfectly well as never having found a leader.

**Sends are rows entering a view.** `send` is an ordinary relation. Python
subscribes to it with `backpressure=true` and delivers each row that enters.
Rows that leave are ignored, since a message can't be unsent. A message sent
twice with the same content is the same row, so it arrives once.

## What this is not

- **Election only.** There's no log replication, and so no commit index.
- **History grows without bound.** Every message and timer tick stays in the
  inbox, and the "as of" rules join against all of it, so a node slows down the
  longer it runs. The example runs for about a minute.
- **Membership changes aren't Raft-safe during the change.** `member` is an
  input the launcher edits on every node. For a moment two nodes can count
  different memberships, which is why the launcher changes it only while a
  leader is in place, never mid-election. Real Raft changes membership through
  the log.
- **One partition.** Every node reads and writes partition `0`, and names it
  explicitly rather than taking the runtime's default.
- **A crashed node doesn't come back.** It rejoins, if at all, as a new node
  under a new id.
