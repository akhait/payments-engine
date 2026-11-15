# payments_engine

A small command-line payments engine. It reads a CSV of transactions
(`deposit`, `withdrawal`, `dispute`, `resolve`, `chargeback`), keeps one account
per client, and prints the final state of every account as CSV.

```
$ cargo run --release -- transactions.csv > accounts.csv
```

For example, `cargo run --release -- tests/fixtures/example.csv` reads this file
(the fixture is the same text, and a test checks that the README and the binary
agree):

```
type,       client, tx, amount
deposit,         7, 10, 5.0
deposit,         3, 11, 2.5
withdrawal,      7, 12, 1.25
dispute,         7, 10
deposit,         3, 13, 1.0
withdrawal,      3, 14, 9.0
```

```
client,available,held,total,locked
3,3.5000,0.0000,3.5000,false
7,-1.2500,5.0000,3.7500,false
```

Client 7 withdrew 1.25 and then disputed the 5.0 deposit that funded it, so the
whole 5.0 is on hold and `available` goes negative (see the rules below). Client
3's withdrawal of 9.0 was refused for lack of funds and logged on stderr.

- [Build, run, test](#build-run-test)
- [Input and output](#input-and-output)
- [Rules and assumptions](#rules-and-assumptions)
- [Architecture](#architecture)
- [Source layout](#source-layout)
- [How correctness is checked](#how-correctness-is-checked)
- [Errors and logging](#errors-and-logging)
- [Performance and memory](#performance-and-memory)
- [Known limits](#known-limits)
- [Using it behind a server](#using-it-behind-a-server)
- [How this was built](#how-this-was-built)

## Build, run, test

Rust 1.85 or newer (edition 2024), tested with 1.89.

```
cargo build --release
./target/release/payments_engine transactions.csv > accounts.csv

cargo test                                  # unit, CLI and concurrency tests
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

The command takes exactly one argument, the path of the input file. Anything
else prints a usage line to stderr and exits with code 2. Only CSV goes to
stdout; everything else goes to stderr.

| Exit code | Meaning |
|---|---|
| 0 | The report was written. |
| 1 | The run failed: the file could not be opened, the header is wrong, a row is too long, reading failed, a worker failed, a total is too large to show, or the report could not be written. Nothing is written to stdout, except that a failure while writing the report itself can leave part of it. |
| 2 | Usage error. |
| 101 | Tokio panicked because the operating system refused to start a thread (see [Known limits](#known-limits)). |

## Input and output

Input columns are `type, client, tx, amount`.

- `client` is a `u16` and `tx` is a `u32`; both are digits only.
- `amount` has at most four decimal places, no sign and no exponent. A row
  that breaks this is a bad row.
- Spaces around fields are allowed (`deposit, 1, 1, 1.0`).
- The header is required. It must name `type`, `client`, `tx` and `amount`
  exactly once each, in any order. Names are case sensitive and other columns
  are ignored. Input with nothing in it at all is not an error.
- `.5` and `5.` are accepted as amounts.
- Dispute, resolve and chargeback rows may leave out the amount. A well-formed
  amount on such a row is ignored; a malformed one makes the row a bad row.
- A UTF-8 byte order mark at the start is skipped.
- Rows are read in order and are assumed to be in chronological order.
- Quoted fields are not supported: every field is a keyword or a number, so a
  quoted one is treated as a bad row. Without quote handling a stray quote cannot
  swallow the rows after it.

Output columns are `client, available, held, total, locked`.

- Amounts always have four decimals (`1.5000`). A negative amount has a minus
  sign.
- `total` is `available + held`.
- `locked` is `true` or `false`.
- The rows come in no particular order, and the order can differ from one run
  to the next. The task says it does not matter, and the tests compare the rows
  as a set.

Amounts are stored as a count of 1/10,000 (`Amount(i64)`), never as floating
point.

## Rules and assumptions

| Transaction | Applied when | Effect |
|---|---|---|
| deposit | amount > 0; account not locked; this client has no deposit with this `tx` yet; no overflow | `available += amount`; the deposit is recorded |
| withdrawal | amount > 0; account not locked; `available >= amount` | `available -= amount` |
| dispute | this client has a deposit with this `tx` and it is not already disputed or charged back | `available -= amount`, `held += amount` |
| resolve | that deposit is under dispute | `held -= amount`, `available += amount` |
| chargeback | that deposit is under dispute | `held -= amount`; the account is locked |

A deposit moves through three states:

```
                  dispute                          chargeback
   Normal ───────────────────► Disputed ───────────────────► ChargedBack
      ▲                           │                           (final; locks
      │                           │                            the account)
      └───────────────────────────┘
                  resolve
```

A transaction that does not apply is skipped, logged and does not stop the run.
It never changes state, with one exception noted in assumption 5 below.

These are the choices the rules above leave open. The spec in
[docs/specs](docs/specs/2026-11-14-payments-engine.md) and the decision log in
[docs/adr](docs/adr/payments-engine.md) give the reasons.

1. **Only deposits can be disputed.** Withdrawals are not recorded, so a dispute
   that names a withdrawal is refused as an unknown transaction.
2. **A locked account refuses deposits and withdrawals** and logs a warning (when
   more than one reason applies, which one is logged is not specified). It still
   processes dispute, resolve and chargeback for its other deposits. A lock is
   permanent; there is no unlock.
3. **`available` can go negative**, and so can the balance of a locked account.
   This happens when a client withdrew money and the deposit that funded it is
   then disputed or charged back. `held` is never negative.
4. **A resolved deposit can be disputed again.** A charged-back deposit cannot.
5. **A withdrawal refused for insufficient funds still creates the client's
   account**, with zero balances, so the client appears in the output. A refused
   deposit never creates an account. Dispute, resolve and chargeback never create
   one.
6. **Deposits are keyed by `(client, tx)`.** A dispute naming a client other than
   the one who made the deposit finds nothing. Transaction ids are therefore
   unique per client, not globally; the same id used by two clients is accepted
   for both. This is what keeps results independent of how clients are spread over
   workers.
7. **Withdrawal ids are not recorded**, so a replayed withdrawal row is applied
   twice. A duplicate deposit id is refused.
8. **Dispute, resolve and chargeback rows come from a trusted source.** The
   engine cannot tell who sent them. Anyone who controls the feed could create a
   tiny deposit for a client and charge it back to lock that client for good.
9. **Balances are limited** to about 9.2 × 10^14 whole currency units (the
   largest amount is 922,337,203,685,477.5807, the `i64` range at four decimals).
   An input amount above that is a bad row. Every operation uses checked
   arithmetic, including `available + held`, and a transaction whose result would
   overflow is refused. The largest amount has 19 significant digits, more than a
   spreadsheet or a floating-point reader keeps exactly.
10. **Bad input does not abort the run**, except for what makes the input
    unreadable: a bad header, a row longer than 1024 bytes, or a
    failing read. A bad row is skipped with a warning.
11. **A final row without a line ending is accepted** as if it were complete. CSV
    cannot tell a cut-off row from a finished one, so a connection that drops
    in the middle could turn `...,100.0` into `...,1`. That is fine for a file; a
    server would have to require a final line ending.

## Architecture

```
 file ──► reader ──► router ──┬──► worker 0 ───┐
 (blocking thread,  client    ├──► worker 1 ───┤
  one row at a time) % N      ├──► ...         ├──► accounts ──► CSV writer ──► stdout
                              └──► worker N-1 ─┘

 The reader and the router are one blocking task. Between the router and each
 worker there is a bounded channel that carries batches of up to 256
 transactions. All workers also write refusals to one shared counter (the
 refusal log).
```

- **The rules are a plain synchronous type.** `Engine` holds the accounts and the
  deposit history and applies one transaction at a time. It knows nothing about
  threads or I/O, so every rule is tested without async code.
- **Sharding by client.** One reader parses rows and sends each one to the worker
  that owns its client (`client % N`). Each worker owns its clients' accounts and
  deposit history outright. There are no locks and no shared account state; the
  only state the workers share is one atomic counter for the refusal log.
- **Order is kept where it matters.** All of a client's transactions go to one
  worker over one channel, so they are applied in file order. A withdrawal cannot
  overtake the deposit that funds it, and a dispute cannot overtake its deposit.
  Different clients are independent, so they can run in parallel.
- **The alternative was rejected.** One task per transaction over
  `Arc<Mutex<..>>` maps gives mutual exclusion but not ordering, and the output
  would depend on timing.
- **Streaming and backpressure.** The reader holds one row at a time, and a row
  may not exceed 1024 bytes. The channels are bounded (a limit of 1024
  transactions per worker, plus the batch the router holds for it and the one the
  worker is applying), so a slow worker makes the reader wait instead of letting
  memory grow.
- **Batches.** The router sends a worker up to 256 transactions per message.
  One message per transaction made the pipeline slower than a single thread (see
  [Performance and memory](#performance-and-memory)).
- **The CSV reader blocks, so it has a thread of its own.** It runs in
  `spawn_blocking`, and the header is read there too, so a peer that is slow to
  send it cannot stall the async threads.
- **One worker per core.** The command uses `available_parallelism()` workers and
  gives the Tokio runtime the same number of threads. It sets that number
  explicitly, so the `TOKIO_WORKER_THREADS` environment variable has no effect.
- **Shutdown follows the channels.** When the reader finishes, its senders drop,
  each worker drains its queue and returns its engine, and the accounts are
  collected. If the future that runs the pipeline is dropped early, the workers
  are aborted, which stops the reader at its next send.
- **A failed worker fails the run.** A worker that panics or goes away is not
  replaced: its accounts are lost with it, and a fresh worker would start empty
  and report balances that look right and are wrong. The reader checks for a
  vanished worker after every 1024 transactions it routes. The error reported is the worker's, ahead of
  any read error.
- **Nothing is written until everything succeeded.** The report is produced only
  after the whole input was read and every worker finished, so a failure never
  leaves half a report on stdout. (A failure while writing the report itself can.)

## Source layout

| File | What it holds |
|---|---|
| `src/amount.rs` | `Amount`, the fixed-point newtype |
| `src/transaction.rs` | the raw CSV row and the `Transaction` enum |
| `src/account.rs` | `Account` and the state of a deposit |
| `src/engine.rs` | `Engine`, the rules, and why a transaction is refused |
| `src/pipeline.rs` | the streaming CSV reader and its limits |
| `src/sharded.rs` | the router, the workers, `run_pipeline` and its failures |
| `src/refusals.rs` | the shared cap on logging refused transactions |
| `src/output.rs` | the report writer |
| `src/lib.rs` | `run`, `report`, the default worker count and channel limit |
| `src/main.rs` | the command line, the Tokio runtime and `RUST_LOG` |
| `src/test_support.rs` | helpers shared by the unit tests (tests only) |
| `tests/` | CLI tests on the real binary, concurrency tests, fixtures |

## How correctness is checked

**The types make invalid states hard to write.**
- `Amount` is a newtype with `checked_add` and `checked_sub` and no operators, so
  unchecked arithmetic cannot creep in. Release builds also keep overflow checks.
- A raw CSV row becomes a `Transaction` enum through `TryFrom`, so a deposit
  without an amount cannot exist past the parsing boundary.
- `ClientId` and `TxId` are `u16` and `u32`, so out-of-range ids are parse errors.
- The deposit history stores only deposits, since only deposits can be disputed.
- `#![forbid(unsafe_code)]`.

**The tests** (more than 230, hermetic, with no randomness except a seeded generator;
the few that wait use generous timeouts; `cargo test` takes a few seconds):
- **Rules.** A scenario test for each rule and each rejection reason, with no
  async and no I/O. Amount parsing and formatting are table-driven.
- **Parsing.** Padding, missing amounts, bad rows, the header rules, a byte order
  mark split across reads, rows over the size limit, and what is and is not
  logged (never the content of a row).
- **The real binary.** The CLI tests run the built executable on fixtures and
  generated files: exit codes, stdout and stderr, `RUST_LOG`, an unwritable
  stderr, a 2,000-client file whose rows are all checked, and a bad
  `TOKIO_WORKER_THREADS`.
- **Concurrency.** A seeded generator makes thousands of transactions of every
  kind over few clients, and the sharded pipeline must give exactly what one
  `Engine` gives, for many shard counts and channel sizes, including input sizes
  around the batch size. Further tests cover per-client ordering, a channel of
  one, back-pressure, cancellation, a worker that panics or goes away, and the
  order in which failures are reported.
- **Invariants.** On generated runs every row of the CSV report has
  `total == available + held`, checked on the text with a parser that does not
  use `Amount`, and every account has `held >= 0` and a total that does not
  overflow.

**Mutation checks, by hand.** During most tasks I made small deliberate breakages
(a flipped comparison, a skipped step) in the code and confirmed that the tests
fail. The decision log records the results for several tasks; the survivors it
records are changes nobody can observe, or cannot be provoked from a test.

**What is not checked.** The concurrency tests compare the pipeline with one
`Engine`, so they show that sharding changes nothing, not that the rules are
right. The rules rest on the table-driven tests and the mutation checks; there
is no independent model of them (decision D20 in the log). Also unchecked:
that the command starts more than one worker (no test sees the worker count);
the exit code for a worker that panics, which is covered only in pieces (no test
makes a worker panic through the binary); and the shared cap on logged
refusals through the binary, which tells a shared cap from a per-worker one only
on a machine with two or more cores. No automated advisory scan of the
dependencies was run (neither `cargo audit` nor `cargo deny` is installed). A
reviewer compared the RustSec advisory database of 15 November 2026 with
`Cargo.lock` by hand and found no applicable advisory and no yanked version. The
versions are pinned by `Cargo.lock`.

## Errors and logging

- Logs go to stderr. The default level is `warn`. `RUST_LOG` takes one level
  name: `off`, `error`, `warn`, `info`, `debug` or `trace`, in any case; an empty
  value counts as unset and any other value is reported and ignored. Only `warn`
  and `error` messages exist today, so `info`, `debug` and `trace` add nothing.
- Fatal errors are printed as `error: ...` whatever `RUST_LOG` says.
- **Bad rows** are logged with the data row number (header and blank lines not
  counted) and a fixed reason. The content of a row is never logged. The first
  20 are logged one by one, and one summary line with the total follows whenever
  at least one row was skipped.
- **Refused transactions** are logged with the client id, the tx id and a fixed
  reason. The first 20 recorded are logged, then a summary. The summary is also
  logged when the run ends in an error. With several workers these lines are in
  no fixed order and are not necessarily the first 20 in the file, and they are
  not in order relative to the warnings about bad rows, which come from the
  reader.
- A refusal that means the engine's own balances are inconsistent is logged at
  `error` level and is not held back by the cap. It should never happen.
- Logging a failed write to stderr never changes the exit code.

## Performance and memory

Measured on a generated file of 3 million rows over 5,000 clients (80 MB; 55%
deposits, 35% withdrawals, 10% disputes), with a release build on a 10-core
machine and `RUST_LOG=off`; the median of five alternating runs, and every build
wrote the same rows. The larger files below are the median of three runs.

| Build | Wall time | CPU time | Peak memory |
|---|---|---|---|
| Sequential (one thread, no channels; commit `a327fa2`) | 2.14 s | 2.1 s | 108 MB |
| Concurrent, one message per transaction | 2.7 to 3.2 s | 5.4 to 6.6 s | 126 to 129 MB |
| Concurrent, batches of 256 (this code) | 1.72 s | 2.1 s | 127 MB |

- Sending one transaction per message was slower than a single thread, most
  likely because each message costs a send and often a thread wake-up on the
  reader's critical path (not confirmed by profiling). Batches of 64 up to 1024 all take about the same time; 16 is slower.
- The gain over sequential is modest (about 20%) because the reader, which parses
  the CSV, remains the limit. Going further would need the parsing itself to run
  in parallel.
- Timed again with the larger files below, the same build took 1.75 s on the
  3-million-row file, within noise of the 1.72 s above.
- On a larger file of 9 million rows (243 MB) the concurrent build takes 5.3 s
  against 6.7 s for the sequential one.
- Memory does not depend on the size of the input, only on how many deposits are
  recorded. A 257 MB file of 9 million withdrawals and disputes, which records
  no deposit, peaks at 4 MB. A file of similar size (243 MB) with about 5 million
  deposits peaks at 521 MB, about 100 bytes per deposit, against 422 MB for the
  sequential build, which has no per-worker maps or batches in flight. See the
  first limit below.

## Known limits

**Memory and scale**
- The deposit history is unbounded: one entry per recorded deposit, roughly 45 to
  105 bytes each, for the life of the run. Tx ids can reach 2^32. Fixing this
  would need an external store.
- All of a client's transactions go to one worker, so one very busy client runs at
  the speed of one worker.
- The reader is the bottleneck (see above).

**Failure behavior**
- One failed worker fails the whole run, however much was already processed.
- If the operating system refuses to start a thread, Tokio panics and the exit
  code is 101.
- The command has no time or byte budget per input. A sender of endless line
  endings, or of one byte per second, holds the reader's thread. The blocking
  thread pool is finite (about 512 threads), so a front end serving many such
  streams would need read deadlines.

**The command line**
- A closed or read-only stdout still gives exit code 0 (the standard library
  treats a closed stdout as success).
- `-h` and `--help` are taken as file names.
- Opening a FIFO that has no writer blocks.
- The value of a bad `RUST_LOG` is echoed in full in the notice about it.

**Input**
- Quoted CSV is not supported; a quoted field is a bad row.
- A trailing comma on every row is rejected as an extra field; leading zeros in
  ids are accepted.
- A final row cut off without a line ending is read as complete (assumption 11).
- Two byte order marks at the start are accepted (the reader skips one and the
  csv crate a second).

The data assumptions (a trusted source for disputes, a permanent lock, negative
balances, unrecorded withdrawal ids, ids unique per client only, the balance
limit) are listed under [Rules and assumptions](#rules-and-assumptions).

## Using it behind a server

The design aims to be usable later behind a server that receives many concurrent
streams. The pieces are built for that: `run_pipeline_from_reader` takes any
`Read`, the engine is independent of the file, and the pipeline can be cancelled
by dropping its future (which cannot interrupt a reader that is inside `read`).
What a server would still need:

- one pipeline per stream, or a shared set of workers keyed by client, which
  changes the ordering story (order is only defined within a stream);
- read deadlines and byte budgets per stream;
- a requirement that a stream ends with a line ending (assumption 11);
- an async CSV reader (for example `csv-async`) in place of the blocking thread;
- a store for the deposit history that does not live in memory;
- a flush timeout on the batches, so that a trickling stream is not held back
  waiting for 256 transactions;
- per-process, not per-stream, caps on logging.

## How this was built

I built this with an AI assistant (Claude Code) and kept the working documents in
the repository so that the process can be reviewed:

- [`docs/specs/2026-11-14-payments-engine.md`](docs/specs/2026-11-14-payments-engine.md)
  is the spec: the problem, the design, the types, the rules, and numbered
  acceptance criteria.
- [`docs/plans/2026-11-14-payments-engine-plan.md`](docs/plans/2026-11-14-payments-engine-plan.md)
  breaks the work into ten tasks, each with what to build, which files it touches
  and how to verify it, and keeps a progress checklist with the commit of each
  task.
- [`docs/adr/payments-engine.md`](docs/adr/payments-engine.md) is the decision
  log: the choices that are not obvious from the code, why they were made, the
  alternatives, and what the reviews found. Some entries correct earlier ones;
  the log says so where that happened.

Each task followed the same loop: write the failing tests first, make them pass,
run the quality gates (`fmt`, `clippy -D warnings`, tests, `cargo doc`, release
build), break the code on purpose to check the tests notice, then have
independent reviewers look at the diff (a code reviewer, a security reviewer and
a documentation reviewer) and fix what they found before the commit. The output
writer, which has no rules logic, had no reviewers of its own until the end.
At the end the whole branch got a last review (a code review, a security review,
a documentation review and an audit of the acceptance criteria against the code
and tests); its findings and what was done with them are in the log (D29 to D31).
Two of the earlier reviews are worth reading in the log: the review of the CLI found that a bad
`TOKIO_WORKER_THREADS` crashed the binary with exit code 101, and a reviewer's
timing showed that the first concurrent version was slower than the sequential
one it replaced, which led to batching.

Dependencies: `csv`, `serde`, `tokio` (`rt-multi-thread` and `sync`), `tracing`
and `tracing-subscriber`. Tests also use `fastrand` and Tokio's `macros` and
`time`.
