"""Runs a Raft cluster of grasp-dbsp-servers and puts it through one failure.

1. Starts three nodes and waits for a leader.
2. Kills the leader, and waits for a new leader in a higher term.
3. Replaces the dead node: removes it from every member list, starts a node
   under a new id, and adds that id to every member list. Membership changes
   here are not Raft's joint consensus, so this is done only once a leader is
   in place, never mid-election.
4. Waits for the new node to follow the current leader.

Throughout, it watches every node's `won` and `voted` views and checks the two
promises an election makes: at most one leader per term, and at most one vote
per node per term. It exits non-zero if either was ever broken.
"""

import argparse
import json
import pathlib
import subprocess
import sys
import tempfile
import threading
import time

import requests

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[1]
PIPELINE = "grasp"


class Watch:
    """Every row each node's views have ever inserted."""

    def __init__(self):
        self.lock = threading.Lock()
        self.won = {}          # term -> {node}
        self.voted = {}        # (node, term) -> {candidate}
        self.leader_known = {} # node -> (term, leader), the latest seen

    def follow(self, base_port, node, stopping):
        for view in ("won", "voted", "leader_known"):
            threading.Thread(
                target=self._stream, args=(base_port, node, view, stopping), daemon=True
            ).start()

    def _stream(self, base_port, node, view, stopping):
        url = f"http://127.0.0.1:{base_port + node}/v0/pipelines/{PIPELINE}/egress/{view}"
        while not stopping.is_set():
            try:
                # The snapshot first: a row derived before this stream opened
                # would otherwise never be seen, since an egress stream carries
                # only what changes after it opens. `pipeline.yaml` materializes
                # these views so that a snapshot can be asked for.
                params = {"format": "json", "send_snapshot": "true"}
                with requests.post(url, params=params, stream=True, timeout=None) as r:
                    for line in r.iter_lines():
                        if stopping.is_set():
                            return
                        if not line:
                            continue
                        for delta in json.loads(line).get("json_data", []):
                            row = delta.get("insert")
                            if row is not None:
                                self._record(node, view, row)
            except requests.RequestException:
                time.sleep(0.2)

    def _record(self, node, view, row):
        with self.lock:
            if view == "won":
                self.won.setdefault(row["term"], set()).add(node)
            elif view == "voted":
                self.voted.setdefault((node, row["term"]), set()).add(row["cand"])
            else:
                current = self.leader_known.get(node)
                if current is None or row["term"] >= current[0]:
                    self.leader_known[node] = (row["term"], row["leader"])

    def leader(self, among):
        """The leader of the highest term won by one of `among`, if any."""
        with self.lock:
            terms = [(t, n) for t, ns in self.won.items() for n in ns if n in among]
        return max(terms) if terms else None

    def violations(self):
        with self.lock:
            out = [f"term {t} has two leaders: {sorted(ns)}" for t, ns in self.won.items() if len(ns) > 1]
            out += [
                f"node {n} voted twice in term {t}: {sorted(cs)}"
                for (n, t), cs in self.voted.items()
                if len(cs) > 1
            ]
            return out


def wait_for(what, predicate, timeout, base_port=None, nodes=()):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.2)
    # Say whether the cluster was still moving: a node whose input count is
    # still climbing is receiving messages, and one that does not answer is not.
    for node in nodes:
        try:
            g = requests.get(
                f"http://127.0.0.1:{base_port + node}/v0/pipelines/{PIPELINE}/stats", timeout=2
            ).json()["global_metrics"]
            print(
                f"  node {node}: {g['total_input_records']} records in, "
                f"{g['total_completed_steps']} steps, {g['buffered_input_records']} buffered"
            )
        except requests.RequestException as e:
            print(f"  node {node}: no answer ({e.__class__.__name__})")
    raise SystemExit(f"timed out waiting for {what}")


def push(base_port, node, table, row, delete=False):
    url = f"http://127.0.0.1:{base_port + node}/v0/pipelines/{PIPELINE}/ingress/{table}"
    body = json.dumps({"delete" if delete else "insert": row})
    requests.post(url, params={"format": "json"}, data=body, timeout=5).raise_for_status()


def main():
    # Progress should be visible as it happens, including when the output is
    # redirected to a file, where Python would otherwise buffer it to the end.
    sys.stdout.reconfigure(line_buffering=True)
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--base-port", type=int, default=18200)
    ap.add_argument("--no-build", action="store_true")
    args = ap.parse_args()
    base = args.base_port

    if not args.no_build:
        subprocess.run(
            ["cargo", "build", "-q", "-p", "grasp-compiler", "-p", "grasp-dbsp-server"],
            cwd=ROOT,
            check=True,
            env={**__import__("os").environ, "CARGO_BUILD_JOBS": "4"},
        )
    grasp = ROOT / "target" / "debug" / "grasp"
    server = ROOT / "target" / "debug" / "grasp-dbsp-server"

    work = pathlib.Path(tempfile.mkdtemp(prefix="grasp-raft-"))
    program = work / "raft.gdbsp"
    subprocess.run([grasp, "compile", HERE / "raft.grasp", "-o", program], check=True)
    print(f"compiled raft.grasp; each node's server log is in {work}")

    stopping = threading.Event()
    nodes = {}
    watch = Watch()

    def start(node, members):
        nodes[node] = subprocess.Popen([
            sys.executable, HERE / "node.py",
            "--id", str(node),
            "--base-port", str(base),
            "--members", ",".join(str(m) for m in members),
            "--server-bin", server,
            "--program", program,
            "--log", work / f"node-{node}.log",
        ])
        watch.follow(base, node, stopping)

    try:
        members = [1, 2, 3]
        for n in members:
            start(n, members)

        term, leader = wait_for("a first leader", lambda: watch.leader(members), 60, base, list(nodes))
        print(f"term {term}: node {leader} leads")

        nodes[leader].terminate()
        nodes[leader].wait(timeout=20)
        members.remove(leader)
        print(f"killed node {leader}")

        term2, leader2 = wait_for(
            "a leader in a later term",
            lambda: (lambda l: l if l and l[0] > term else None)(watch.leader(members)),
            60,
            base,
            list(members),
        )
        print(f"term {term2}: node {leader2} leads")

        newcomer = 4
        for n in members:
            push(base, n, "member", {"node": leader}, delete=True)
        start(newcomer, members + [newcomer])
        for n in members:
            push(base, n, "member", {"node": newcomer})
        members.append(newcomer)
        print(f"replaced node {leader} with node {newcomer}")

        following = wait_for(
            f"node {newcomer} to follow a leader",
            lambda: watch.leader_known.get(newcomer),
            60,
            base,
            list(members),
        )
        print(f"node {newcomer} follows node {following[1]} in term {following[0]}")

        time.sleep(3)
        problems = watch.violations()
        for p in problems:
            print("VIOLATION:", p)
        print("OK: one leader per term, one vote per node per term" if not problems else "FAILED")
        return 1 if problems else 0
    finally:
        stopping.set()
        for proc in nodes.values():
            if proc.poll() is None:
                proc.terminate()
        for proc in nodes.values():
            try:
                proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                proc.kill()


if __name__ == "__main__":
    sys.exit(main())
