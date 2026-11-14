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
no shared mutable state. The business rules live in a plain synchronous
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
 (spawn_blocking,    router├──────────────────────────────► worker 1 ─┤─► merge ─► csv writer ─► stdout
  streaming)    shard=id%N │                                           │
                           └──────────────────────────────► worker N-1┘
```

Shutdown is driven by channel closure: the reader finishes, its `Sender`s are
dropped, each worker drains its queue and returns its `Engine`, and `main`
joins the workers and merges their account maps (client sets are disjoint).

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
- `DepositRecord { client: ClientId, amount: Amount, state: DisputeState }`.
  Only deposits are stored, because only deposits can be disputed.
- `Transaction` enum: `Deposit{client, tx, amount}`, `Withdrawal{..}`,
  `Dispute{client, tx}`, `Resolve{client, tx}`, `Chargeback{client, tx}`.
  Built from a raw CSV row via `TryFrom`, so a deposit without an amount is
  unrepresentable past the parsing boundary.
- `Rejection` enum with a `Display` impl: why a transaction was not applied.

### Engine (synchronous, no async)

`Engine { accounts: HashMap<ClientId, Account>, deposits: HashMap<TxId, DepositRecord> }`
with `apply(&mut self, Transaction) -> Result<(), Rejection>`.
A rejected transaction never changes state.

| Transaction | Valid when | Effect |
|---|---|---|
| deposit | amount > 0; account not locked; `tx` not already a recorded deposit; no overflow | `available += amount`; record deposit as `Normal`; account created if missing |
| withdrawal | amount > 0; account not locked; `available >= amount` | `available -= amount`; not recorded |
| dispute | `tx` is a recorded deposit of **this client**; state is `Normal` | `available -= amount`, `held += amount`; state `Disputed` |
| resolve | same client; state is `Disputed` | `held -= amount`, `available += amount`; state `Normal` |
| chargeback | same client; state is `Disputed` | `held -= amount`; state `ChargedBack`; `locked = true` |

Agreed rules and assumptions:

1. Only deposits can be disputed.
2. Deposits and withdrawals on a locked account are rejected and logged at
   `warn`.
3. `available` may go negative after a dispute (client withdrew, then disputed
   the funding deposit). `held` is never negative by construction.
4. A resolved deposit returns to `Normal` and may be disputed again.
   `ChargedBack` is terminal.
5. Bad input never aborts the run. The row is skipped or the transaction
   rejected, a `warn` line goes to stderr, and processing continues.
6. A dispute, resolve or chargeback whose `client` differs from the stored
   deposit's client is rejected. This also makes cross-shard lookups
   unnecessary.
7. Dispute, resolve and chargeback never create an account. A deposit or
   withdrawal with a valid amount does, even if the withdrawal is then rejected
   for insufficient funds (the client exists and is reported with zero
   balances).
8. Every mutation uses checked arithmetic, including `available + held`. On
   overflow the transaction is rejected.

### Reader / router (Tokio layer)

- `csv::ReaderBuilder` with `trim(Trim::All)` and `flexible(true)` (dispute,
  resolve and chargeback rows omit the trailing amount column). `amount` is
  `Option<Amount>` with `#[serde(default)]`.
- Rows are iterated with `deserialize()`, so only one record is in memory at a
  time.
- `csv` is blocking, so the reader runs inside `tokio::task::spawn_blocking`
  and hands rows over with `Sender::blocking_send`. A blocking read on an async
  worker thread would stall other tasks.
- A deserialize error on a row: warn with the row number, skip. An I/O error
  while reading: abort the run with a non-zero exit.
- Channels are bounded (constant capacity, e.g. 1024 per worker). When a worker
  is slow its channel fills and `send` suspends the reader (backpressure), so
  memory stays flat regardless of file size.
- Worker count `N` is `available_parallelism()`, minimum 1.
- Workers are async tasks looping on `rx.recv().await` and calling the
  synchronous `Engine::apply`. Rejections are logged with client and tx ids.

### Output

- After all workers return, merge the account maps and write them with
  `csv::Writer` to stdout, in no particular order.
- Header `client,available,held,total,locked`; amounts via `Amount::Display`
  (four decimals); `locked` as `true` / `false`.

### Errors and logging

- `tracing` + `tracing-subscriber` writing to **stderr** only; default filter
  `warn`, overridable with `RUST_LOG`. Stdout carries nothing but the CSV.
- Exit codes: `0` success; `1` runtime failure (file cannot be opened, I/O
  error, worker panic); `2` usage error.
- A worker panic surfaces as a `JoinError`. The run fails with a message on
  stderr and no partial CSV is written.

### Layout

Crate `payments_engine` (library + binary):

```
src/main.rs      CLI parsing, runtime, exit codes
src/lib.rs       module wiring, public entry point
src/amount.rs    Amount newtype
src/transaction.rs  raw row, Transaction, TryFrom
src/account.rs   Account, DepositRecord, DisputeState
src/engine.rs    Engine, Rejection, rules
src/pipeline.rs  reader, router, workers, merge
src/output.rs    CSV writer
tests/           integration tests + fixtures
```

Dependencies: `tokio` (rt-multi-thread, macros, sync), `csv`, `serde`,
`tracing`, `tracing-subscriber`. No others unless a need shows up.

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
   0..=4294967295, a non-numeric field, a missing amount on a deposit or
   withdrawal, a negative amount, or an amount with more than four decimal
   places, then the system shall log a warning containing the row number, skip
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
15. If a deposit reuses a tx id already recorded as a deposit, then the system
    shall reject it and leave balances unchanged.

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
- **Disputes on a locked account.** Assumed allowed (rule 26): freezing stops
  new money moving but does not stop dispute handling. Resolved by default,
  not yet confirmed with a reviewer.
- **Rejected withdrawal creates an empty account** (rule 7). Resolved by
  default; the alternative would hide a client that did appear in the input.
- **Blocking CSV reader inside Tokio.** `spawn_blocking` plus `blocking_send`
  keeps the `csv` crate. For a real multi-stream server an async reader (e.g.
  `csv-async`) would replace it. Deferred.
- **Ordering across multiple streams.** With several concurrent inputs, order
  is only defined per stream. Deferred, relevant only to the server scenario.
- **Warning volume.** A file full of bad rows produces one stderr line each.
  Accepted.
- **Unchecked claim to verify early:** a missing trailing field really
  deserializes to `None` with `flexible(true)` and a header row. Needs a first
  failing test (AC 5).
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
- **Concurrency:** deterministic pseudo-random transaction generator (no extra
  crate); assert 1 worker equals 8 workers (AC 27), per-client ordering
  (AC 28), capacity-1 channel completes (AC 29), worker panic path (AC 30).
- **Invariant check:** after every generated run, `total == available + held`
  for every account (AC 32).
- **Hygiene:** `cargo fmt --check` and `cargo clippy -- -D warnings` clean;
  all tests hermetic and fast.
