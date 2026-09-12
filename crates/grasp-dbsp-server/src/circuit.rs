//! The thread that owns the circuit, and the protocol every handler speaks to
//! it.
//!
//! [`Runner::step`] takes `&mut self`, so exactly one thread may drive a
//! circuit. That is not an artifact to work around: a transaction *is*
//! exclusive, and the `&mut` says so. So the `Runner` is owned outright by one
//! plain OS thread, and the HTTP handlers — which run on actix's tokio workers
//! — reach it only by sending commands.
//!
//! **Not an `RwLock`.** `push(&self)` and `step(&mut self)` fit a read/write
//! lock so exactly that it is the first thing anyone tries. Three reasons not
//! to, in the order they matter:
//!
//! 1. It makes the step a row lands in unknowable. A handler that takes the
//!    read lock, pushes and releases cannot say which transaction consumed its
//!    rows, because a writer may have been waiting the whole time. Completion
//!    tokens are then unanswerable, and a completion token is the entire
//!    contract `POST /ingress` offers its caller.
//! 2. A write lock held for a transaction blocks every pusher for its whole
//!    duration. A queue blocks them for the drain, and in the order their
//!    tokens were issued.
//! 3. It converts a statement about semantics into a fact about actix's thread
//!    pool.
//!
//! **Two channel libraries, deliberately.** Commands travel on an unbounded
//! `crossbeam-channel`, because the receiver is a plain thread with no runtime
//! in scope and because sending on an unbounded crossbeam channel never blocks
//! — which is what makes it callable from an async handler. Replies and chunks
//! travel on `tokio` channels, because the receiver there *is* async and a
//! blocking `recv` on a tokio worker stalls every other connection that worker
//! is multiplexing. Each library is on the side of the boundary it belongs to.

use crossbeam_channel::{Receiver, Sender};
use grasp_dbsp::lower::{Delta, Runner};
use grasp_dbsp::value::{BatchType, DynValue};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use tokio::sync::{mpsc, oneshot};

/// How many chunks a subscriber may fall behind before it starts losing them.
///
/// Feldera's number, and its behaviour: a hundred buffered chunks and then
/// `try_send` failures that are dropped on the floor
/// (`adapters/src/transport/http/output.rs:22-23`, `:152-158`). Matching it
/// matters because a client reconstructing state from a delta stream has to
/// know which way it loses — see [`Chunk::skipped`].
const QUEUE: usize = 100;

/// A transaction's output for one view, as it reaches one subscriber.
pub struct Chunk {
    /// The deltas, shared by every subscriber to this view rather than encoded
    /// once per connection on the circuit thread. Encoding is the expensive
    /// part and it depends on the *subscriber's* chosen format, so it happens
    /// in the connection's own task.
    pub deltas: Arc<Vec<Delta>>,
    /// How many chunks were dropped before this one.
    ///
    /// A lossy subscriber leaves a gap in its sequence numbers rather than
    /// silently renumbering, because Feldera does: it assigns the number before
    /// the send that may fail (`http/output.rs:97` then `:152`). A client that
    /// sees 7 followed by 9 has lost one and can say so.
    pub skipped: u64,
}

/// What a subscriber gets when it asks to watch a view.
pub struct Subscription {
    pub chunks: mpsc::Receiver<Chunk>,
    /// The view's full contents at the instant of subscription, for a
    /// materialized view asked for a snapshot. See [`Circuit::subscribe`].
    pub snapshot: Option<Arc<Rows>>,
    /// The view's shape, so the connection can encode without consulting the
    /// plan.
    pub shape: BatchType,
}

/// A view's contents: every live row and its weight.
///
/// `BTreeMap` rather than `HashMap` because `step` already delivers deltas in
/// key order, so a snapshot taken from this comes out in the same order as the
/// deltas that follow it — which makes a snapshot deterministic, and a
/// deterministic snapshot testable.
pub type Rows = BTreeMap<(DynValue, Option<DynValue>), dbsp::ZWeight>;

/// Why a command could not be carried out.
#[derive(Debug)]
pub enum Fault {
    /// The table or view is not in this program. `what` is the word for the
    /// kind of name, and it decides which of Feldera's two "unknown relation"
    /// error codes the client sees — so it travels with the fault rather than
    /// being guessed at the point the fault becomes a response.
    NoSuchName { what: &'static str, name: String },
    /// The circuit thread is gone: it panicked, or the server is stopping.
    /// Every handler turns this into 410, never into a bare "channel closed".
    Gone(String),
    /// The runner refused — a transaction already open, a commit with nothing
    /// to commit. The message is the runner's own.
    Refused(String),
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fault::NoSuchName { what, name } => {
                write!(f, "`{name}` is not a {what} of this program")
            }
            Fault::Gone(m) => write!(f, "the circuit is no longer running: {m}"),
            Fault::Refused(m) => f.write_str(m),
        }
    }
}

/// A command for the circuit thread. Every one carries the channel its answer
/// goes back on, so a handler awaits exactly its own reply.
pub enum Command {
    Push {
        table: String,
        rows: Vec<(DynValue, dbsp::ZWeight)>,
        reply: oneshot::Sender<Result<u64, Fault>>,
    },
    Subscribe {
        view: String,
        snapshot: bool,
        backpressure: bool,
        reply: oneshot::Sender<Result<Subscription, Fault>>,
    },
    SetRunning {
        running: bool,
        reply: oneshot::Sender<bool>,
    },
    BeginTransaction {
        reply: oneshot::Sender<Result<i64, Fault>>,
    },
    CommitTransaction {
        reply: oneshot::Sender<Result<(), Fault>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// State a handler may read without waiting for the circuit thread.
///
/// This is a correctness requirement rather than an optimisation. Committing a
/// transaction occupies the circuit thread for the whole commit, so `/stats`
/// and `/completion_status` must not queue behind it — a long transaction
/// would make the pipeline unobservable exactly when an operator wants to
/// watch it. Feldera splits the same way, for the same reason: its global
/// metrics are all atomics.
#[derive(Debug)]
pub struct Shared {
    /// Identifies this run. A completion token minted before a restart decodes
    /// fine and is refused, rather than being satisfied by a step number that
    /// happens to have come round again.
    pub incarnation: uuid::Uuid,
    pub running: AtomicBool,
    /// Transactions completed since startup. A token is complete when this
    /// reaches the step the token names.
    pub completed_steps: AtomicU64,
    /// Transactions begun, which exceeds the above by one while one is running.
    pub initiated_steps: AtomicU64,
    /// Rows pushed and not yet consumed by a transaction.
    pub buffered_input_records: AtomicU64,
    pub total_input_records: AtomicU64,
    /// The open explicit transaction's id, or zero.
    pub transaction_id: AtomicI64,
    pub transaction_open: AtomicBool,
    pub committing: AtomicBool,
    /// Set when the circuit thread stops for a reason other than being asked
    /// to. Read by every handler that finds its reply channel closed, so the
    /// answer is the cause rather than "the channel closed".
    pub fatal: std::sync::Mutex<Option<String>>,
    /// What a completion token is resolved against.
    ///
    /// Here rather than on the circuit thread because `/completion_status`
    /// must never queue behind a transaction: a long commit occupies the
    /// thread, and a client polling to find out whether its rows have landed
    /// is exactly the client that would be blocked. The critical sections are
    /// a few map operations.
    pub progress: std::sync::Mutex<Progress>,
}

/// How far ingestion has got, and what is still in flight.
#[derive(Debug, Default)]
pub struct Progress {
    /// Rows accepted, ever. A completion token names one of these counts.
    pub accepted: u64,
    /// Everything at or below this is complete, as of `done_step`.
    pub done_count: u64,
    pub done_step: u64,
    /// Accepted-count watermark to the transaction that will consume it. At
    /// most one entry, since at most one transaction is in flight.
    pub in_flight: BTreeMap<u64, u64>,
}

impl Progress {
    /// The transaction a token's watermark is waiting for, if any.
    ///
    /// `None` means the token names rows this run never accepted — which, for
    /// a token that decoded and carried the right incarnation, cannot happen.
    fn step_for(&self, count: u64) -> Option<u64> {
        if count <= self.done_count {
            Some(self.done_step)
        } else {
            self.in_flight.range(count..).next().map(|(_, s)| *s)
        }
    }
}

impl Shared {
    fn new() -> Shared {
        Shared {
            incarnation: uuid::Uuid::new_v4(),
            running: AtomicBool::new(false),
            completed_steps: AtomicU64::new(0),
            initiated_steps: AtomicU64::new(0),
            buffered_input_records: AtomicU64::new(0),
            total_input_records: AtomicU64::new(0),
            transaction_id: AtomicI64::new(0),
            transaction_open: AtomicBool::new(false),
            committing: AtomicBool::new(false),
            fatal: std::sync::Mutex::new(None),
            progress: std::sync::Mutex::new(Progress::default()),
        }
    }

    /// Why the circuit thread is gone, as far as anyone knows.
    pub fn why_gone(&self) -> String {
        self.fatal
            .lock()
            .ok()
            .and_then(|f| f.clone())
            .unwrap_or_else(|| "the server is stopping".to_string())
    }
}

/// The handle a handler holds: a channel in, and state it may read directly.
#[derive(Clone)]
pub struct Handle {
    commands: Sender<Command>,
    pub shared: Arc<Shared>,
    /// Every view, so a 404 can be answered without troubling the circuit
    /// thread — and so `/metadata` can list them.
    pub views: Vec<String>,
    pub tables: Vec<String>,
    pub materialized: Vec<String>,
    /// Every relation's shape, so a handler can decode a request body and
    /// encode a response without the plan.
    pub shapes: Arc<HashMap<String, BatchType>>,
}

impl Handle {
    /// Sends a command without waiting for its answer.
    ///
    /// Public so that a test can drive the circuit thread synchronously,
    /// without a tokio runtime: `tests/threading.rs` is the whole of the
    /// state-machine evidence and needs no async to be it.
    pub fn send(&self, command: Command) -> Result<(), Fault> {
        self.commands
            .send(command)
            .map_err(|_| Fault::Gone(self.shared.why_gone()))
    }

    /// Rows this pipeline has accepted, ever.
    pub fn accepted_now(&self) -> u64 {
        self.shared.progress.lock().map(|p| p.accepted).unwrap_or(0)
    }

    /// The transaction a completion token is waiting for.
    ///
    /// Reads `Shared` directly and never sends a command, so a client polling
    /// a token is answered even while a long transaction has the circuit
    /// thread to itself.
    pub fn step_for(&self, count: u64) -> Option<u64> {
        self.shared
            .progress
            .lock()
            .ok()
            .and_then(|p| p.step_for(count))
    }

    /// Sends a command and awaits its answer.
    ///
    /// Both halves can fail by the circuit thread having gone, and both are
    /// reported as [`Fault::Gone`] carrying whatever reason it left behind.
    pub async fn ask<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, Fault> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(make(tx))
            .map_err(|_| Fault::Gone(self.shared.why_gone()))?;
        rx.await.map_err(|_| Fault::Gone(self.shared.why_gone()))
    }
}

/// One subscriber's end of a view.
struct Subscriber {
    chunks: mpsc::Sender<Chunk>,
    /// Chunks dropped since the last one that got through.
    skipped: u64,
    /// Whether to stall the circuit rather than drop.
    backpressure: bool,
}

/// The circuit, its views, and the bookkeeping that answers completion tokens.
struct Circuit {
    runner: Runner,
    shared: Arc<Shared>,
    /// Every selected view's shape, resolved once at startup.
    shapes: HashMap<String, BatchType>,
    subscribers: HashMap<String, Vec<Subscriber>>,
    materialized: HashMap<String, Arc<Rows>>,
    /// Rows pushed since the last transaction, which is what decides whether
    /// there is anything to step for.
    pending: u64,
    next_transaction_id: i64,
}

/// Starts the circuit thread and returns the handle to it.
///
/// The `Runner` is built by the caller, on the caller's thread, and moved in —
/// so a program that does not compile, a configuration that is wrong or a
/// storage directory that is locked is a diagnostic before a port is bound
/// rather than a server that starts and cannot run. `tests/threads.rs` in the
/// runner is what says that move is sound.
pub fn start(
    runner: Runner,
    shapes: HashMap<String, BatchType>,
    materialized: &[String],
    tables: Vec<String>,
    running: bool,
) -> (Handle, std::thread::JoinHandle<()>) {
    let (commands, rx) = crossbeam_channel::unbounded();
    let shared = Arc::new(Shared::new());
    shared.running.store(running, Ordering::Relaxed);

    let mut views: Vec<String> = shapes.keys().cloned().collect();
    views.sort();
    let shapes_for_handle = Arc::new(shapes.clone());

    let circuit = Circuit {
        runner,
        shared: Arc::clone(&shared),
        subscribers: HashMap::new(),
        materialized: materialized
            .iter()
            .map(|v| (v.clone(), Arc::new(Rows::new())))
            .collect(),
        shapes,
        pending: 0,
        next_transaction_id: 1,
    };

    let thread = std::thread::Builder::new()
        .name("grasp-circuit".to_string())
        .spawn(move || circuit.run(rx))
        .expect("spawning the circuit thread");

    (
        Handle {
            commands,
            shared,
            views,
            tables,
            materialized: materialized.to_vec(),
            shapes: shapes_for_handle,
        },
        thread,
    )
}

impl Circuit {
    /// The loop.
    ///
    /// One blocking receive, then everything else already queued, and then at
    /// most one transaction. Draining before stepping is what makes a burst of
    /// ten ingress requests one transaction rather than ten — the batching
    /// Feldera spends a `max_buffering_delay_usecs` knob on, had for free
    /// because the queue is the buffer.
    ///
    /// **There is no timer here**, and that is deliberate: every wake-up is
    /// caused by a command, so the thread is idle when nothing is happening and
    /// a test driving it observes only what the test caused.
    fn run(mut self, commands: Receiver<Command>) {
        // A guard rather than a `catch_unwind`, because it runs during
        // unwinding and so records a panic anywhere in the loop, including
        // inside `dbsp`.
        let guard = FatalGuard {
            shared: Arc::clone(&self.shared),
            armed: true,
        };

        while let Ok(first) = commands.recv() {
            let mut deferred = Vec::new();
            if self.handle(first, &mut deferred) {
                break;
            }
            let mut stop = false;
            while let Ok(next) = commands.try_recv() {
                if self.handle(next, &mut deferred) {
                    stop = true;
                    break;
                }
            }

            let idle = !self.shared.transaction_open.load(Ordering::Relaxed);
            let failed = idle
                && self.shared.running.load(Ordering::Relaxed)
                && self.pending > 0
                && match self.transaction() {
                    Ok(()) => false,
                    Err(e) => {
                        self.fail(format!("running a transaction: {e}"));
                        true
                    }
                };

            // Only now. A caller waiting on one of these replies is entitled to
            // read `Shared` afterwards and see the transaction its command
            // caused — which is what makes a completion token usable as a
            // barrier, and what makes `tests/threading.rs` deterministic
            // without a sleep in it.
            for reply in deferred {
                reply();
            }
            if stop || failed {
                break;
            }
        }
        self.finish(guard)
    }

    fn finish(self, mut guard: FatalGuard) {
        guard.armed = false;
        self.runner.kill();
    }

    /// Handles one command. Answers `true` when the thread should stop.
    ///
    /// A command that can *cause* a transaction — a push, a resume — has its
    /// reply put in `deferred` rather than sent, because a caller that reads
    /// `Shared` the moment its reply arrives must see the transaction its own
    /// command caused. Commands that cannot cause one answer immediately, and
    /// a commit answers immediately because it has already done the work.
    fn handle(&mut self, command: Command, deferred: &mut Vec<Box<dyn FnOnce()>>) -> bool {
        match command {
            Command::Push { table, rows, reply } => {
                let answer = self.push(&table, rows);
                deferred.push(Box::new(move || {
                    let _ = reply.send(answer);
                }));
            }
            Command::Subscribe {
                view,
                snapshot,
                backpressure,
                reply,
            } => {
                let answer = self.subscribe(&view, snapshot, backpressure);
                let _ = reply.send(answer);
            }
            Command::SetRunning { running, reply } => {
                let was = self.shared.running.swap(running, Ordering::Relaxed);
                deferred.push(Box::new(move || {
                    let _ = reply.send(was);
                }));
            }
            Command::BeginTransaction { reply } => {
                let answer = match self.runner.begin_transaction() {
                    Ok(()) => {
                        let id = self.next_transaction_id;
                        self.next_transaction_id += 1;
                        self.shared.transaction_id.store(id, Ordering::Relaxed);
                        self.shared.transaction_open.store(true, Ordering::Relaxed);
                        Ok(id)
                    }
                    Err(e) => Err(Fault::Refused(e.to_string())),
                };
                let _ = reply.send(answer);
            }
            Command::CommitTransaction { reply } => {
                let answer = self.commit();
                let _ = reply.send(answer);
            }
            Command::Shutdown { reply } => {
                let _ = reply.send(());
                return true;
            }
        }
        false
    }

    fn push(&mut self, table: &str, rows: Vec<(DynValue, dbsp::ZWeight)>) -> Result<u64, Fault> {
        let count = rows.len() as u64;
        for (row, weight) in rows {
            self.runner
                .push(table, row, weight)
                .map_err(|_| Fault::NoSuchName {
                    what: "table",
                    name: table.to_string(),
                })?;
        }

        self.pending += count;
        self.shared
            .buffered_input_records
            .store(self.pending, Ordering::Relaxed);
        self.shared
            .total_input_records
            .fetch_add(count, Ordering::Relaxed);

        // The transaction these rows will be part of. This thread is the only
        // stepper, so there is no race: whatever is pushed now is consumed by
        // the next transaction, whose number is one past the last completed.
        let step = self.shared.completed_steps.load(Ordering::Relaxed) + 1;
        let mut progress = self.shared.progress.lock().expect("the progress lock");
        progress.accepted += count;
        let accepted = progress.accepted;
        progress.in_flight.insert(accepted, step);
        Ok(accepted)
    }

    /// Registers a subscriber, and takes its snapshot in the same turn.
    ///
    /// **This is the whole of the no-gap-no-duplicate guarantee**, and it rests
    /// on *where* the code runs rather than on a lock. Subscription is handled
    /// on the circuit thread, between transactions: transaction N's deltas are
    /// already folded into the materialized map, and transaction N+1 has not
    /// begun, because the loop steps only after the command queue is drained.
    /// So the snapshot is exactly the fold of transactions 1..=N and the
    /// channel receives exactly N+1 onwards.
    fn subscribe(
        &mut self,
        view: &str,
        snapshot: bool,
        backpressure: bool,
    ) -> Result<Subscription, Fault> {
        let shape = self
            .shapes
            .get(view)
            .cloned()
            .ok_or_else(|| Fault::NoSuchName {
                what: "view",
                name: view.to_string(),
            })?;

        let snapshot = if snapshot {
            match self.materialized.get(view) {
                Some(rows) => Some(Arc::clone(rows)),
                None => {
                    return Err(Fault::Refused(format!(
                        "`{view}` is not materialized, so there is no snapshot to send. \
                         This runner emits deltas and does not keep a relation's contents \
                         unless asked: add `{view}` to `materialized:` in the configuration."
                    )));
                }
            }
        } else {
            None
        };

        let (tx, rx) = mpsc::channel(QUEUE);
        self.subscribers
            .entry(view.to_string())
            .or_default()
            .push(Subscriber {
                chunks: tx,
                skipped: 0,
                backpressure,
            });

        Ok(Subscription {
            chunks: rx,
            snapshot,
            shape,
        })
    }

    fn commit(&mut self) -> Result<(), Fault> {
        if !self.shared.transaction_open.load(Ordering::Relaxed) {
            return Err(Fault::Refused(
                "no transaction is open to commit".to_string(),
            ));
        }
        self.shared.committing.store(true, Ordering::Relaxed);
        self.shared.initiated_steps.fetch_add(1, Ordering::Relaxed);

        let result = self.runner.commit_transaction();
        self.shared.committing.store(false, Ordering::Relaxed);
        self.shared.transaction_open.store(false, Ordering::Relaxed);
        self.shared.transaction_id.store(0, Ordering::Relaxed);

        match result {
            Ok(deltas) => {
                self.published(deltas);
                Ok(())
            }
            Err(e) => Err(Fault::Refused(e.to_string())),
        }
    }

    /// One whole transaction, in continuous mode.
    fn transaction(&mut self) -> Result<(), grasp_dbsp::lower::RunError> {
        self.shared.initiated_steps.fetch_add(1, Ordering::Relaxed);
        let deltas = self.runner.step()?;
        self.published(deltas);
        Ok(())
    }

    /// Everything that happens once a transaction's deltas are in hand.
    ///
    /// A completion token reporting `complete` implies the chunk is already in
    /// every subscriber's queue — Feldera's meaning of completion, and what
    /// lets a caller use a token as a barrier rather than polling and hoping.
    ///
    /// **That guarantee comes from the loop, not from the order here.** An
    /// earlier version of this comment claimed the order was load-bearing —
    /// counter last, subscribers first — and a mutation proved it was not:
    /// moving the increment to the top of this function broke no test, because
    /// the whole function runs before any reply is sent. It is the deferred
    /// reply that makes the transaction observable as one event. The order
    /// below is merely the order that reads correctly, and nothing rests on
    /// it.
    fn published(&mut self, deltas: Vec<(String, Vec<Delta>)>) {
        self.pending = 0;
        self.shared
            .buffered_input_records
            .store(0, Ordering::Relaxed);

        for (view, deltas) in deltas {
            if let Some(rows) = self.materialized.get_mut(&view) {
                let map = Arc::make_mut(rows);
                for d in &deltas {
                    let key = (d.key.clone(), d.value.clone());
                    let weight = map.entry(key).or_insert(0);
                    *weight += d.weight;
                    if *weight == 0 {
                        map.remove(&(d.key.clone(), d.value.clone()));
                    }
                }
            }

            let Some(subscribers) = self.subscribers.get_mut(&view) else {
                continue;
            };
            if deltas.is_empty() || subscribers.is_empty() {
                continue;
            }
            let deltas = Arc::new(deltas);
            subscribers.retain_mut(|s| {
                let chunk = Chunk {
                    deltas: Arc::clone(&deltas),
                    skipped: s.skipped,
                };
                if s.backpressure {
                    // Stalls the circuit, which is what `backpressure=true`
                    // promises. A closed channel is a departed client.
                    s.chunks.blocking_send(chunk).is_ok()
                } else {
                    match s.chunks.try_send(chunk) {
                        Ok(()) => {
                            s.skipped = 0;
                            true
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            s.skipped += 1;
                            true
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => false,
                    }
                }
            });
        }

        let step = self.shared.completed_steps.load(Ordering::Relaxed) + 1;
        if let Ok(mut progress) = self.shared.progress.lock() {
            while let Some((&count, &at)) = progress.in_flight.iter().next() {
                if at > step {
                    break;
                }
                progress.done_count = count;
                progress.done_step = at;
                progress.in_flight.remove(&count);
            }
        }
        self.shared.completed_steps.store(step, Ordering::Relaxed);
    }

    fn fail(&mut self, why: String) {
        if let Ok(mut fatal) = self.shared.fatal.lock() {
            *fatal = Some(why);
        }
        self.shared.running.store(false, Ordering::Relaxed);
    }
}

/// Records a panic on the circuit thread, so that handlers finding their reply
/// channel closed can say *why* rather than "the channel closed".
///
/// A guard rather than `catch_unwind` because it runs during unwinding and so
/// covers a panic anywhere in the loop, `dbsp`'s own code included. It is
/// disarmed on the ordinary path.
struct FatalGuard {
    shared: Arc<Shared>,
    armed: bool,
}

impl Drop for FatalGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(mut fatal) = self.shared.fatal.lock()
                && fatal.is_none()
            {
                *fatal = Some(
                    "the circuit thread panicked; the server is no longer computing".to_string(),
                );
            }
            self.shared.running.store(false, Ordering::Relaxed);
        }
    }
}

/// Whether a completion token's step has been reached.
///
/// Feldera's rule verbatim (`feldera-types/src/adapter_stats.rs:472-474`): a
/// record ingested when the counter was `n` is fully processed once
/// `total_completed_steps >= n`.
pub fn is_complete(shared: &Shared, step: u64) -> bool {
    shared.completed_steps.load(Ordering::Relaxed) >= step
}
