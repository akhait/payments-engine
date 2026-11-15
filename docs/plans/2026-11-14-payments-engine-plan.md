# Payments Engine — Implementation Plan

Spec: docs/specs/2026-11-14-payments-engine.md
Workspace: branch payments-engine (a branch-safety hook blocks commits on main)
Jira: N/A

AC numbers below refer to the numbered acceptance criteria in the spec.

## Execution notes

- Every task is done test-first (RED, GREEN, REFACTOR).
- A task is finished only when `cargo test`, `cargo fmt --check` and
  `cargo clippy -- -D warnings` are all clean.
- Tasks 1-4 build the synchronous core, which is the heart of the solution.
  Tokio does not appear until Task 8, so the rules are proven before
  concurrency is added.
- Parallelizable: Task 3 and Task 5 share no files (both depend on Task 2),
  and Task 6 shares none with Tasks 3, 4 or 5. We will still run them in order
  so each piece can be explained as it lands.

## Progress

- [x] Task 1: Crate scaffold and `Amount` — `35edbae`
- [x] Task 2: Engine with deposits and withdrawals — `14e870f`
- [x] Task 3: Dispute and resolve — `61d7244`
- [x] Task 4: Chargeback and account locking — `cad6dfe`
- [x] Task 5: Row parsing and streaming reader — `2a842ea`
- [x] Task 6: Output writer — `2b23831`
- [x] Task 7: Sequential end-to-end CLI — `1e12659`
- [x] Task 8: Sharded concurrent pipeline — `a327fa2`
- [ ] Task 9: Switch the CLI to the concurrent pipeline and failure handling
- [ ] Task 10: README and final verification

Follow-up commits that belong to finished tasks: `477aa49` (tests build amounts
from raw units), `acfb739` (the output writer checks totals with
`try_for_each`) and the commit "test(concurrency): generate test input with
fastrand".

## Tasks

### Task 1: Crate scaffold and `Amount`

**What**: Create the crate (`cargo init --name payments_engine`, library plus
binary, with a stub `main`). Add `serde` with the derive feature. Implement
`Amount(i64)` in ten-thousandths: `FromStr` (trim, digits, optional `.`, at most
4 fractional digits, at least one digit, no sign, no exponent), `Display`
(always 4 decimals), `checked_add` / `checked_sub`, and `Deserialize` via
`FromStr`. Table-driven unit tests written first.
**Files**: `Cargo.toml`, `.gitignore` (contains `/target`), `src/lib.rs`,
`src/main.rs` (these three come from `cargo init`), `src/amount.rs`
**Depends on**: none
**Verify**: `cargo test amount` passes for AC 7, 8 and 9 (parse `"1.5"` to
15000, `"1"` to 10000, `"0.0001"` to 1, `" 2.0 "` to 20000; reject
`"1.00001"`, `"-1"`, `"abc"`, `"1e3"`, `""`; format 15000 as `1.5000`;
`i64::MAX` plus 1 gives `None`). `cargo build` succeeds.
**Status**: Complete

### Task 2: Engine with deposits and withdrawals

**What**: Add `Account { available, held, locked }` with `total()` built on
`checked_add`, the `Transaction` enum (all five variants), the `Rejection`
enum with `Display`, and `Engine::apply` for deposit and withdrawal only. The
deposit is recorded in the deposit map as `Normal`. Rules covered: amount must
be greater than zero (no account created otherwise), duplicate deposit tx id
rejected, insufficient funds rejected, overflow rejected with no state change,
account created by a valid deposit or withdrawal (rule 7). Dispute, resolve
and chargeback return a `Rejection::Unsupported` placeholder for now.
**Files**: `src/account.rs`, `src/transaction.rs`, `src/engine.rs`,
`src/lib.rs`, `src/amount.rs` (adds `Default` and `is_positive`)
**Depends on**: Task 1
**Verify**: `cargo test engine` passes AC 10 through 15 and the overflow
no-change case of AC 9 (for example `deposit,1,1,3.0` then `withdrawal,1,2,1.5`
leaves `available == 1.5`, `held == 0`; a withdrawal of 3.0 against 2.0
leaves balances at 2.0; a first-row withdrawal leaves an all-zero account).
**Status**: Complete

### Task 3: Dispute and resolve

**What**: Implement dispute and resolve in `Engine::apply`, using the
`DisputeState` and `DepositRecord` types introduced in Task 2. Also add the
`available + held` overflow check to `Account::credit` that Task 2 deferred
(`held` is always zero until a dispute can happen). Dispute requires a recorded deposit of the same
client in state `Normal`, moves the amount from available to held, and sets
`Disputed`. Resolve requires a `Disputed` deposit of the same client, moves the
amount back, and sets `Normal`. Neither creates an account. Available may go
negative; held never does.
**Files**: `src/account.rs`, `src/engine.rs`
**Depends on**: Task 2
**Verify**: `cargo test engine` passes AC 16, AC 17 and 18 (dispute and resolve
parts), AC 19 (already `Disputed`), AC 20, AC 21, AC 22 and AC 23 (resolve
part). For example `deposit,1,1,5.0` then `dispute,1,1` gives available 0,
held 5, total 5; a dispute on another client's tx changes nothing; after
`resolve` the same tx can be disputed again.
**Status**: Complete

### Task 4: Chargeback and account locking

**What**: Implement chargeback (same-client `Disputed` deposit; held decreases;
state becomes `ChargedBack`; account locked). Add the locked-account check to
deposit and withdrawal (`Rejection::AccountLocked`). Dispute, resolve and
chargeback remain allowed on a locked account for its other deposits (rule 26).
Remove the `Unsupported` placeholder.
**Files**: `src/engine.rs`, `src/account.rs` (adds `charge_back` and the lock
rule)
**Depends on**: Task 3
**Verify**: `cargo test engine` passes AC 17 and 18 (chargeback part), AC 19
(`ChargedBack`), AC 23 (chargeback part, and a rejected chargeback does not
lock), AC 24, AC 25 and AC 26. For example `deposit,1,1,5.0`, `dispute,1,1`,
`chargeback,1,1` gives `0, 0, 0, locked`, and a further deposit is rejected.
**Status**: Complete

### Task 5: Row parsing and streaming reader

**What**: Add `csv`, `tracing` as dependencies. Define the raw row struct
(`type`, `client`, `tx`, `amount: Option<Amount>` with `#[serde(default)]`) and
`TryFrom<RawRow> for Transaction` (a deposit or withdrawal without an amount is
an error). Add `read_transactions<R: Read>(reader) -> impl Iterator<Item =
Transaction>` using `ReaderBuilder` with `trim(Trim::All)` and
`flexible(true)`, reading one record at a time. A row-level error logs a `warn`
with the row number and is skipped; an I/O error aborts with an error.
**Files**: `Cargo.toml`, `src/transaction.rs`, `src/pipeline.rs`
**Depends on**: Task 2
**Verify**: `cargo test pipeline` using in-memory readers passes AC 4 (padded
fields and header), AC 5 (`dispute,1,1` with no amount field), and AC 6 (unknown
type, client 70000, tx 4294967296, negative amount, five decimals, missing
amount on a deposit are each skipped while the following valid row still
arrives).
**Status**: Complete
**Delivered as**: `read_transactions(reader) -> Result<Transactions<R>,
ReadError>`, an iterator of `Result<Transaction, ReadError>`, with quoting off, a
header check, a 1 KiB row limit, a byte order mark wrapper and capped logging.
See the spec (Reader / router) and the decision log.

### Task 6: Output writer

**What**: Add `write_accounts(out: impl Write, accounts)` that writes the header
`client,available,held,total,locked`, one row per account in the order given
(the order of the rows is unspecified), amounts via `Amount::Display`, `locked`
as `true` / `false`. A `total` computed with overflow produces an error rather
than a wrong number.
**Files**: `src/output.rs`, `src/lib.rs`
**Depends on**: Task 2
**Verify**: `cargo test output` writes into a `Vec<u8>` and checks AC 31 (header,
four decimals, `true` / `false`) and AC 32 (`total == available +
held` on every row).
**Status**: Complete

### Task 7: Sequential end-to-end CLI

**What**: Add `Engine::into_accounts()` so the engine can hand over its accounts.
Wire reader, one `Engine` and writer into `run(input, output)` and a real `main`: exactly one argument (usage line on stderr and exit 2 otherwise), file
open error to stderr and exit 1 with nothing on stdout, `tracing-subscriber`
(a runtime dependency now, `fmt` only, ANSI off) initialised to stderr with
default filter `warn`; `RUST_LOG` may name another level (`off`, `error`,
`warn`, `info`, `debug`, `trace`), which avoids the `env-filter` machinery and
the crates it pulls in. Add
small hand-written fixtures and an integration test that runs the built binary.
This is the walking skeleton: `cargo run -- file.csv > out.csv` works, still
single-threaded.
**Files**: `src/main.rs`, `src/lib.rs`, `src/engine.rs`, `Cargo.toml`,
`Cargo.lock`, `tests/cli.rs`, `tests/fixtures/*.csv`
**Depends on**: Tasks 4, 5, 6
**Verify**: `cargo test --test cli` passes AC 1, 2 and 3 (stdout holds only
CSV; no args and two args exit 2 with empty stdout; missing file exits 1 with
empty stdout), plus fixture runs comparing output as an unordered set of rows
(so row order does not matter). `cargo run -- tests/fixtures/basic.csv` shows
the expected accounts. Also checks stderr at the CLI level: a bad row is logged
with its data-row number (the fixture has CRLF line endings and blank lines) and
a fixed reason, never its content; a clean run logs nothing; an oversize row or
a bad header exits 1 with an error on stderr and nothing on stdout.

### Task 8: Sharded concurrent pipeline

**What**: Add `tokio` (`rt-multi-thread`, `sync`; `macros` and `time` are
dev-only, see D23). Implement
`run_pipeline(rows: impl Iterator<Item = Result<Transaction, ReadError>> + Send + 'static, shards,
capacity) -> Result<Vec<(ClientId, Account)>, PipelineError>`: a
`spawn_blocking` router consumes the iterator and calls
`Sender::blocking_send` to shard `client % shards`; each shard has a bounded
`mpsc` channel and a worker task owning an `Engine` that loops on
`recv().await` and logs rejections; when senders drop the workers
return their `Engine`s; a `collect_workers` helper joins them (a panic is
`PipelineError::WorkerPanicked`, any other `JoinError` is `Stopped`) and the
accounts of all engines are concatenated. Add a seeded pseudo-random
transaction generator for tests (`fastrand`, a dev-dependency, which replaced a
hand-written generator; see D21).
The router stops at the first `Err` item (a failing input or an oversize row),
lets the workers drain what they were already sent, and returns that error.
Shard and channel sizes are `NonZeroUsize`, so a zero cannot be passed, and both
are lowered to a maximum (65,536 shards, 1,048,576 queued transactions). The
code lives in a new `sharded` module rather than in `pipeline.rs`, which is the
reader. A private `run_with_workers` takes a function that opens the input and
a function that spawns each worker; it is the seam that lets tests use slow,
panicking or early-returning workers, and the public functions wrap it.
`run_pipeline_from_reader` opens the input (reads the header) inside the
blocking task. A guard aborts the workers when the future is dropped, and every
1024 transactions routed the router checks that no worker is gone. Refusal logging moves into
a small `RefusalLog` shared by all workers (one cap of 20 lines and one summary
whatever the shard count), and the sequential `run` of Task 7 uses it too, until
Task 9 replaces it;
`Rejection::InsufficientHeld`, which means the engine's bookkeeping is broken,
is never held back by the cap and is logged at error level (it still counts
towards the 20). A `Transactions` stream
that is dropped early logs its summary of skipped rows. The log-capturing test
helper moves to a shared `test_support` module so the `RefusalLog` tests can use
it.
**Files**: `Cargo.toml`, `Cargo.lock`, `src/lib.rs`, `src/sharded.rs` (new),
`src/refusals.rs` (new), `src/test_support.rs` (new, tests only),
`src/pipeline.rs` (a `Drop` for `Transactions`, and tests), `tests/concurrency.rs`
**Depends on**: Tasks 4, 5 and 7 (it uses `Engine::into_accounts` and
`Transaction::client` and `tx`)
**Verify**: `cargo test --test concurrency` passes AC 27 (1 worker and 8
workers give the same accounts on a generated stream), AC 28 (10,000 interleaved
deposit/withdrawal pairs over 100 clients on 8 workers, no withdrawal rejected
for insufficient funds), AC 29 (capacity 1 completes without deadlock) and AC 32
(`total == available + held` for every account of 10 generated runs).
`cargo test --lib sharded` passes the unit tests with stand-in workers: AC 29
(the queue bound for several capacities), AC 30 (a panicking worker fails the
run; the exit code, the message on stderr and the empty stdout wait for
Task 9), AC 42 (dropping the future stops the reader) and AC 43 (a vanished
worker stops the reader).

### Task 9: Switch the CLI to the concurrent pipeline and failure handling

**What**: Make `run` async, taking the shard count and the channel capacity as
parameters and calling `run_pipeline_from_reader`, and remove the sequential
path. Add `default_shards()` (`available_parallelism()`, minimum 1) and
`DEFAULT_CHANNEL_CAPACITY` (1024), which `main` passes. `RunError::Read` becomes
`RunError::Pipeline`. `main` builds a multi-thread Tokio runtime by hand, rather
than with `#[tokio::main]`, with the worker count set explicitly, and blocks on
`run` (decision D23). Map `PipelineError` and I/O errors to exit 1 with a
message on stderr and nothing written to stdout (write output only after the
pipeline has fully succeeded, in a small `report` function).
**Files**: `src/main.rs`, `src/lib.rs`, `src/sharded.rs` and
`src/test_support.rs` (the shared test helpers `nz` and `a_read_error`),
`tests/cli.rs` (`Cargo.toml` does not change: the `macros` feature stays
dev-only)
**Depends on**: Tasks 7, 8
**Verify**: every existing `cargo test --test cli` case still passes unchanged,
and new ones run a 2,000-client file through the binary (the expected rows,
and nothing on stdout when an oversize row follows the valid rows), check the cap on logged refusals across 30 clients, and check that a bad
`TOKIO_WORKER_THREADS` does not change the exit code. AC 30 is covered
piecewise: Task 8 shows by unit tests in `sharded.rs` that a panicking worker
becomes `WorkerPanicked`; Task 9 adds a `report` test that asserts no bytes are
written for each kind of `PipelineError`, and `main` gives exit 1 for every
error, which the CLI tests show for read errors. No test makes a worker panic
through the binary. A `run` test gives the same report for 1, 2, 3, 8 and 64
workers (AC 27 on the CSV). A worker that goes away needs nothing more in the
CLI: it is noticed within 1024 transactions routed (D15) and the run exits 1.
The runtime has no blocking thread left at shutdown, because `run` awaits the
reader to its end on every path (D25).

### Task 10: README and final verification

**What**: Write `README.md`: how to build and run, architecture (diagram of the
sharded pipeline), every assumption from the spec, the dispute state machine,
how correctness is checked (type system, tests, invariants), error handling and
logging, and the known memory limit of the deposit history. Run a manual
large-input check: generate several million rows into the scratchpad directory
(not the repo), run the release build under `/usr/bin/time -l`, confirm memory
stays flat, and record the wall-clock and CPU time next to the figures that
reviewers reported for the sequential version (D24). The README also says that
`TOKIO_WORKER_THREADS` has no effect (D23), that warnings are not in input order
(D24), and what the throughput caveat is. Confirm the repo contains no mention of the company or its
products and does not contain the challenge PDF.
**Files**: `README.md`
**Depends on**: Task 9
**Verify**: AC 33 (README covers assumptions, architecture, tests, error
handling). `cargo build --release` succeeds; `cargo test`, `cargo fmt --check`
and `cargo clippy -- -D warnings` are clean; the large run finishes with
bounded peak memory; a case-insensitive search of the repo (excluding
`target/`) for the company and brand names listed in the brief finds nothing,
and `git ls-files` shows no PDF.
