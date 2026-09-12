//! Operator state larger than memory lives in a file, and says the same thing.
//!
//! `dbsp` writes a batch to storage rather than keeping it in memory when the
//! batch is bigger than a threshold, and the batch types this runner already
//! builds are the ones that can go either way. A file is not a different
//! answer: the whole claim of this file is that a program computes the same
//! relations, delta for delta, wherever its batches live.
//!
//! **Forced, not hoped for.** Setting both thresholds to zero sends every batch
//! to storage — `BuildTo::for_capacity` short-circuits before it consults a
//! size at all (`dbsp/src/trace/ord/fallback/utils.rs:41-71`) — so these tests
//! do not depend on how much memory the machine has or on when a background
//! merger happens to run.
//!
//! **And checked, not assumed.** Two agreeing runs prove nothing if storage
//! quietly failed to turn on, so every case also reads the backend's own byte
//! count and its file count. The bytes have to be sampled *after each step*
//! rather than at the end: a batch built during one step is dropped at the end
//! of it, and its file with it, so only a stateful operator — a join, a
//! distinct, an aggregate, a fixpoint — leaves anything to read across a step
//! boundary. The files are counted because that is the only evidence a batch
//! which spills and dies inside one step leaves behind at all, which is what
//! the last test here needs.
//!
//! What is not covered: spilling under real memory pressure, which needs
//! `max_rss_bytes` and a resident set this cannot control, and performance,
//! which these thresholds deliberately ruin.

use dbsp::ZWeight;
use dbsp::circuit::{CircuitStorageConfig, StorageCacheConfig, StorageConfig, StorageOptions};
use grasp_dbsp_runner::json::decode_value;
use grasp_dbsp_runner::lower::{Delta, Runner, RunnerConfig};
use grasp_dbsp_runner::value::BatchType;
use serde_json::{Value as J, json};
use tempfile::TempDir;

/// One transaction: `(table, row, weight)`.
type Epoch = Vec<(&'static str, J, ZWeight)>;

/// Held for the whole of any test that writes to storage.
///
/// `FILES_CREATED` is one counter for the process
/// (`dbsp/src/circuit/metrics.rs:17`), so two storage tests running at once
/// would each see the other's files — and the direction of that error is the
/// bad one, since a test could pass on a file it did not write. Serialising
/// them is cheaper than a per-backend counter that `dbsp` does not have.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn files_created() -> u64 {
    dbsp::circuit::metrics::FILES_CREATED.load(std::sync::atomic::Ordering::Relaxed)
}

/// A storage configuration in its own directory, forced to write everything.
///
/// The `TempDir` comes back with it and must outlive the `Runner`: the workers
/// are still writing into it until `kill` has joined them.
fn forced(step_threshold: usize) -> (TempDir, CircuitStorageConfig) {
    let dir = TempDir::new().expect("a directory to spill into");
    let config = CircuitStorageConfig::for_config(
        StorageConfig {
            path: dir.path().to_string_lossy().into_owned(),
            cache: StorageCacheConfig::default(),
        },
        StorageOptions {
            // The merge output, and a batch built during one step. The third
            // threshold — a batch entering a spine — is not settable and moves
            // only under memory pressure.
            min_storage_bytes: Some(0),
            min_step_storage_bytes: Some(step_threshold),
            ..StorageOptions::default()
        },
    )
    .expect("a storage backend");
    (dir, config)
}

/// What one run of a program produced, and what it wrote while producing it.
struct Ran {
    /// Every epoch's deltas, in order.
    deltas: Vec<Vec<(String, Vec<Delta>)>>,
    /// The most the backend ever had on disk, sampled after each step.
    peak: i64,
    /// How many files it wrote, whether or not they outlived a step.
    files: u64,
}

fn run(
    source: &str,
    outputs: &[&str],
    epochs: &[Epoch],
    storage: Option<&CircuitStorageConfig>,
) -> Ran {
    let plan = grasp_dbsp_runner::compile(source)
        .unwrap_or_else(|d| panic!("compiles: {}", grasp_dbsp_runner::diag::render(&d)));
    let usage = storage.map(|s| s.backend.usage());
    let files_before = files_created();
    let mut runner = Runner::build(
        &plan,
        &outputs.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
        RunnerConfig {
            storage: storage.cloned(),
            ..RunnerConfig::default()
        },
    )
    .unwrap_or_else(|d| panic!("builds: {}", grasp_dbsp_runner::diag::render(&d)));

    let mut produced = Vec::new();
    let mut peak = 0;
    for epoch in epochs {
        for (table, row, weight) in epoch {
            let idx = plan
                .inputs()
                .into_iter()
                .find(|(_, t)| t == table)
                .map(|(i, _)| i)
                .unwrap_or_else(|| panic!("an input table `{table}`"));
            let BatchType::ZSet(ty) = &plan.nodes[idx].ty else {
                panic!("`{table}` is indexed");
            };
            let value = decode_value(row, ty).expect("a row of the table's type");
            runner.push(table, value, *weight).expect("pushes");
        }
        produced.push(runner.step().expect("steps"));
        if let Some(usage) = &usage {
            peak = peak.max(usage.load(std::sync::atomic::Ordering::Relaxed));
        }
    }
    runner.kill();
    Ran {
        deltas: produced,
        peak,
        files: files_created() - files_before,
    }
}

/// The claim, for one program: same answers either way, and it really spilled.
fn agrees(what: &str, source: &str, outputs: &[&str], epochs: &[Epoch]) {
    let _lock = ONE_AT_A_TIME.lock().expect("the storage lock");

    let memory = run(source, outputs, epochs, None);
    assert_eq!(
        memory.peak, 0,
        "{what}: nothing is on disk with storage off"
    );
    assert_eq!(
        memory.files, 0,
        "{what}: a file was written with storage off"
    );

    let (dir, storage) = forced(0);
    let spilled = run(source, outputs, epochs, Some(&storage));
    drop(dir);

    assert_eq!(
        memory.deltas, spilled.deltas,
        "{what}: the answer changed when it went to disk"
    );
    assert!(
        memory
            .deltas
            .iter()
            .any(|e| e.iter().any(|(_, d)| !d.is_empty())),
        "{what}: neither run produced anything, so they agree about nothing"
    );
    assert!(
        spilled.peak > 0 && spilled.files > 0,
        "{what}: {} files and a peak of {} bytes, so this compared two runs in \
         memory",
        spilled.files,
        spilled.peak
    );
}

const KEYS: i64 = 40;

#[test]
fn a_join_and_a_distinct_agree() {
    let source = "\
l := input(\"l\")
l :: zset(record(k: i64, a: i64))
r := input(\"r\")
r :: zset(record(k: i64, b: i64))
li := map_index(l, function((v) -> record(key: v.k, value: record(a: v.a))))
ri := map_index(r, function((v) -> record(key: v.k, value: record(b: v.b))))
joined := join(li, ri, function((k, p, q) -> record(k: k, total: (p.a + q.b))))
uniq   := distinct(map(joined, function((v) -> record(bucket: (v.k % 5)))))
";
    let mut first: Epoch = Vec::new();
    for k in 0..KEYS {
        first.push(("l", json!({"k": k, "a": k * 10}), 1));
        first.push(("r", json!({"k": k, "b": k + 1}), 1));
    }
    let second: Epoch = first
        .iter()
        .map(|(t, row, _)| (*t, row.clone(), -1))
        .collect();
    agrees(
        "a join and a distinct",
        source,
        &["joined", "uniq"],
        &[first, second],
    );
}

#[test]
fn aggregates_agree() {
    let source = "\
t := input(\"t\")
t :: zset(record(g: i64, id: i64, v: i64))
idx     := map_index(t, function((r) -> record(key: r.g, value: r)))
total   := aggregate(idx, sum, function((v) -> v.v))
highest := aggregate(idx, max, function((v) -> v.v))
rows    := weighted_count(map(t, function((r) -> r.g)))
";
    let mut first: Epoch = Vec::new();
    for g in 0..KEYS {
        for id in 0..3 {
            first.push(("t", json!({"g": g, "id": id, "v": g * 10 + id}), 1));
        }
    }
    let second: Epoch = first
        .iter()
        .filter(|(_, row, _)| row["id"] == json!(1))
        .map(|(t, row, _)| (*t, row.clone(), -1))
        .collect();
    agrees(
        "sum, max and weighted_count",
        source,
        &["total", "highest", "rows"],
        &[first, second],
    );
}

#[test]
fn a_fixpoint_agrees() {
    let source = "\
edges := input(\"edges\")
edges :: zset(record(src: i64, dst: i64))
base  := map_index(edges, function((r) -> record(key: r.dst, value: record(src: r.src))))
fwd   := map_index(edges, function((r) -> record(key: r.src, value: record(dst: r.dst))))

circuit tc(base: b, fwd: f, path: p) {
    step := join_index(p, f, function((k, a, e) -> record(key: e.dst, value: record(src: a.src))))
    path := plus(b, step)
}

fp      := fixpoint(tc(base: base, fwd: fwd, path: empty()))
closure := fp.path
";
    let first: Epoch = (0..KEYS - 1)
        .map(|n| ("edges", json!({"src": n, "dst": n + 1}), 1))
        .collect();
    let second: Epoch = vec![("edges", json!({"src": KEYS / 2, "dst": KEYS / 2 + 1}), -1)];
    agrees("a fixpoint", source, &["closure"], &[first, second]);
}

/// Every value type goes out to a file and comes back.
///
/// This is where `tests/invariants.rs` stops and a circuit starts: that file
/// builds archived bytes by hand with `rkyv::to_bytes`, which proves nothing
/// about the file writer, the key comparisons a file cursor makes in archived
/// form, or the deserializer it reads back with. An `interval` is the sharp
/// case — `ShortInterval` panics unless handed `dbsp`'s own deserializer, which
/// is the one a spilled batch uses.
#[test]
fn every_value_type_survives_a_file() {
    let source = "\
t := input(\"t\")
t :: zset(record(
    b: bool, n: i64, f: f64, s: string,
    d: date, tm: time, ts: timestamp, iv: interval,
    by: bytes, doc: json, dyn: dynamic,
    arr: array(i64), dct: dict(string, i64), rec: record(inner: i64),
    opt: optional(i64)))
idx  := map_index(t, function((r) -> record(key: r, value: r)))
kept := distinct(map(idx, function((k, v) -> k)))
";
    let row = |n: i64| {
        json!({
            "b": n % 2 == 0, "n": n, "f": n as f64 + 0.5, "s": format!("s{n}"),
            "d": "2024-01-15", "tm": "14:30:00", "ts": "2024-01-15 14:30:00",
            "iv": "PT1H", "by": {"base64": "SGVsbG8="},
            "doc": {"k": n}, "dyn": {"i64": n},
            "arr": [n, n + 1], "dct": {"k": n}, "rec": {"inner": n},
            "opt": if n % 3 == 0 { J::Null } else { json!(n) },
        })
    };
    let first: Epoch = (0..KEYS).map(|n| ("t", row(n), 1)).collect();
    let second: Epoch = first
        .iter()
        .take(5)
        .map(|(t, r, _)| (*t, r.clone(), -1))
        .collect();
    agrees("every value type", source, &["kept"], &[first, second]);
}

/// A batch spills because of what its rows weigh, which is what `SizeOf` says.
///
/// The tests above set the threshold to zero, where no size is consulted at
/// all. This one leaves it at a megabyte, where the builder's capacity guess —
/// thirty-two bytes a row (`dbsp/src/trace/ord/fallback/utils.rs:56-68`) —
/// falls under it, so the builder starts in memory and accumulates
/// `key.size_of().total_bytes()` per row, spilling when the sum crosses
/// (`dbsp/src/trace/ord/fallback/wset.rs:562-578`). Two hundred rows of an
/// eight-kilobyte string cross it; the same two hundred rows reported as a bare
/// enum discriminant — which is what they weighed before `DynValue` had a
/// written `SizeOf` — come to 6 KiB and do not.
///
/// It is counted in files rather than in bytes on disk, because this batch is
/// transient: it is built, spilled, read and dropped inside one step, and the
/// backend's byte count is back to zero by the time a caller could read it.
/// Both runs are here so the assertion is a difference rather than a threshold:
/// the same program, the same row count, the same threshold, and nothing
/// between them but how much each row holds.
#[test]
fn a_batch_spills_on_what_its_rows_weigh() {
    let _lock = ONE_AT_A_TIME.lock().expect("the storage lock");

    let source = "\
t := input(\"t\")
t :: zset(record(id: i64, blob: string))
idx  := map_index(t, function((r) -> record(key: r.id, value: r)))
kept := distinct(map(idx, function((k, v) -> v)))
";
    let rows = |blob: &str| -> Epoch {
        (0..200)
            .map(|id| ("t", json!({"id": id, "blob": blob}), 1))
            .collect()
    };

    let heavy = rows(&"x".repeat(8 * 1024));
    let light = rows("x");

    let (dir, storage) = forced(1024 * 1024);
    let heavy_files = run(
        source,
        &["kept"],
        std::slice::from_ref(&heavy),
        Some(&storage),
    )
    .files;
    drop(dir);

    let (dir, storage) = forced(1024 * 1024);
    let light_files = run(
        source,
        &["kept"],
        std::slice::from_ref(&light),
        Some(&storage),
    )
    .files;
    drop(dir);

    assert_eq!(
        light_files, 0,
        "two hundred short rows are nowhere near a megabyte, and wrote {light_files} files"
    );
    assert!(
        heavy_files > 0,
        "1.6 MiB of rows did not reach a 1 MiB threshold: the rows are reporting less \
         than they hold, so `dbsp` sees a relation that never grows"
    );
}

/// A storage configuration naming an initial checkpoint is refused, not
/// half-honoured: this runner starts a circuit from nothing. Claimed by both
/// `overview.md` and `mapping.md`, and until now pinned by neither.
#[test]
fn a_configuration_naming_a_checkpoint_is_refused() {
    let _lock = ONE_AT_A_TIME.lock().expect("the storage lock");
    let (_dir, config) = forced(0);
    let config = config.with_init_checkpoint(Some(uuid::Uuid::nil()));
    let plan = grasp_dbsp_runner::compile("t := input(\"t\")\nt :: zset(record(v: i64))\n")
        .expect("compiles");
    let err = Runner::build(
        &plan,
        &["t".to_string()],
        RunnerConfig {
            storage: Some(config),
            ..RunnerConfig::default()
        },
    )
    .err()
    .expect("refused");
    let text = grasp_dbsp_runner::diag::render(&err);
    assert!(
        text.contains("naming an initial checkpoint"),
        "the refusal names the checkpoint: {text}"
    );
}
