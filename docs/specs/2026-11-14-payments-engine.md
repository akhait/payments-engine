# Payments Engine

Jira: N/A
Date: 2026-11-14

## Problem

Build a small command-line payments engine. It streams a CSV of transactions
(`deposit`, `withdrawal`, `dispute`, `resolve`, `chargeback`), maintains one
account per client, and prints the final state of every account as CSV on
stdout.

```
$ cargo run -- transactions.csv > accounts.csv
```

Input columns: `type, client, tx, amount`. `client` is a `u16`, `tx` is a
`u32`, `amount` has at most four decimal places. Whitespace around fields is
allowed. Rows are in chronological order. Dispute, resolve and chargeback rows
carry no amount and refer to an earlier transaction by `tx`.

Output columns: `client, available, held, total, locked`.

The implementation must be concurrent (Tokio), must stream its input instead of
loading it all, and must be easy to read and verify. It should also be usable
later behind a server that receives many concurrent streams.

## Approach

**Sharded workers.** One streaming reader parses rows and routes each one by
`client_id % N` over bounded channels to `N` worker tasks. Each worker owns the
accounts and deposit history of its clients outright, so there are no locks and
no shared account state; the only state the workers share is one atomic counter
for the refusal log. The business rules live in a plain synchronous
`Engine` type; Tokio is a thin layer around it.

Rejected alternative: one task per transaction over `Arc<Mutex<..>>` / `DashMap`
maps. Locks give mutual exclusion but not ordering, so a withdrawal could be
applied before the deposit that funds it, and a dispute before its deposit.
Output would be non-deterministic.

## Design

### Data flow

```
                              bounded mpsc (capacity C)
                           ┌──────────────────────────────► worker 0 ─┐
 file ─► csv reader ─►     │                                           │
 (spawn_blocking,    router├──────────────────────────────► worker 1 ─┤─► accounts of
  streaming)    shard=id%N │                                           │   all workers
                           └──────────────────────────────► worker N-1┘       │
                                                                              ▼
                                                              csv writer ► stdout

 All workers also write refusals to one shared refusal log (an atomic counter).
```

Shutdown is driven by channel closure: the reader finishes, its `Sender`s are
dropped, each worker drains its queue and returns its `Engine`, and
`run_pipeline` joins the workers and collects their accounts (client sets are
disjoint, so there is nothing to merge).
If the `run_pipeline` future is dropped before it finishes, the workers are
aborted, which closes their inboxes and stops the reader at its next send. Both
entry points (`run_pipeline` and `run_pipeline_from_reader`) work this way.

### Types

- `Amount(i64)`: fixed-point count of 1/10_000 units. `1.5` is `15000`.
  - `FromStr`: optional surrounding whitespace, digits with an optional `.` and
    at most 4 fractional digits, at least one digit overall, no sign, no
    exponent. Anything else is an error.
  - `Display`: always four decimals (`1.5000`).
  - `checked_add` / `checked_sub` return `None` on overflow. No operator
    overloads, so unchecked arithmetic cannot sneak in.
  - Signed, because `available` may go negative (see rules).
  - `Deserialize` via `FromStr` so `csv`/`serde` can read it directly.
- `ClientId = u16`, `TxId = u32`. Narrow types make out-of-range ids a parse
  error for free.
- `Account { available: Amount, held: Amount, locked: bool }`. `total()` is
  derived: `available.checked_add(held)`. The client id is the map key.
- `DisputeState { Normal, Disputed, ChargedBack }`.
- `DepositRecord { amount: Amount, state: DisputeState }`. Only deposits are
  stored, because only deposits can be disputed. The engine keys them by
  `(client, tx)`, so the client is part of the key.
- `Transaction` enum: `Deposit{client, tx, amount}`, `Withdrawal{..}`,
  `Dispute{client, tx}`, `Resolve{client, tx}`, `Chargeback{client, tx}`.
  Built from a raw CSV row via `TryFrom`, so a deposit without an amount is
  unrepresentable past the parsing boundary.
- `Rejection` enum with a `Display` impl: why a transaction was not applied.

### Engine (synchronous, no async)

`Engine { accounts: HashMap<ClientId, Account>, deposits: HashMap<(ClientId, TxId), DepositRecord> }`
with `apply(&mut self, Transaction) -> Result<(), Rejection>`.
A rejected transaction never changes state, except that a withdrawal refused
for insufficient funds still creates the client's account (rule 7).

| Transaction | Valid when | Effect |
|---|---|---|
| deposit | amount > 0; account not locked; `(client, tx)` not already a recorded deposit; no overflow | `available += amount`; record deposit as `Normal`; account created if missing |
| withdrawal | amount > 0; account not locked; `available >= amount` | `available -= amount`; not recorded |
| dispute | `(client, tx)` is a recorded deposit; state is `Normal` | `available -= amount`, `held += amount`; state `Disputed` |
| resolve | `(client, tx)` is a recorded deposit; state is `Disputed` | `held -= amount`, `available += amount`; state `Normal` |
| chargeback | `(client, tx)` is a recorded deposit; state is `Disputed` | `held -= amount`; state `ChargedBack`; `locked = true` |

Agreed rules and assumptions:

1. Only deposits can be disputed.
2. Deposits and withdrawals on a locked account are rejected and logged at
   `warn`.
3. `available` may go negative after a dispute (client withdrew, then disputed
   the funding deposit). `held` is never negative by construction.
4. A resolved deposit returns to `Normal` and may be disputed again.
   `ChargedBack` is terminal.
5. Bad rows and refused transactions never abort the run: the row is skipped or
   the transaction rejected, a `warn` line goes to stderr (capped, see AC 36
   and AC 40), and processing continues. Only input that cannot be read at all, a
   header that lacks a required column, or a row longer than 1024 bytes ends the
   run, with an error.
6. Deposits are stored and looked up by `(client, tx)`. A dispute, resolve or
   chargeback naming a client that did not make the deposit therefore finds
   nothing and is rejected as an unknown tx. The same key makes duplicate-id
   detection local to one client, so results do not depend on how clients are
   spread over workers.
7. Dispute, resolve and chargeback never create an account. A deposit or
   withdrawal with a valid amount creates one if the client is new. A deposit
   that is then rejected cannot leave a new account behind (duplicates and
   overflow only occur for existing clients), but a withdrawal refused for
   insufficient funds does: the client exists and is reported with zero
   balances.
8. Every mutation uses checked arithmetic, including `available + held`. On
   overflow the transaction is rejected.

### Reader / router (Tokio layer)

- `csv::ReaderBuilder` with `trim(Trim::All)`, `flexible(true)` (dispute,
  resolve and chargeback rows omit the trailing amount column) and
  `quoting(false)`: every field is a keyword or a number, so none needs
  quoting, and without quote handling a bad row stays one line instead of a
  stray quote pulling the following rows into one record. A quoted field is
  therefore a bad row. `amount` is `Option<Amount>`; `client` and `tx` accept
  ASCII digits only.
- The header must name `type`, `client`, `tx` and `amount` exactly once each,
  in any order (case sensitive, other columns ignored); otherwise reading fails
  with an error naming the column. Input with nothing at all in it is not an
  error.
- A UTF-8 byte order mark at the start is skipped by a small wrapper that first
  collects three bytes (the csv crate only strips it when it arrives within the
  first read, so a mark delivered alone made the whole input look empty). A row
  shorter than the header is padded with empty fields before parsing, because
  the csv deserializer wants a field for every header column; a required field
  that was missing then becomes an empty one and still fails.
- Rows are read one at a time with `read_record` and `StringRecord::deserialize`
  through a `RowLimit` wrapper. It fails the stream with an error as soon as a
  row exceeds 1024 bytes (`MAX_ROW_BYTES`; a valid row is about 60 bytes),
  because the csv crate has no limit of its own and would buffer a hostile
  endless row. The wrapper also retries interrupted reads. A row with more
  fields than the header is a bad row, since the extra fields would be dropped
  silently.
- `csv` is blocking, so the reader runs inside `tokio::task::spawn_blocking`
  and hands rows over with `Sender::blocking_send`. A blocking read on an async
  worker thread would stall other tasks. The reader is opened there too
  (`run_pipeline_from_reader`), so waiting for the header cannot stall them
  either.
- A bad row: warn with its row number (counting data rows from 1, header and
  blank lines excluded) and a fixed reason, never the row's content, then skip
  it. Only the first 20 skips are logged one by one; after that they are
  counted, and one summary with the total is logged when the stream ends or is
  dropped. An
  I/O error, or a row over the size limit, ends the stream with an error that
  aborts the run with a non-zero exit.
- Channels are bounded (constant capacity, e.g. 1024 per worker). When a worker
  is slow its channel fills and `blocking_send` blocks the reader thread
  (backpressure), so the queued transactions stay bounded whatever the size of
  the file. A requested capacity above 1,048,576 is lowered to that, and a
  requested worker count above 65,536 (the number of client ids) to that, so a
  huge request cannot panic the channel or exhaust memory on workers that could
  never receive anything.
- Worker count `N` is `available_parallelism()`, minimum 1.
- Workers are async tasks looping on `rx.recv().await` and calling the
  synchronous `Engine::apply`. Rejections go to one `RefusalLog` shared by all
  workers (an atomic counter), so the cap on logged refusals holds for the whole
  run and not per worker (AC 40). A refusal that means the engine's own
  balances are inconsistent (`InsufficientHeld`) is the exception: input cannot
  cause it, so it is never held back by the cap and is logged at error level.
  It is still counted: it takes one of the 20 slots if it comes first, and it is
  in the total of the summary.
- After every 1024 transactions routed, the reader checks that no worker has
  dropped its inbox. A
  worker that is gone shows when the reader sends to its shard, but a shard the
  input no longer names is never sent to, and the reader would read on to the
  end of the input unaware.

### Output

- After all workers return, their accounts are collected and handed to
  `write_accounts`, which writes them with `csv::Writer` to stdout in the order
  it gets them (the order of the rows is unspecified and may differ from run to
  run). If any account's total cannot be represented,
  nothing is written and the run fails.
- Header `client,available,held,total,locked`; amounts via `Amount::Display`
  (four decimals); `locked` as `true` / `false`.

### Errors and logging

- `tracing` + `tracing-subscriber` writing to **stderr** only; default filter
  `warn`, overridable with `RUST_LOG`, which names one level (`off`, `error`, `warn`,
  `info`, `debug`, `trace`). Stdout carries nothing but the CSV.
- Exit codes: `0` success; `1` runtime failure (file cannot be opened, I/O
  error, worker panic); `2` usage error.
- `run_pipeline` fails with a `PipelineError`: `Read` (the input failed, a row
  was too long, or the header was bad),
  `WorkerPanicked`, `RouterPanicked` (the task reading the input panicked) or
  `Stopped` (a task ended without a panic before the input did: a worker went
  away, or the runtime was shutting down). A panic surfaces as a `JoinError`.
  When more than one thing went wrong, a worker panic is reported first, then a
  stopped worker, then the outcome of the reader, because a failed worker is the
  likelier cause of the rest. The run fails with a message on stderr and no
  partial CSV is written. In every case in which `run_pipeline` returns, the
  summary of refusals is logged; the summary of skipped rows is logged when the
  reader ends, including when it is stopped early.

### Layout

Crate `payments_engine` (library + binary):

```
src/main.rs      CLI parsing, runtime, exit codes
src/lib.rs       module wiring, public entry point
src/amount.rs    Amount newtype
src/transaction.rs  raw row, Transaction, TryFrom
src/account.rs   Account, DepositRecord, DisputeState
src/engine.rs    Engine, Rejection, rules
src/pipeline.rs  streaming CSV reader (Transactions)
src/sharded.rs  router, workers, run_pipeline
src/refusals.rs  RefusalLog: one cap on refusal logging, shared by workers
src/test_support.rs  log-capture helper for unit tests (cfg(test) only)
src/output.rs    CSV writer
tests/           integration tests + fixtures
```

Dependencies: `tokio` (`rt-multi-thread`, `sync`), `csv`, `serde`, `tracing`,
`tracing-subscriber`. The tests also use tokio's `macros` and `time`, and
`fastrand` for seeded test data (dev-dependencies only); `macros` joins the
runtime features when the CLI gets `#[tokio::main]`. No others unless a need
shows up.

## Acceptance Criteria

CLI and input

1. The system shall take exactly one argument, the input CSV path, write only
   CSV to stdout, and write all diagnostics to stderr.
2. If zero or more than one argument is given, then the system shall print a
   usage line to stderr, write nothing to stdout, and exit with code 2.
3. If the input file cannot be opened, then the system shall print the error to
   stderr, write nothing to stdout, and exit with code 1.
4. When fields and header names have spaces around them
   (`deposit, 1, 1, 1.0`), the system shall parse them exactly like the
   unpadded form.
5. When a dispute, resolve or chargeback row has no trailing amount field
   (`dispute,1,1`), the system shall parse it successfully.
6. If a row has an unknown type, a client outside 0..=65535, a tx outside
   0..=4294967295, a non-numeric field, an id that is not plain digits (such as
   `+1` or `0x10`), more fields than the header names, a quoted field, a
   missing amount on a deposit or withdrawal, a negative amount, or an amount
   with more than four decimal places, then the system shall log a warning containing the row number, skip
   the row, and continue.

Amount

7. The system shall parse `"1.5"` as 15000, `"1"` as 10000, `"0.0001"` as 1,
   and `" 2.0 "` as 20000, and shall reject `"1.00001"`, `"-1"`, `"abc"`,
   `"1e3"` and `""`.
8. The system shall format 15000 as `1.5000`, 1 as `0.0001` and 0 as `0.0000`.
9. If an addition or subtraction would overflow `i64`, then `Amount` shall
   report failure and the engine shall leave all state unchanged.

Deposit and withdrawal

10. When `deposit,1,1,1.0` and `deposit,1,2,2.0` are processed, the system shall
    output `1,3.0000,0.0000,3.0000,false`.
11. When `deposit,1,1,3.0` then `withdrawal,1,2,1.5` are processed, the system
    shall output `1,1.5000,0.0000,1.5000,false`.
12. If a withdrawal exceeds available funds, then the system shall reject it,
    log a warning, and leave balances unchanged: `deposit,2,1,2.0` then
    `withdrawal,2,2,3.0` outputs `2,2.0000,0.0000,2.0000,false`.
13. If the first row for a client is a withdrawal it cannot cover, then the
    system shall still output that client with `0.0000,0.0000,0.0000,false`.
14. If a deposit or withdrawal amount is zero, then the system shall reject it
    and shall not create an account for that client.
15. If a deposit reuses a tx id already recorded as a deposit of the same
    client, then the system shall reject it and leave balances unchanged. A
    deposit whose tx id was used by a different client is not a duplicate: ids
    are assumed globally unique, and the engine enforces uniqueness per client
    only.

Dispute, resolve, chargeback

16. When `deposit,1,1,5.0` then `dispute,1,1` are processed, the system shall
    output `1,0.0000,5.0000,5.0000,false`.
17. If a dispute, resolve or chargeback references an unknown tx, then the
    system shall ignore it with a warning and shall not create an account.
18. If a dispute, resolve or chargeback names a client different from the one
    that made the referenced deposit, then the system shall reject it and
    change neither client's balances.
19. If a dispute references a deposit that is already `Disputed` or
    `ChargedBack`, then the system shall reject it and held funds shall not
    change.
20. If a dispute references a withdrawal's tx id, then the system shall reject
    it as an unknown tx.
21. When `deposit,1,1,5.0`, `withdrawal,1,2,5.0`, `dispute,1,1` are processed,
    the system shall output `1,-5.0000,5.0000,0.0000,false`.
22. When `deposit,1,1,5.0`, `dispute,1,1`, `resolve,1,1` are processed, the
    system shall output `1,5.0000,0.0000,5.0000,false`, and a following
    `dispute,1,1` shall be accepted.
23. If a resolve or chargeback references a deposit that is not `Disputed`, then
    the system shall reject it, and a rejected chargeback shall not lock the
    account.
24. When `deposit,1,1,5.0`, `dispute,1,1`, `chargeback,1,1` are processed, the
    system shall output `1,0.0000,0.0000,0.0000,true`, and a further
    `dispute,1,1` shall be rejected.
25. While an account is locked, the system shall reject its deposits and
    withdrawals with a warning and leave balances unchanged.
26. While an account is locked, the system shall still process dispute, resolve
    and chargeback for its other recorded deposits.

Concurrency

27. When the same input is processed with 1 worker and with 8 workers, the
    system shall produce the same accounts (the order of the output rows is
    not specified).
28. The system shall apply each client's transactions in file order: for an
    input of 10,000 interleaved deposit/withdrawal pairs across 100 clients
    run on 8 workers, no withdrawal is rejected for insufficient funds.
29. While a worker is slower than the reader, the number of queued transactions
    shall not exceed channel capacity per worker. With capacity 1 the pipeline
    shall still complete without deadlock.
30. If a worker task panics, then the system shall exit with code 1, print an
    error to stderr, and write no CSV rows to stdout.

Output

31. The system shall write the header `client,available,held,total,locked`
    followed by one row per known client, in any order.
32. For every output row, `total` shall equal `available + held`.

Documentation

33. The README shall document the assumptions above, the architecture, the
    test strategy and the error-handling choices.

Reader robustness

34. If the header lacks one of the columns `type`, `client`, `tx` and `amount`,
    or repeats one, then reading shall fail with an error naming the column.
    The columns may come in any order.
35. If a single row is longer than 1024 bytes, then the system shall process the
    rows before it, then stop reading with an error.
36. While the input contains many bad rows, the system shall log the first 20
    individually and then only one summary with the total count.
37. The system shall never put the content of a skipped row in a log line.
38. If the input starts with a UTF-8 byte order mark, then the system shall
    ignore it, however the first reads of the input are split.
39. When the header names columns besides the four required ones, the system
    shall still accept rows that stop before them, such as a dispute without an
    amount.

Command line

40. When the engine refuses a transaction, the system shall log a warning with
    the client id, the tx id and a fixed reason, the first 20 refusals one by
    one and then one summary with the total, and shall carry on. The summary is
    also logged when the run ends in an error. The one exception is a refusal
    that means the engine's own balances are inconsistent, which is never held
    back by the cap and is logged at error level; it still counts towards the
    20 and the total.
41. The system shall exit with the same code whether or not stderr can be
    written.

Failure handling

42. If the pipeline future is dropped before it finishes, then the workers and
    the reader shall stop: the reader reads no further rows.
43. If a worker is gone before the input ends, then the reader shall stop and
    the run shall fail, even when no further row belongs to that worker's
    shard. The reader notices within 1024 transactions routed.

## Out of Scope

- Disputing withdrawals.
- A TCP server or multiple simultaneous input streams (the design supports it,
  nothing is built for it).
- Persistence, a database, or spilling the deposit history to disk.
- Unlocking accounts, multiple assets per client, currencies other than the
  single implicit one.
- Detecting duplicate tx ids on withdrawals (withdrawals are not stored).
- Configurable worker count, channel capacity or output format.
- Amounts with more than four decimal places (rejected, not rounded).

## Risks & Open Questions

- **Deposit history is unbounded in memory.** Tx ids can reach 2^32; every
  deposit costs a map entry. Deferred. Documented in the README as a known
  limit; the fix would be an external store.
- **Tx ids are assumed globally unique.** The engine enforces uniqueness per
  client only (keys are `(client, tx)`), because enforcing it globally would
  need coordination between workers and make results depend on the worker
  count. Input that reuses an id across clients is accepted for each client.
  Resolved.
- **Disputes on a locked account.** Assumed allowed (rule 26): freezing stops
  new money moving but does not stop dispute handling. Resolved by default,
  not yet confirmed with a reviewer.
- **Rejected withdrawal creates an empty account** (rule 7). Resolved by
  default; the alternative would hide a client that did appear in the input.
- **Blocking CSV reader inside Tokio.** `spawn_blocking` plus `blocking_send`
  keeps the `csv` crate. For a real multi-stream server an async reader (e.g.
  `csv-async`) would replace it. Deferred. The blocking pool is finite (about
  512 threads), a reader stuck in `read` holds one of them, and the runtime
  waits for it at shutdown; dropping the pipeline future cannot interrupt a
  read in progress. A front end would need read deadlines.
- **One busy client.** All of a client's transactions go to one worker, so a
  single hot client runs at the speed of one worker. Inherent to ordering per
  client.
- **Ordering across multiple streams.** With several concurrent inputs, order
  is only defined per stream. Deferred, relevant only to the server scenario.
- **Warning volume.** Capped per stream: the first 20 skipped rows are logged,
  then one summary line (AC 36); refused transactions are capped the same way
  (AC 40). Both caps are per stream, not per process.
- **Quoted CSV is not supported.** A quoted field is a bad row and is skipped
  with a warning. Accepted: the format has no field that needs quoting, and
  quote handling lets one stray quote swallow the rows after it.
- **A truncated final row is accepted.** Input that stops in the middle of a
  row, without a final line ending, is read as if the row were complete; CSV
  cannot tell the difference. Fine for a file; a network front end would need
  to require a final line ending.
- No feature flag: this is a standalone CLI with no rollout.

## Testing

- **Unit, `Amount`:** table-driven parse, format and overflow cases (AC 7-9).
- **Unit, `Engine`:** table-driven scenarios for every rule and every rejection
  reason (AC 10-26), no async, no I/O.
- **Parsing:** rows with padding, missing amount field, malformed rows (AC 4-6),
  using in-memory readers.
- **Integration:** run the real binary on small hand-written fixtures in
  `tests/fixtures/`, compare output as a set of rows so row order does not
  matter; also usage and file-not-found exit codes (AC 1-3, 31).
- **Concurrency:** seeded pseudo-random transaction generator (`fastrand`, a
  dev-dependency); assert 1 worker equals 8 workers (AC 27), per-client ordering
  (AC 28), capacity-1 channel completes (AC 29). Unit tests in `sharded.rs`
  with stand-in workers: the queue bound for several capacities (AC 29), worker
  panic and early stop (AC 30), dropped future (AC 42) and vanished worker
  (AC 43), and the order in which failures are reported.
- **Logging:** the shared cap and one summary across workers, the summary on a
  read error and on a worker panic, and the error-level exception
  (`sharded.rs`, `refusals.rs`; AC 40); the skipped-rows summary on an early
  drop (`pipeline.rs`; AC 36).
- **Invariant check:** `total == available + held` and `held >= 0` for every
  account of generated runs (`every_account_keeps_total_equal_to_available_plus_held`
  in `tests/concurrency.rs`, 10 seeds; AC 32).
- **Hygiene:** `cargo fmt --check` and `cargo clippy -- -D warnings` clean;
  all tests hermetic and fast.
