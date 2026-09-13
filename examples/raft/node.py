"""One Raft node: a grasp-dbsp-server running raft.gdbsp, and the plumbing
around it.

This file makes no Raft decisions. It starts the server, tells it who it is
and who the members are, delivers every message the program derives into the
recipient's server, and fires two timers into its own. Everything that decides
anything is in raft.grasp.

A node's server listens on base_port + id, so any node can reach any other by
its id alone, including one that joined after it started.
"""

import argparse
import itertools
import json
import random
import signal
import subprocess
import sys
import threading
import time

import requests

PIPELINE = "grasp"


def url(base_port, node, path):
    return f"http://127.0.0.1:{base_port + node}/v0/pipelines/{PIPELINE}{path}"


def push(base_port, node, table, row, delete=False, partition=None):
    """Insert (or delete) one row into a node's input table."""
    params = {"format": "json"}
    if partition is not None:
        params["partition"] = str(partition)
    body = json.dumps({"delete" if delete else "insert": row})
    r = requests.post(url(base_port, node, f"/ingress/{table}"), params=params, data=body, timeout=5)
    r.raise_for_status()


def wait_until_up(base_port, node, timeout=60):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if requests.get(url(base_port, node, "/stats"), timeout=1).ok:
                return
        except requests.ConnectionError:
            pass
        time.sleep(0.2)
    raise SystemExit(f"node {node}: its server did not come up")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--id", type=int, required=True)
    ap.add_argument("--base-port", type=int, default=18200)
    ap.add_argument("--members", required=True, help="comma-separated node ids, this one included")
    ap.add_argument("--server-bin", required=True)
    ap.add_argument("--program", required=True, help="the compiled raft.gdbsp")
    ap.add_argument(
        "--config",
        default=str(__import__("pathlib").Path(__file__).resolve().parent / "pipeline.yaml"),
        help="the server's pipeline configuration",
    )
    ap.add_argument("--heartbeat-ms", type=int, default=150)
    ap.add_argument("--election-ms", default="600-1200", help="random election timeout range")
    ap.add_argument("--log", help="where the server's output goes; discarded if not given")
    args = ap.parse_args()

    me = args.id
    base = args.base_port
    lo, hi = (int(x) for x in args.election_ms.split("-"))

    log = open(args.log, "ab") if args.log else subprocess.DEVNULL
    server = subprocess.Popen(
        [args.server_bin, "serve", args.program, "--config-file", args.config, "--port", str(base + me)],
        stdout=log,
        stderr=log,
    )
    stopping = threading.Event()

    def stop(*_):
        stopping.set()
        server.terminate()
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()
        sys.exit(0)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    wait_until_up(base, me)
    push(base, me, "me", {"node": me})
    for m in args.members.split(","):
        push(base, me, "member", {"node": int(m)})

    ticks = itertools.count(1)
    ticks_lock = threading.Lock()

    def tick():
        with ticks_lock:
            return next(ticks)

    def inbox(kind, term=0):
        # Partition 0 is named explicitly: the program reads partition 0, and a
        # request that names none gets whatever the runtime chooses.
        push(base, me, "inbox", {"kind": kind, "from": me, "term": term, "tick": tick()}, partition=0)

    def timers():
        next_timeout = time.monotonic() + random.uniform(lo, hi) / 1000
        while not stopping.is_set():
            time.sleep(args.heartbeat_ms / 1000)
            try:
                inbox("hb_tick")
                if time.monotonic() >= next_timeout:
                    inbox("timeout")
                    next_timeout = time.monotonic() + random.uniform(lo, hi) / 1000
            except requests.RequestException:
                pass

    # Every row entering `send` is one message to deliver. `backpressure=true`
    # stalls this node's circuit rather than dropping a send when this loop
    # falls behind. Rows leaving `send` are ignored: a send cannot be undone.
    with requests.post(
        url(base, me, "/egress/send"),
        params={"format": "json", "backpressure": "true"},
        stream=True,
        timeout=None,
    ) as stream:
        # Only now: the subscription is open once the response has begun, so a
        # campaign the first timeout derives is not lost to a stream nobody has
        # opened yet.
        threading.Thread(target=timers, daemon=True).start()
        for line in stream.iter_lines():
            if stopping.is_set():
                break
            if not line:
                continue
            chunk = json.loads(line)
            for delta in chunk.get("json_data", []):
                row = delta.get("insert")
                if row is None:
                    continue
                message = {k: row[k] for k in ("kind", "from", "term", "tick")}
                try:
                    push(base, row["to"], "inbox", message, partition=0)
                except requests.RequestException:
                    # The recipient is gone, or not up yet. Raft tolerates a
                    # lost message; the timers will produce another.
                    pass


if __name__ == "__main__":
    main()
