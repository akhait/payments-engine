//! The concurrent pipeline: one reader routes each transaction to the worker
//! that owns its client, so no account is shared between workers and every
//! client's transactions are applied in the order they arrived.
//!
//! The only state workers share is the refusal log (`RefusalLog`), which keeps
//! the log of refused transactions under one cap for the whole run.

use std::fmt;
use std::io::Read;
use std::num::NonZeroUsize;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};

use crate::account::Account;
use crate::engine::Engine;
use crate::pipeline::{ReadError, read_transactions};
use crate::refusals::RefusalLog;
use crate::transaction::{ClientId, Transaction};

/// The most workers that can be of any use: client ids are 16 bits wide, so no
/// transaction can belong to a shard past this many.
const MAX_SHARDS: usize = ClientId::MAX as usize + 1;

/// The most transactions a channel is allowed to hold, whatever is requested.
///
/// It holds the memory that a stalled worker can pin to a known amount per
/// channel.
const MAX_CAPACITY: usize = 1 << 20;

/// How many transactions the router hands to one worker in one message.
///
/// A message costs the same however many transactions it carries, and sending
/// one transaction at a time made the pipeline slower than a single thread. A
/// smaller channel limit than this gets a smaller batch.
const BATCH_SIZE: usize = 256;

/// What the router sends to a worker: some of its transactions, in input order.
type Batch = Vec<Transaction>;

/// How many transactions the router routes, whether it has sent them yet or
/// still holds them in a buffer, between two checks that no worker is gone. Rows
/// skipped as bad are not routed and do not count.
///
/// A worker that is gone shows when the router sends a batch to it. A shard the
/// input no longer names is never sent to, and a shard that is named only now
/// and then fills a batch slowly, so without this check the router could read
/// on for a long time before it noticed.
const LIVENESS_CHECK_INTERVAL: usize = 1024;

/// The pipeline could not produce the accounts.
#[derive(Debug)]
pub enum PipelineError {
    /// The input failed, held a row that was too long, or had a bad header.
    Read(ReadError),
    /// A worker task panicked, so its clients' accounts are lost.
    WorkerPanicked,
    /// The task that reads and routes the input panicked.
    RouterPanicked,
    /// A task ended before the input did, without panicking: a worker went
    /// away, or the runtime was shutting down and cancelled it.
    Stopped,
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => error.fmt(f),
            Self::WorkerPanicked => f.write_str("a worker task panicked"),
            Self::RouterPanicked => f.write_str("the task reading the input panicked"),
            Self::Stopped => f.write_str("the pipeline was stopped before it finished"),
        }
    }
}

impl std::error::Error for PipelineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::WorkerPanicked | Self::RouterPanicked | Self::Stopped => None,
        }
    }
}

/// Why routing stopped early.
enum RouteError {
    Read(ReadError),
    WorkerGone,
}

/// The shard that owns `client`, out of `shards`.
fn shard_of(client: ClientId, shards: usize) -> usize {
    usize::from(client) % shards
}

/// How a channel limit of `capacity` transactions (at least 1) is spent: the
/// size of a batch, and how many batches the channel holds.
///
/// The channel never holds more than `capacity` transactions, and a limit
/// smaller than [`BATCH_SIZE`] gets a batch of that size, so that a limit of 1
/// still means one transaction at a time.
fn channel_shape(capacity: usize) -> (usize, usize) {
    debug_assert!(capacity >= 1, "a channel holds at least one transaction");
    let batch = capacity.min(BATCH_SIZE);
    (batch, capacity / batch)
}

/// [`channel_shape`] for a requested limit, lowered to [`MAX_CAPACITY`].
fn shape_for(capacity: NonZeroUsize) -> (usize, usize) {
    channel_shape(capacity.get().min(MAX_CAPACITY))
}

/// Sends every transaction to the shard that owns its client, in batches of
/// `batch` (at least 1).
///
/// A shard's transactions are collected until there are `batch` of them, and
/// then sent as one message, so the order of a client's transactions is kept.
/// What is left over when the input ends is sent last. Blocks while a channel
/// is full, which holds the reader back. Meant to run on a thread that may
/// block, not on an async worker. `senders` must not be empty.
///
/// Stops with [`RouteError::WorkerGone`] as soon as a send fails, and also
/// when a periodic check finds that some worker has dropped its inbox. When the
/// input fails to read, the transactions read before the error are still sent,
/// so the workers see everything that was read; a worker that is gone, or a
/// panic in the input, skips that. If the input fails and a worker turns out to
/// be gone while the rest is sent, the worker is what is reported, as in the
/// pipeline, where a failed worker outranks the outcome of the reader.
fn route<I>(rows: I, senders: &[mpsc::Sender<Batch>], batch: usize) -> Result<(), RouteError>
where
    I: Iterator<Item = Result<Transaction, ReadError>>,
{
    debug_assert!(batch >= 1, "a batch holds at least one transaction");
    let mut buffers = vec![Batch::new(); senders.len()];
    let routed = route_rows(rows, senders, batch, &mut buffers);
    if matches!(routed, Err(RouteError::WorkerGone)) {
        return routed;
    }
    // Sending a failed worker's batch is what reveals it, so its error goes
    // first.
    flush(senders, &mut buffers).and(routed)
}

/// The loop of [`route`]: collects the transactions of each shard into
/// `buffers` and sends a buffer when it is full.
fn route_rows<I>(
    rows: I,
    senders: &[mpsc::Sender<Batch>],
    batch: usize,
    buffers: &mut [Batch],
) -> Result<(), RouteError>
where
    I: Iterator<Item = Result<Transaction, ReadError>>,
{
    let mut since_check = 0;
    for row in rows {
        let transaction = row.map_err(RouteError::Read)?;
        let shard = shard_of(transaction.client(), senders.len());
        let buffer = &mut buffers[shard];
        // A buffer grows as rows arrive, so a shard that gets few rows does not
        // hold room for a whole batch: with very many shards that would be far
        // more memory than the input. Once a shard has filled a batch it is busy,
        // and its next buffer is sized for a full batch.
        buffer.push(transaction);
        if buffer.len() >= batch {
            let full = std::mem::replace(buffer, Vec::with_capacity(batch));
            senders[shard]
                .blocking_send(full)
                .map_err(|_| RouteError::WorkerGone)?;
        }
        since_check += 1;
        if since_check == LIVENESS_CHECK_INTERVAL {
            since_check = 0;
            if senders.iter().any(mpsc::Sender::is_closed) {
                return Err(RouteError::WorkerGone);
            }
        }
    }
    Ok(())
}

/// Sends what is left in the buffers, leaving them empty.
fn flush(senders: &[mpsc::Sender<Batch>], buffers: &mut [Batch]) -> Result<(), RouteError> {
    let mut result = Ok(());
    for (sender, buffer) in senders.iter().zip(buffers) {
        if !buffer.is_empty() && sender.blocking_send(std::mem::take(buffer)).is_err() {
            result = Err(RouteError::WorkerGone);
        }
    }
    result
}

/// Applies the transactions it receives to its own engine until the channel
/// closes, then hands the engine back.
async fn worker(mut inbox: mpsc::Receiver<Batch>, refusals: Arc<RefusalLog>) -> Engine {
    let mut engine = Engine::default();
    while let Some(batch) = inbox.recv().await {
        for transaction in batch {
            if let Err(reason) = engine.apply(transaction) {
                refusals.record(&transaction, reason);
            }
        }
    }
    engine
}

/// Waits for every worker, even after one has failed, and hands back their
/// engines if all of them finished.
///
/// A panic is reported as such even if another worker was merely stopped.
async fn collect_workers(workers: Vec<JoinHandle<Engine>>) -> Result<Vec<Engine>, PipelineError> {
    let mut engines = Vec::with_capacity(workers.len());
    let mut panicked = false;
    let mut stopped = false;
    for worker in workers {
        match worker.await {
            Ok(engine) => engines.push(engine),
            Err(error) if error.is_panic() => panicked = true,
            Err(_) => stopped = true,
        }
    }
    if panicked {
        Err(PipelineError::WorkerPanicked)
    } else if stopped {
        Err(PipelineError::Stopped)
    } else {
        Ok(engines)
    }
}

fn spawn_worker_task(
    inbox: mpsc::Receiver<Batch>,
    refusals: Arc<RefusalLog>,
) -> JoinHandle<Engine> {
    tokio::spawn(worker(inbox, refusals))
}

/// Stops the workers when it is dropped.
///
/// Without it, a pipeline future that is dropped before it finishes (a timeout,
/// a `select!`) would leave the workers running. Stopping them closes their
/// inboxes, which in turn stops the router at its next send.
struct AbortOnDrop(Vec<AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

/// Applies the transactions with `shards` workers and returns every account,
/// in no particular order.
///
/// Each channel holds at most `capacity` transactions, so a slow worker slows
/// the reader down instead of letting memory grow. Transactions travel in
/// batches of up to 256 (fewer when `capacity` is smaller), and the reader holds
/// one more batch per shard while it fills it or waits to send it. `shards` is
/// lowered to 65,536, which is as many as client ids can use, and `capacity` to
/// 1,048,576.
///
/// `rows` is consumed on a thread that is allowed to block. To read from a
/// reader, use [`run_pipeline_from_reader`], which also opens the input there.
///
/// # Errors
///
/// - [`PipelineError::Read`] if `rows` yields an error. Nothing is returned for
///   the rows before it.
/// - [`PipelineError::WorkerPanicked`] or [`PipelineError::RouterPanicked`] if
///   a task panicked.
/// - [`PipelineError::Stopped`] if a task ended without a panic before the
///   input did.
///
/// When several of these happen, a worker panic is reported first, then a
/// worker that was stopped, then the outcome of the router (a panic, a stopped
/// worker it found, or the read error). A failed worker is the likelier cause
/// of the rest.
///
/// # Runtime
///
/// Must be awaited inside a Tokio runtime. The workers run as tasks on it and
/// the input is read by `spawn_blocking`, which draws on a finite pool of
/// threads: a reader that is stuck inside `read` holds one until the read
/// returns, and the runtime waits for it when it shuts down.
///
/// # Cancellation
///
/// Dropping the future stops the workers and, through them, the reader at its
/// next send. It cannot interrupt a reader that is inside `read`. The same
/// holds for [`run_pipeline_from_reader`].
pub async fn run_pipeline<I>(
    rows: I,
    shards: NonZeroUsize,
    capacity: NonZeroUsize,
) -> Result<Vec<(ClientId, Account)>, PipelineError>
where
    I: Iterator<Item = Result<Transaction, ReadError>> + Send + 'static,
{
    run_with_workers(move || Ok(rows), shards, capacity, spawn_worker_task).await
}

/// Like [`run_pipeline`], for CSV read from `reader`.
///
/// The header is read on the blocking thread too, so a reader that is slow to
/// deliver it does not hold up the async threads. Its errors, and those of the
/// rows, are [`PipelineError::Read`].
pub async fn run_pipeline_from_reader<R>(
    reader: R,
    shards: NonZeroUsize,
    capacity: NonZeroUsize,
) -> Result<Vec<(ClientId, Account)>, PipelineError>
where
    R: Read + Send + 'static,
{
    run_with_workers(
        move || read_transactions(reader),
        shards,
        capacity,
        spawn_worker_task,
    )
    .await
}

/// `run_pipeline` with the creation of each worker left to `spawn_worker`, so
/// that tests can put in a worker that is slow or that fails, and with the
/// input produced by `open` on the blocking thread that will read it.
async fn run_with_workers<I, O, F>(
    open: O,
    shards: NonZeroUsize,
    capacity: NonZeroUsize,
    spawn_worker: F,
) -> Result<Vec<(ClientId, Account)>, PipelineError>
where
    I: Iterator<Item = Result<Transaction, ReadError>>,
    O: FnOnce() -> Result<I, ReadError> + Send + 'static,
    F: Fn(mpsc::Receiver<Batch>, Arc<RefusalLog>) -> JoinHandle<Engine>,
{
    let shards = shards.get().min(MAX_SHARDS);
    let (batch, slots) = shape_for(capacity);
    let refusals = Arc::new(RefusalLog::default());
    let mut senders = Vec::with_capacity(shards);
    let mut workers = Vec::with_capacity(shards);
    for _ in 0..shards {
        let (sender, inbox) = mpsc::channel(slots);
        senders.push(sender);
        workers.push(spawn_worker(inbox, Arc::clone(&refusals)));
    }
    // Held until this function returns, whichever way that happens, including
    // by the future being dropped at one of the awaits below.
    let _stop_workers = AbortOnDrop(workers.iter().map(JoinHandle::abort_handle).collect());

    // The senders move into the router and are dropped when it ends, however
    // it ends. That tells the workers to finish what they were sent and stop.
    let routed = tokio::task::spawn_blocking(move || {
        let rows = open().map_err(RouteError::Read)?;
        route(rows, &senders, batch)
    })
    .await;
    let engines = collect_workers(workers).await;
    refusals.log_summary();

    // A worker that panicked is the likely reason routing stopped, so it is
    // reported before the router's own outcome.
    let engines = engines?;
    match routed {
        Err(error) if error.is_panic() => Err(PipelineError::RouterPanicked),
        Err(_) => Err(PipelineError::Stopped),
        Ok(Err(RouteError::Read(error))) => Err(PipelineError::Read(error)),
        Ok(Err(RouteError::WorkerGone)) => Err(PipelineError::Stopped),
        Ok(Ok(())) => Ok(engines
            .into_iter()
            .flat_map(Engine::into_accounts)
            .collect()),
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::amount::Amount;
    use crate::test_support::{a_read_error, capture_logs, nz};

    fn withdrawal(client: ClientId, tx: u32, units: i64) -> Transaction {
        Transaction::Withdrawal {
            client,
            tx,
            amount: Amount::from_units(units),
        }
    }

    fn deposit(client: ClientId, tx: u32) -> Transaction {
        Transaction::Deposit {
            client,
            tx,
            amount: Amount::from_units(10_000),
        }
    }

    /// Input that is already built, in the form `run_with_workers` takes.
    fn opened<I>(rows: I) -> impl FnOnce() -> Result<I, ReadError> + Send + 'static
    where
        I: Send + 'static,
    {
        move || Ok(rows)
    }

    /// The batches that reached an inbox, once the router has finished with it.
    fn batches_in(inbox: &mut mpsc::Receiver<Batch>) -> Vec<Batch> {
        let mut batches = Vec::new();
        while let Some(batch) = inbox.blocking_recv() {
            batches.push(batch);
        }
        batches
    }

    /// Runs a future to completion on this thread. Everything the pipeline
    /// spawns then runs on this thread as well, so its logs can be captured.
    fn block_on_current_thread<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime")
            .block_on(future)
    }

    /// Fails the test instead of hanging it if a shutdown ever stops working.
    async fn within<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(30), future)
            .await
            .expect("the pipeline finished within 30 seconds")
    }

    /// A reader that blocks in `read` until released, like a peer that has
    /// connected and sent nothing.
    struct Stalled {
        release: std::sync::mpsc::Receiver<()>,
        released: bool,
        data: Cursor<Vec<u8>>,
    }

    impl Read for Stalled {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.released {
                self.release
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|_| std::io::Error::other("the reader was never released"))?;
                self.released = true;
            }
            self.data.read(buf)
        }
    }

    #[test]
    fn a_client_goes_to_the_shard_given_by_its_id_modulo_the_shard_count() {
        for shards in [1, 2, 3, 8, 64, 65_536] {
            for client in [0, 1, 7, 255, 1_000, 65_535] {
                assert_eq!(
                    shard_of(client, shards),
                    usize::from(client) % shards,
                    "client {client}, {shards} shards"
                );
            }
        }
        let used: std::collections::BTreeSet<usize> =
            (0..8).map(|client| shard_of(client, 8)).collect();
        assert_eq!(used.len(), 8);
    }

    #[test]
    fn route_sends_each_transaction_to_the_shard_of_its_client_in_input_order() {
        let rows: Vec<Result<Transaction, ReadError>> = (0..30u32)
            .map(|n| Ok(deposit((n % 6) as u16, n + 1)))
            .collect();
        let (senders, mut inboxes): (Vec<_>, Vec<_>) = (0..3).map(|_| mpsc::channel(64)).unzip();
        assert!(route(rows.into_iter(), &senders, 4).is_ok());
        drop(senders);
        for (shard, inbox) in inboxes.iter_mut().enumerate() {
            let seen: Vec<(ClientId, u32)> = batches_in(inbox)
                .into_iter()
                .flatten()
                .map(|transaction| (transaction.client(), transaction.tx()))
                .collect();
            assert!(!seen.is_empty(), "shard {shard} got nothing");
            assert!(
                seen.iter()
                    .all(|(client, _)| usize::from(*client) % 3 == shard),
                "shard {shard} got a foreign client: {seen:?}"
            );
            assert!(
                seen.windows(2).all(|pair| pair[0].1 < pair[1].1),
                "shard {shard} out of order: {seen:?}"
            );
        }
    }

    #[test]
    fn route_sends_full_batches_and_flushes_the_rest_at_the_end() {
        let rows = (1..=10u32).map(|tx| Ok(deposit(1, tx)));
        let (sender, mut inbox) = mpsc::channel(8);
        assert!(route(rows, std::slice::from_ref(&sender), 4).is_ok());
        drop(sender);
        let lengths: Vec<usize> = batches_in(&mut inbox).iter().map(Vec::len).collect();
        assert_eq!(lengths, [4, 4, 2]);
    }

    #[test]
    fn route_sends_nothing_for_an_empty_input_and_no_empty_batch_at_the_end() {
        let (sender, mut inbox) = mpsc::channel(8);
        assert!(route(std::iter::empty(), std::slice::from_ref(&sender), 4).is_ok());
        let rows = (1..=8u32).map(|tx| Ok(deposit(1, tx)));
        assert!(route(rows, std::slice::from_ref(&sender), 4).is_ok());
        drop(sender);
        let lengths: Vec<usize> = batches_in(&mut inbox).iter().map(Vec::len).collect();
        assert_eq!(lengths, [4, 4]);
    }

    #[test]
    fn route_sends_a_batch_as_soon_as_it_is_full() {
        // One slot and a batch of 3: the first batch goes out when the third
        // row arrives, and the router then blocks on the second, so the input is
        // read exactly up to the sixth row.
        let produced = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&produced);
        let rows = (1..=100u32).map(move |tx| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(deposit(1, tx))
        });
        let (sender, mut inbox) = mpsc::channel(1);
        let router = std::thread::spawn(move || route(rows, &[sender], 3).is_ok());
        let started = std::time::Instant::now();
        while produced.load(Ordering::SeqCst) < 6 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the router never got to the second batch"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(produced.load(Ordering::SeqCst), 6);
        let received: usize = batches_in(&mut inbox).iter().map(Vec::len).sum();
        assert_eq!(received, 100);
        assert!(router.join().expect("the router thread"));
    }

    #[test]
    fn route_stops_at_the_first_error_and_reports_it_after_sending_what_it_had_read() {
        let rows = vec![Ok(deposit(1, 1)), Err(a_read_error()), Ok(deposit(1, 2))];
        let (sender, mut inbox) = mpsc::channel(8);
        // The batch size is larger than the number of rows read before the error,
        // so the row before it is still waiting in the router's buffer.
        let result = route(rows.into_iter(), std::slice::from_ref(&sender), 4);
        assert!(matches!(result, Err(RouteError::Read(_))));
        drop(sender);
        let received: Vec<u32> = batches_in(&mut inbox)
            .into_iter()
            .flatten()
            .map(|transaction| transaction.tx())
            .collect();
        assert_eq!(received, [1]);
    }

    #[test]
    fn a_worker_that_is_gone_is_reported_before_a_read_error() {
        // The row before the error is still in the router's buffer when the
        // input fails, and sending it shows that the worker is gone. As in the
        // pipeline, a failed worker outranks what the reader found.
        let rows = vec![Ok(deposit(1, 1)), Err(a_read_error())];
        let (sender, inbox) = mpsc::channel(8);
        drop(inbox);
        let result = route(rows.into_iter(), &[sender], 4);
        assert!(matches!(result, Err(RouteError::WorkerGone)));
    }

    #[test]
    fn a_shard_that_has_barely_been_used_does_not_hold_a_full_batch_of_memory() {
        // With many shards, most of them may see a few rows at most, and a buffer
        // of a whole batch for each would be far more memory than the input.
        let (senders, _inboxes): (Vec<_>, Vec<_>) = (0..8).map(|_| mpsc::channel(4)).unzip();
        let mut buffers = vec![Batch::new(); 8];
        let rows = (0..8u16).map(|client| Ok(deposit(client, u32::from(client) + 1)));
        assert!(route_rows(rows, &senders, BATCH_SIZE, &mut buffers).is_ok());
        for (shard, buffer) in buffers.iter().enumerate() {
            assert_eq!(buffer.len(), 1, "shard {shard}");
            assert!(
                buffer.capacity() < BATCH_SIZE,
                "shard {shard} holds room for {} rows for one row",
                buffer.capacity()
            );
        }
    }

    #[test]
    fn a_shard_that_has_filled_a_batch_keeps_room_for_the_next() {
        // A batch of 10 is larger than the smallest buffer a Vec allocates, so a
        // buffer that started from nothing again would show.
        let (sender, _inbox) = mpsc::channel(4);
        let mut buffers = vec![Batch::new()];
        let rows = (1..=11u32).map(|tx| Ok(deposit(1, tx)));
        assert!(route_rows(rows, std::slice::from_ref(&sender), 10, &mut buffers).is_ok());
        assert_eq!(buffers[0].len(), 1);
        assert!(buffers[0].capacity() >= 10, "the next batch would regrow");
    }

    #[test]
    fn route_reports_a_worker_that_is_gone_when_it_sends_the_last_batch() {
        let (sender, inbox) = mpsc::channel(8);
        drop(inbox);
        let produced = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&produced);
        let rows = (1..=3u32).map(move |tx| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(deposit(1, tx))
        });
        let result = route(rows, &[sender], 8);
        assert!(matches!(result, Err(RouteError::WorkerGone)));
        assert_eq!(produced.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn route_stops_at_the_next_full_batch_when_the_worker_of_the_shard_is_gone() {
        for batch in [1, 4, BATCH_SIZE] {
            let (sender, inbox) = mpsc::channel(8);
            drop(inbox);
            let produced = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&produced);
            let rows = (1..=1_000u32).map(move |tx| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(deposit(1, tx))
            });
            let result = route(rows, &[sender], batch);
            assert!(matches!(result, Err(RouteError::WorkerGone)));
            assert_eq!(produced.load(Ordering::SeqCst), batch, "batch {batch}");
        }
    }

    #[test]
    fn route_notices_a_worker_that_goes_away_on_a_shard_the_input_no_longer_names() {
        // Every row belongs to shard 1, so shard 0 is never sent to. Its worker
        // goes away while the input is being read, and only the periodic check
        // can find out. The check runs after every LIVENESS_CHECK_INTERVAL rows
        // routed, whatever the size of a batch.
        for batch in [1, 4, BATCH_SIZE] {
            for (dies_at_row, noticed_at_row) in [
                (10, LIVENESS_CHECK_INTERVAL),
                (LIVENESS_CHECK_INTERVAL + 10, 2 * LIVENESS_CHECK_INTERVAL),
            ] {
                let (dead, gone) = mpsc::channel(8);
                let mut doomed = Some(gone);
                let (live, _inbox) = mpsc::channel(100_000);
                let produced = Arc::new(AtomicUsize::new(0));
                let counter = Arc::clone(&produced);
                let rows = (1..=50_000u32).map(move |tx| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    if tx as usize == dies_at_row {
                        doomed.take();
                    }
                    Ok(deposit(1, tx))
                });
                let result = route(rows, &[dead, live], batch);
                assert!(matches!(result, Err(RouteError::WorkerGone)));
                assert_eq!(
                    produced.load(Ordering::SeqCst),
                    noticed_at_row,
                    "batch {batch}: the worker went away at row {dies_at_row}"
                );
            }
        }
    }

    #[test]
    fn a_full_channel_holds_the_reader_back() {
        let produced = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&produced);
        let rows = (1..=100u32).map(move |tx| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(deposit(1, tx))
        });
        let (sender, mut inbox) = mpsc::channel(2);
        let router = std::thread::spawn(move || route(rows, &[sender], 1).is_ok());

        // Two transactions fit in the channel and a third has been taken from
        // the input and is waiting to be sent: the reader cannot go further.
        let started = std::time::Instant::now();
        while produced.load(Ordering::SeqCst) < 3 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the reader never started"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(produced.load(Ordering::SeqCst), 3);

        let received: usize = batches_in(&mut inbox).iter().map(Vec::len).sum();
        assert_eq!(received, 100);
        assert!(router.join().expect("the router thread"));
    }

    #[test]
    fn the_limits_are_the_documented_ones() {
        assert_eq!(BATCH_SIZE, 256);
        assert_eq!(LIVENESS_CHECK_INTERVAL, 1024);
        assert_eq!(MAX_SHARDS, 65_536);
        assert_eq!(MAX_CAPACITY, 1_048_576);
    }

    #[test]
    fn a_channel_limit_is_split_into_batches_and_slots_that_never_exceed_it() {
        let cases = [
            (1, (1, 1)),
            (2, (2, 1)),
            (3, (3, 1)),
            (10, (10, 1)),
            (255, (255, 1)),
            (256, (256, 1)),
            (257, (256, 1)),
            (511, (256, 1)),
            (512, (256, 2)),
            (1000, (256, 3)),
            (1024, (256, 4)),
            (MAX_CAPACITY, (256, 4096)),
        ];
        for (capacity, expected) in cases {
            assert_eq!(channel_shape(capacity), expected, "capacity {capacity}");
        }
        for capacity in 1..=4096 {
            let (batch, slots) = channel_shape(capacity);
            assert!(batch >= 1 && slots >= 1, "capacity {capacity}");
            assert!(batch * slots <= capacity, "capacity {capacity}");
            assert!(
                batch * (slots + 1) > capacity,
                "capacity {capacity}: a slot is wasted"
            );
        }
    }

    #[test]
    fn a_requested_capacity_beyond_the_maximum_is_lowered_to_it() {
        assert_eq!(shape_for(nz(1024)), (256, 4));
        assert_eq!(shape_for(nz(MAX_CAPACITY)), (256, 4096));
        assert_eq!(shape_for(NonZeroUsize::MAX), (256, 4096));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_workers_hands_back_every_engine_when_none_panicked() {
        let workers = vec![
            tokio::spawn(async { Engine::default() }),
            tokio::spawn(async { Engine::default() }),
        ];
        let engines = collect_workers(workers).await.expect("no worker panicked");
        assert_eq!(engines.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_workers_reports_a_panicking_worker_after_waiting_for_the_others() {
        let finished = Arc::new(AtomicUsize::new(0));
        let slow = {
            let finished = Arc::clone(&finished);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                finished.fetch_add(1, Ordering::SeqCst);
                Engine::default()
            })
        };
        let failing = tokio::spawn(async { panic!("a worker failed on purpose") });
        let result = collect_workers(vec![failing, slow]).await;
        assert!(matches!(result, Err(PipelineError::WorkerPanicked)));
        assert_eq!(
            finished.load(Ordering::SeqCst),
            1,
            "the slow worker was not awaited"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_workers_calls_a_stopped_worker_stopped_and_a_panic_a_panic() {
        let stopped = || {
            let handle: JoinHandle<Engine> = tokio::spawn(async {
                loop {
                    tokio::task::yield_now().await;
                }
            });
            handle.abort();
            handle
        };
        let result = collect_workers(vec![stopped()]).await;
        assert!(matches!(result, Err(PipelineError::Stopped)), "{result:?}");

        let failing: JoinHandle<Engine> = tokio::spawn(async { panic!("a worker failed") });
        let result = collect_workers(vec![stopped(), failing, stopped()]).await;
        assert!(
            matches!(result, Err(PipelineError::WorkerPanicked)),
            "{result:?}"
        );
    }

    #[test]
    fn a_worker_applies_what_it_receives_logs_what_it_refuses_and_hands_back_its_engine() {
        let refusals = Arc::new(RefusalLog::default());
        let (engine, logs) = capture_logs(|| {
            block_on_current_thread(async {
                let (sender, inbox) = mpsc::channel(8);
                // Two batches, so that the worker is shown to go on after the first.
                sender
                    .send(vec![deposit(1, 1)])
                    .await
                    .expect("the worker listens");
                sender
                    .send(vec![withdrawal(1, 2, 15_000), deposit(1, 3)])
                    .await
                    .expect("the worker listens");
                drop(sender);
                worker(inbox, Arc::clone(&refusals)).await
            })
        });
        let account = engine.account(1).expect("client 1 has an account");
        // The withdrawal is refused only if it is applied before the deposit
        // after it in the same batch, so this checks the order inside a batch.
        assert_eq!(account.available(), Amount::from_units(20_000));
        let line = logs.lines().next().expect("the refusal was logged");
        assert!(line.contains("client=1 tx=2 "), "{line}");
        assert!(line.contains("insufficient available funds"), "{line}");
    }

    #[test]
    fn the_pipeline_logs_refusals_under_one_cap_shared_by_all_workers_and_one_summary() {
        // Every withdrawal is refused, and the three clients sit on three shards.
        let rows = (1..=30u32).map(|tx| Ok(withdrawal((tx % 3) as u16, tx, 10_000)));
        let (result, logs) =
            capture_logs(|| block_on_current_thread(run_pipeline(rows, nz(3), nz(4))));
        assert!(result.is_ok());
        let individual = logs
            .lines()
            .filter(|line| line.contains("transaction rejected"))
            .count();
        assert_eq!(individual, 20, "{logs}");
        let summaries = logs
            .lines()
            .filter(|line| line.contains("transactions rejected in total"))
            .count();
        assert_eq!(summaries, 1, "{logs}");
        assert!(logs.contains("rejected=30"), "{logs}");
    }

    #[test]
    fn the_refusal_summary_is_logged_when_the_run_ends_in_a_read_error() {
        let rows = (1..=25u32)
            .map(|tx| Ok(withdrawal(1, tx, 10_000)))
            .chain(std::iter::once(Err(a_read_error())));
        let (result, logs) =
            capture_logs(|| block_on_current_thread(run_pipeline(rows, nz(2), nz(4))));
        assert!(
            matches!(result, Err(PipelineError::Read(_))),
            "got {result:?}"
        );
        assert!(logs.contains("rejected=25"), "{logs}");
    }

    #[test]
    fn the_refusal_summary_is_logged_when_a_worker_panics_after_its_work() {
        let rows = (1..=25u32).map(|tx| Ok(withdrawal(1, tx, 10_000)));
        let (result, logs) = capture_logs(|| {
            block_on_current_thread(run_with_workers(
                opened(rows),
                nz(1),
                nz(4),
                |inbox, refusals| {
                    tokio::spawn(async move {
                        let _engine = worker(inbox, refusals).await;
                        panic!("a worker failed after its work")
                    })
                },
            ))
        });
        assert!(
            matches!(result, Err(PipelineError::WorkerPanicked)),
            "got {result:?}"
        );
        assert!(logs.contains("rejected=25"), "{logs}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_requested_capacity_limits_how_far_the_reader_runs_ahead() {
        const ROWS: usize = 3_000;
        for capacity in [1, 2, 3, 10, 256, 300, 600] {
            let produced = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&produced);
            let rows = (1..=ROWS as u32).map(move |tx| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(deposit(1, tx))
            });
            let gate = Arc::new(tokio::sync::Notify::new());
            let worker_gate = Arc::clone(&gate);
            let run = tokio::spawn(run_with_workers(
                opened(rows),
                nz(1),
                nz(capacity),
                move |inbox, refusals| {
                    let gate = Arc::clone(&worker_gate);
                    tokio::spawn(async move {
                        gate.notified().await;
                        worker(inbox, refusals).await
                    })
                },
            ));

            // The worker holds still. The channel holds `slots` batches, and the
            // router has filled one more batch and waits to send it.
            let (batch, slots) = channel_shape(capacity);
            let stalled_at = slots * batch + batch;
            let started = std::time::Instant::now();
            while produced.load(Ordering::SeqCst) < stalled_at {
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "capacity {capacity}: the reader never reached the channel limit"
                );
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(
                produced.load(Ordering::SeqCst),
                stalled_at,
                "capacity {capacity}"
            );

            gate.notify_one();
            let accounts = within(run)
                .await
                .expect("the pipeline task")
                .expect("the pipeline succeeds");
            assert_eq!(accounts.len(), 1);
            assert_eq!(produced.load(Ordering::SeqCst), ROWS);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_worker_is_started_per_shard() {
        let started = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&started);
        let rows = (1..=10u32).map(|tx| Ok(deposit(tx as u16, tx)));
        let accounts = within(run_with_workers(
            opened(rows),
            nz(5),
            nz(2),
            move |inbox, refusals| {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(worker(inbox, refusals))
            },
        ))
        .await
        .expect("the pipeline succeeds");
        assert_eq!(started.load(Ordering::SeqCst), 5);
        assert_eq!(accounts.len(), 10);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requests_beyond_what_can_be_used_are_lowered_instead_of_panicking() {
        let started = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&started);
        let rows = (1..=5u32).map(|tx| Ok(deposit(tx as u16, tx)));
        let accounts = within(run_with_workers(
            opened(rows),
            NonZeroUsize::MAX,
            NonZeroUsize::MAX,
            move |inbox, refusals| {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(worker(inbox, refusals))
            },
        ))
        .await
        .expect("the pipeline succeeds");
        // No client id can reach a shard beyond the 65,536th.
        assert_eq!(started.load(Ordering::SeqCst), 65_536);
        assert_eq!(accounts.len(), 5);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_worker_that_panics_makes_the_pipeline_fail_with_that_error() {
        let rows = (1..=1_000u32).map(|tx| Ok(deposit(1, tx)));
        let result = within(run_with_workers(
            opened(rows),
            nz(2),
            nz(4),
            |inbox, _refusals| {
                tokio::spawn(async move {
                    drop(inbox);
                    panic!("a worker failed on purpose")
                })
            },
        ))
        .await;
        assert!(
            matches!(result, Err(PipelineError::WorkerPanicked)),
            "got {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_worker_that_stops_early_without_panicking_makes_the_pipeline_fail_as_stopped() {
        let rows = (1..=1_000u32).map(|tx| Ok(deposit(1, tx)));
        let result = within(run_with_workers(
            opened(rows),
            nz(1),
            nz(4),
            |inbox, _refusals| {
                tokio::spawn(async move {
                    drop(inbox);
                    Engine::default()
                })
            },
        ))
        .await;
        assert!(
            matches!(result, Err(PipelineError::Stopped)),
            "got {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_worker_that_is_gone_on_a_shard_the_input_never_names_stops_the_run() {
        // Every row belongs to client 1, which sits on shard 1 of 2. Shard 0's
        // worker is gone before the router starts, and nothing is ever sent to it,
        // so only the periodic check can find out.
        let produced = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&produced);
        let rows = (1..=100_000u32).map(move |tx| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(deposit(1, tx))
        });
        let spawned = Arc::new(AtomicUsize::new(0));
        let result = within(run_with_workers(
            opened(rows),
            nz(2),
            nz(1024),
            move |inbox, refusals| {
                if spawned.fetch_add(1, Ordering::SeqCst) == 0 {
                    drop(inbox);
                    tokio::spawn(async { Engine::default() })
                } else {
                    tokio::spawn(worker(inbox, refusals))
                }
            },
        ))
        .await;
        assert!(
            matches!(result, Err(PipelineError::Stopped)),
            "got {result:?}"
        );
        assert_eq!(produced.load(Ordering::SeqCst), 1024);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_worker_panic_is_reported_before_a_read_error() {
        let rows = std::iter::once(Err(a_read_error()));
        let result = within(run_with_workers(
            opened(rows),
            nz(2),
            nz(4),
            |inbox, _refusals| {
                tokio::spawn(async move {
                    drop(inbox);
                    panic!("a worker failed on purpose")
                })
            },
        ))
        .await;
        assert!(
            matches!(result, Err(PipelineError::WorkerPanicked)),
            "got {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dropping_the_pipeline_future_stops_the_reader_and_the_workers() {
        let produced = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&produced);
        let rows = (1u32..).map(move |tx| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(deposit((tx % 100) as u16, tx))
        });
        let outcome = tokio::time::timeout(
            Duration::from_millis(100),
            run_pipeline(rows, nz(4), nz(64)),
        )
        .await;
        assert!(outcome.is_err(), "an endless input cannot finish");

        // The future is gone. Give the reader time to notice, then it must
        // stand still.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let before = produced.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            produced.load(Ordering::SeqCst),
            before,
            "the reader kept going after the future was dropped"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_reader_entry_point_waits_for_the_header_off_the_async_threads() {
        let (release, wait) = std::sync::mpsc::channel();
        let reader = Stalled {
            release: wait,
            released: false,
            data: Cursor::new(b"type,client,tx,amount\ndeposit,1,1,1.0\n".to_vec()),
        };
        let run = tokio::spawn(run_pipeline_from_reader(reader, nz(2), nz(4)));

        // The only runtime thread has to stay free while the reader is stuck.
        // This sleep can finish only if the header is not read on that thread.
        tokio::time::sleep(Duration::from_millis(100)).await;
        release.send(()).expect("the reader is waiting");
        let accounts = within(run)
            .await
            .expect("the pipeline task")
            .expect("the pipeline succeeds");
        assert_eq!(accounts.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_reader_entry_point_gives_the_accounts_of_the_input_and_reports_a_bad_header() {
        let csv = "type,client,tx,amount\ndeposit,1,1,1.0\ndeposit,2,2,3.0\nwithdrawal,2,3,1.0\n";
        let mut accounts = within(run_pipeline_from_reader(
            Cursor::new(csv.as_bytes().to_vec()),
            nz(2),
            nz(4),
        ))
        .await
        .expect("the pipeline succeeds");
        accounts.sort_by_key(|(client, _)| *client);
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].1.available(), Amount::from_units(10_000));
        assert_eq!(accounts[1].1.available(), Amount::from_units(20_000));

        let bad = within(run_pipeline_from_reader(
            Cursor::new(b"deposit,1,1,1.0\n".to_vec()),
            nz(2),
            nz(4),
        ))
        .await;
        assert!(matches!(bad, Err(PipelineError::Read(_))), "got {bad:?}");
    }

    #[test]
    fn pipeline_errors_describe_themselves() {
        assert_eq!(
            PipelineError::WorkerPanicked.to_string(),
            "a worker task panicked"
        );
        assert_eq!(
            PipelineError::RouterPanicked.to_string(),
            "the task reading the input panicked"
        );
        assert_eq!(
            PipelineError::Stopped.to_string(),
            "the pipeline was stopped before it finished"
        );
        assert!(std::error::Error::source(&PipelineError::WorkerPanicked).is_none());
        assert!(std::error::Error::source(&PipelineError::Stopped).is_none());

        let read = PipelineError::Read(a_read_error());
        assert!(read.to_string().contains("`type`"), "got: {read}");
        assert!(std::error::Error::source(&read).is_some());
    }
}
