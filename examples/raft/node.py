"""One Raft node: a grasp-dbsp-server running raft.gdbsp, and the plumbing
around it.

This file makes no Raft decisions. It starts the server, tells it who it is,
delivers every message the program derives to the recipient's Kafka topic, and
fires two timers into its own. Everything that decides anything is in
raft.grasp.

Messages travel through Kafka, not HTTP. Each node's inbox is a topic of its
own, `<prefix>-node-<id>`, with one partition, and the node's server reads it
into the `inbox` table: the offset a message takes in that partition is the
offset the program orders the node's history by. Membership is one topic every
node reads into `member`. The server has no Kafka output, so what a node sends
still leaves over HTTP, as rows entering the `send` view.

A node's server listens on base_port + id, and its topic is named by its id, so
any node can reach any other by id alone, including one that joined after it
started.
"""

import argparse
import itertools
import json
import pathlib
import random
import signal
import subprocess
import sys
import threading
import time

import requests
from kafka import KafkaProducer
from kafka.errors import KafkaError

HERE = pathlib.Path(__file__).resolve().parent
PIPELINE = "grasp"


def url(base_port, node, path):
    return f"http://127.0.0.1:{base_port + node}/v0/pipelines/{PIPELINE}{path}"


def inbox_topic(prefix, node):
    return f"{prefix}-node-{node}"


def members_topic(prefix):
    return f"{prefix}-members"


def config(brokers, prefix, node):
    """`pipeline.yaml`, and the two Kafka inputs that are this node's own.

    JSON is YAML, so the inputs are appended as JSON rather than needing a YAML
    library to write them.
    """

    def kafka(stream, topic):
        return {
            "stream": stream,
            "transport": {
                "name": "kafka_input",
                "config": {
                    "bootstrap.servers": brokers,
                    "topic": topic,
                    # A node's history is its topic from the start: a new node
                    # reads the messages sent to it before it was up, and the
                    # whole membership history.
                    "start_from": "earliest",
                },
            },
            "format": {"name": "json"},
        }

    inputs = {
        "inbox_kafka": kafka("inbox", inbox_topic(prefix, node)),
        "member_kafka": kafka("member", members_topic(prefix)),
    }
    return (HERE / "pipeline.yaml").read_text() + "\ninputs: " + json.dumps(inputs) + "\n"


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
    ap.add_argument("--brokers", default="127.0.0.1:9092", help="Kafka bootstrap servers")
    ap.add_argument("--topic-prefix", required=True, help="names this cluster's topics")
    ap.add_argument("--server-bin", required=True)
    ap.add_argument("--program", required=True, help="the compiled raft.gdbsp")
    ap.add_argument("--work", required=True, help="a directory for this node's configuration")
    ap.add_argument("--heartbeat-ms", type=int, default=150)
    ap.add_argument("--election-ms", default="600-1200", help="random election timeout range")
    ap.add_argument("--log", help="where the server's output goes; discarded if not given")
    args = ap.parse_args()

    me = args.id
    base = args.base_port
    lo, hi = (int(x) for x in args.election_ms.split("-"))

    config_file = pathlib.Path(args.work) / f"node-{me}.yaml"
    config_file.write_text(config(args.brokers, args.topic_prefix, me))

    log = open(args.log, "ab") if args.log else subprocess.DEVNULL
    server = subprocess.Popen(
        [args.server_bin, "serve", args.program, "--config-file", config_file, "--port", str(base + me)],
        stdout=log,
        stderr=log,
    )
    # `max_block_ms` bounds how long a send to a topic whose metadata is not in
    # hand may wait. No batching delay: a heartbeat that sits in a buffer is a
    # heartbeat that arrives late.
    producer = KafkaProducer(bootstrap_servers=args.brokers, linger_ms=0, max_block_ms=2000)
    stopping = threading.Event()

    def stop(*_):
        stopping.set()
        server.terminate()
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()
        producer.close(timeout=2)
        sys.exit(0)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    wait_until_up(base, me)
    # Who this node is: set once, by the node itself, so it stays over HTTP.
    requests.post(
        url(base, me, "/ingress/me"),
        params={"format": "json"},
        data=json.dumps({"insert": {"node": me}}),
        timeout=5,
    ).raise_for_status()

    ticks = itertools.count(1)
    ticks_lock = threading.Lock()

    def deliver(to, message):
        # Partition 0, named: the program reads partition 0. The offset Kafka
        # gives the message there is the order the recipient sees it in.
        body = json.dumps({"insert": message}).encode()
        producer.send(inbox_topic(args.topic_prefix, to), value=body, partition=0)

    def timer(kind):
        with ticks_lock:
            tick = next(ticks)
        deliver(me, {"kind": kind, "from": me, "term": 0, "tick": tick})

    def timers():
        next_timeout = time.monotonic() + random.uniform(lo, hi) / 1000
        while not stopping.is_set():
            time.sleep(args.heartbeat_ms / 1000)
            try:
                timer("hb_tick")
                if time.monotonic() >= next_timeout:
                    timer("timeout")
                    next_timeout = time.monotonic() + random.uniform(lo, hi) / 1000
            except KafkaError:
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
                    deliver(row["to"], message)
                except KafkaError:
                    # The recipient's topic is unreachable. Raft tolerates a
                    # lost message; the timers will produce another.
                    pass


if __name__ == "__main__":
    main()
