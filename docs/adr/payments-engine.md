# Payments Engine: decision log

Decisions made during implementation that are not obvious from the spec or the
diff.

This is the project's decision log (an ADR). Entries up to and including Task 6
were written before the log adopted numbered entries, and are kept as written.
From Task 7 on, entries are numbered `D1`, `D2`, ... and are append-only: a later
decision that reverses an earlier one is a new entry, and the old one is marked
as superseded.

## 2026-11-14: Task 1: Crate scaffold and `Amount`

**Decision**: `Amount::from_str` accepts `.5` and `5.` (at least one digit
overall), and trims Unicode whitespace.
**Reason**: The spec asks for whitespace tolerance and at least one digit;
`str::trim` matches what `csv::Trim::All` will already have done upstream.
**Alternatives considered**: ASCII-only trimming. Rejected, since it adds a
second definition of "whitespace" for no benefit.

**Decision**: `Display` prints negative amounts with a leading `-`, and `FromStr`
rejects any sign.
**Reason**: `available` can legitimately be negative in output, but input amounts
must never carry a sign. The asymmetry is documented on `Display`.

**Decision**: `SCALE` is derived from `DECIMALS` and the `Display` width uses
`DECIMALS`.
**Reason**: Code review pointed out that two hand-written constants could drift
apart without any compile error.

**Decision**: Added `#![forbid(unsafe_code)]` and `overflow-checks = true` in the
release profile.
**Reason**: Security review hardening. All arithmetic on amounts is already
checked, so this only guards against future unchecked arithmetic panicking
instead of wrapping silently in release builds.

**Decision**: Committed `Cargo.lock` and set `rust-version = "1.85"`.
**Reason**: Edition 2024 needs Rust 1.85, and a committed lock file is what makes
dependency versions actually pinned.

**Decision**: Replaced the `cargo init` hello-world `main` with an empty `main`.
**Reason**: Stdout must carry only the CSV output; the real `main` arrives in
Task 7.

**Decision**: Added boundary tests for results landing exactly on `i64::MAX` /
`i64::MIN` and for a whole part that fits in an `i64` but overflows when scaled.
**Reason**: Review found two mutants (a wrapping multiply, and a rejected result
of exactly `i64::MAX`) that the first 10 tests did not catch. All three
mutants tried afterwards are killed.

**Open**: the reviewers noted that `serde`'s `derive` feature pulls in `syn`
3.0.10, a brand-new major version. Consider an advisory scan (`cargo audit` or
`cargo deny`) before submitting; neither tool is installed locally.

## 2026-11-14: Task 2: Engine with deposits and withdrawals

**Decision**: The deposit history is keyed by `(client, tx)` instead of `tx`.
**Reason**: With a plain `tx` key, a duplicate id across two clients is rejected
by one engine but accepted when the clients sit in different shards, so output
would depend on the worker count (AC 27). Both reviewers found this. A
`(client, tx)` key keeps everything about a client in one engine, drops
`client` from `DepositRecord`, and makes "dispute names the wrong client" fall
out as an ordinary unknown-tx lookup (rule 6).
**Alternatives considered**: Global uniqueness across shards. Rejected: it needs
cross-worker coordination, and the spec already promises globally unique ids,
so well-formed input never exercises the difference.

**Decision**: The duplicate check runs before the account is created.
**Reason**: This ordering alone protects nothing observable (review showed that
swapping it changes no test). What actually guarantees that a rejected deposit
leaves no account behind is the `(client, tx)` key, because a recorded deposit
implies an existing account, together with the fact that crediting a positive
amount to an empty account cannot fail. Task 3 will make this structural by
writing the account and the record only after every check has passed.

**Decision**: `Account` owns balance arithmetic: private fields, `credit` and
`debit` reject non-positive amounts and check overflow. `Engine` only decides
which rules apply.
**Reason**: The engine's positivity check was the only guard, so any future
caller could have credited a negative amount. The engine keeps its own check so
that a refused amount does not create an account.

**Decision**: `Account::debit` reports one error, `InsufficientFunds`, whether
the result would be negative or would underflow.
**Reason**: For a positive amount the two cases mean the same thing, and a
separate overflow branch could not be reached or tested.

**Decision**: `DepositRecord` and `DisputeState` were added in this task rather
than Task 3, and `Account::credit` does not yet check `available + held`.
**Reason**: Recording a deposit needs both types. `held` is always zero until
disputes exist, so a total-overflow check would be untested code; Task 3 adds
it test-first.

**Open for Task 8**: do not log `{:?}` of `Engine` or `Account`. Their derived
`Debug` prints every balance. Log only client and tx ids.

## 2026-11-14: Between Tasks 2 and 3

**Decision**: Keep `Amount` as an `i64` count of ten-thousandths. Anything that
does not fit is rejected, never widened, wrapped or clamped.
**Reason**: The limit is about 9.2 x 10^14 currency units, far beyond a real
balance, so overflow is a guard against garbage or hostile input and not an
expected business path. Rejecting keeps balances correct and the client and tx
ids are logged for reconciliation. `u64` cannot represent the negative
`available` balance that disputes allow. `i128` would remove the case, but
roughly doubles the deposit history, the one structure that can grow to
billions of entries. Because `Amount` is a sealed newtype, switching later is a
change inside `amount.rs` only.
**To do**: state the limit in the README (Task 10).

**Decision**: Tests build amounts with a test-only `Amount::from_units(i64)`
(raw ten-thousandths) instead of parsing strings through `amt()` / `negative()`
helpers.
**Reason**: Simpler call sites, and negative amounts need no workaround since the
parser rejects signs. The constructor is `#[cfg(test)]`, so production code
still has no way to create an amount except parsing. The cost is that readers
convert ten-thousandths (10_000 is 1.0); each test module says so at the top.
**Alternatives considered**: making the field `pub(crate)`. Rejected, since any
module could then bypass the checked arithmetic.

## 2026-11-14: Task 3: Dispute and resolve

**Decision**: `Account` gets `hold` and `release`. The engine picks the deposit
and checks its state; `Account` owns the balance change and refuses anything
that would take `held` below zero (`InsufficientHeld`), even though the engine
never asks for it.
**Reason**: Same split as `credit` / `debit`: invariants live with the data.
Both methods work out both new balances before storing either, so a failure on
the second cannot leave the first changed.

**Decision**: A dispute or resolve changes the deposit's `DisputeState` only
after the account change succeeded.
**Reason**: A refused dispute (for example `held` would overflow) must leave the
deposit `Normal` and spendable. Pinned by
`dispute_that_would_overflow_held_is_rejected_and_state_is_unchanged`.

**Decision**: `Account::credit` now also checks that `available + held` fits.
**Reason**: So `total()` cannot become `None` for an account built through these
methods. Added test-first in two steps: the first version of the test failed in
its setup (`hold` not written yet), so the check went in only after the test was
seen failing at its real assertion.

**Decision**: Rejections added: `UnknownTransaction`, `NotDisputable`,
`NotUnderDispute`, `InsufficientHeld`. A deposit that belongs to another client
and a deposit that does not exist are both `UnknownTransaction`.
**Reason**: Deposits are keyed by `(client, tx)`, so another client's deposit is
simply absent under this client's key.

**Decision**: `deposit` credits an existing account in place and stores a new
account only after its first credit succeeded.
**Reason**: Keeps "a refused deposit leaves nothing behind" structural. A
copy-then-store version does one more hash lookup per deposit (4 operations
instead of 3), which review pointed out for no observable gain.

**Rules confirmed by tests**: a withdrawal cannot spend held funds; disputing an
already-withdrawn deposit leaves `available` negative; a duplicate deposit id is
refused even while the original is disputed; a withdrawal's id is never
disputable.

**Open for Task 4**: a chargeback must follow the same pattern (check, then
change the balance, then change the state) and must check `held >= amount`. After
a chargeback of an already-withdrawn deposit `total` can be negative (deposit 5,
withdraw 5, dispute, chargeback gives available -5, held 0, total -5). Use
checked arithmetic and keep the claim in `credit` that `total()` never fails
true.

**Open for Task 8** (see D20 for what became of the first item): both reviewers built a randomized comparison against an
`i128` reference model, and it caught three breakages the example tests missed.
The spec already plans a deterministic generator; make it compare against such a
model. Log `Rejection::InsufficientHeld` at error level (it means the engine's
bookkeeping is broken), and still never log `{:?}` of `Engine` or `Account`.

## 2026-11-14: Task 4: Chargeback and account locking

**Decision**: The lock rule lives in `Account`. `credit` and `debit` return
`AccountLocked` once the account is locked; `hold`, `release` and `charge_back`
stay allowed.
**Reason**: `Account` guards its own invariants, and rule 26 says freezing stops
funds moving in or out, not dispute handling. The spec table listed "account not
locked" among the engine's checks; the observable behaviour is the same.

**Decision**: `charge_back` sets `locked` as its very last step, after the held
check, and the engine marks the deposit `ChargedBack` only after the account
call succeeded.
**Reason**: A refused chargeback must not lock the account or change anything.
Pinned by tests where another deposit is still held, so a refused chargeback
that moved funds would be visible.

**Decision**: No extra overflow check in `charge_back`.
**Reason**: The argument sits next to the code: the new held balance is at least
zero (the filter) and the old total fitted (guaranteed by `credit`, `hold` and
`release`), so the new total lies between `available` and the old total. A
reviewer checked it by exhaustive search over the real `Account` API at 8-bit
width and by randomized comparison at full width.

**Decision**: A chargeback of an already-withdrawn deposit leaves the account
locked with negative available and total (deposit 5, withdraw 5, dispute,
chargeback gives -5 / 0 / -5).
**Reason**: Rule 3. Because the account is then locked, the engine cannot be
repaid that deficit; the checked arithmetic bounds the negative value.

**Decision**: Removed the `Rejection::Unsupported` placeholder.

**Note**: Which rejection wins when several apply to one transaction (for
example a duplicate deposit id on a locked account reports
`DuplicateTransaction`) is unspecified, and tests avoid pinning it.

**Test strength**: 17 of 18 targeted mutants are killed. The survivor is
equivalent: marking the deposit `ChargedBack` before or after `charge_back`
cannot be observed, since the engine never lets `charge_back` fail.

**Carry to the README (Task 10)**, as explicit assumptions:
1. Dispute, resolve and chargeback rows come from a trusted source. The engine
   cannot tell who sent them, so whoever controls the feed could create a tiny
   deposit for a client and charge it back to lock that client id for good.
2. A lock is permanent; there is no unlock.
3. A locked account can have a negative balance.
4. Withdrawal ids are not recorded, so a replayed withdrawal row applies twice.
5. A withdrawal refused for insufficient funds still creates an empty account.
6. Tx ids are unique per client only, not globally.
7. Balances are limited to about 9.2 x 10^14 units; larger results are refused.

## 2026-11-14: Task 5: Row parsing and streaming reader

**Decision**: `read_transactions` yields `Result<Transaction, ReadError>`. Bad
rows never appear as items: they are logged and skipped inside the iterator. Only
a failing input, or a row over the size limit, is yielded as an error, once, and
then the stream ends. Rows are read with `read_record` plus
`StringRecord::deserialize` (not `deserialize()`) to get row numbers.

**Decision**: Quoting is switched off.
**Reason**: Both reviewers showed that one stray `"` made the reader merge every
following row into one record: one warning, silently lost valid rows, and memory
that grew with the file (about 200 MB for a 70 MB file against 2 MB when clean).
Every field is a keyword or a number, so nothing needs quotes. The price: a
quoted field such as `"deposit"` is now a bad row, skipped with a warning.
**Alternatives considered**: keep quoting and bound the record size. More code,
and it would still let a stray quote swallow rows up to the bound.

**Decision**: A small `RowLimit` reader wrapper fails the stream once a row passes
1024 bytes, and retries interrupted reads.
**Reason**: The csv crate has no size limit. A single endless line cost about 3x
its size in memory, a line of commas about 25x. The error text is fixed and holds
no input. Rows already read are still delivered before the error. It also
protects the header line.

**Decision**: Only the first 20 skipped rows are logged, then one summary with the
total. There is no abort after too many bad rows.
**Reason**: One warning per bad row produced about 70x the input in log output.
Aborting would contradict rule 5 (bad input never aborts the run).

**Decision**: The logged number is the data-row index (`Position::record`), not a
line number.
**Reason**: csv takes the line position before skipping blank lines and before it
consumes the `\n` of a `\r\n`, so line numbers were one or more too low on CRLF or
blank-line input. Found in review because logging was not tested; it is now
tested by capturing the log output with LF, CRLF, blank lines and a bad UTF-8 row.

**Decision**: `client` and `tx` accept ASCII digits only; a row with more fields
than the header is bad; the header must name `type`, `client`, `tx` and `amount`
exactly once each (case sensitive, any order, extra columns ignored). Completely
empty input is accepted and gives no transactions.
**Reason**: csv's integer parsing accepts `+1` and `0x10` (one client, several
spellings). An extra field such as a decimal comma (`1,5`) would have credited 1
and dropped the 5 silently. A missing or differently cased header used to
"succeed" with every row skipped.

**Decision**: Log lines carry a row number and a fixed reason, never row content
(tested with marker text in the bad fields).

**Decision**: `tracing` is used without its default features, and
`tracing-subscriber` is a dev-dependency used only to capture logs in tests.
**Reason**: Dropping the default features removes a proc-macro crate and a second
`syn`. Task 7 needs `tracing-subscriber` for real: move it to the normal
dependencies with the `env-filter` feature, at version 0.3.20 or later
(RUSTSEC-2025-0055, ANSI escape log poisoning, is fixed from 0.3.20; 0.3.23 is
locked).

**Known limits**: A truncated final row is read as complete. `Trim::All` makes csv copy
each record, so the buffer is not allocation-free (memory is bounded by the row
limit, not by reuse). The reader is blocking.

**Test strength**: 45 of 47 targeted mutants are killed. The 2 survivors are
equivalent: the empty-id check (parsing an empty string fails anyway) and the
`serde(default)` on `amount` (serde already defaults an `Option` to `None`).
`csv` itself latches an I/O error and then reports end of input, so the explicit
`finished` flag after an I/O error is redundant for this csv version; it stays so
our contract does not depend on it.

**Carry to the README (Task 10)**: add to the assumptions: quoted CSV is not
supported; the header is required and strict; a row over 1 KiB aborts the run; a
final row without a line ending is accepted; warnings are capped at 20 plus a
summary.

### Task 5 after the re-review

**Correction**: I first wrote that a byte order mark split across reads "fails
loudly". That held for 1 or 2 byte splits only. If the mark arrived as its own
3-byte read, csv took it for the end of the input and the whole stream read as
empty, with exit code 0 and no log line, because empty input is allowed.
**Decision**: A small `SkipBom` wrapper collects the first three bytes before
anything is passed on and drops a mark itself. The mark then also no longer
counts towards the row limit. Tests deliver the input in 9 different read sizes.

**Decision**: A row shorter than the header is padded with empty fields before it
is parsed.
**Reason**: csv's deserializer needs a field for every header column, so with a
header such as `type,client,tx,amount,note` every `dispute,1,1` was dropped,
silently losing disputes and chargebacks. A required field that was missing
becomes an empty one and still fails to parse; an empty amount means none.

**Decision**: The log-capturing tests use one subscriber installed once for the
whole test process, with a separate buffer per thread.
**Reason**: With a subscriber scoped to one thread, `cargo test` failed in about
5% of runs (50 in 800): tracing remembers "nobody is listening" for a log call
site when another test thread reaches it first, and the scoped subscriber then
sometimes misses events. Now 0 failures in 300 runs (200 default, 100 at 32
threads).

**Test strength after the re-review**: the limit is checked with reads of 1, 2,
3, 4, 59, 1023, 1024, 1025 and 4096 bytes. A per-read reset of the row counter
(which would let a hostile row slip past the cap in small reads) is caught, and
so are the 1024 and 20 literals, the summary after an error, the `tx` header and
the log reason. The retry of an interrupted read is checked both at the start of
the input and in the middle of it, because the BOM wrapper otherwise absorbs the
first one.

**Known limits of the design, for the README and any network front end** (from the
security re-review):
- No time or byte budget per stream. A sender of endless line endings, or one
  byte per second, holds its blocking thread; a front end needs idle and total
  timeouts.
- The log cap is per stream, not per process, and the summary is not logged if
  the caller drops the iterator early (the second part superseded by D17).
- A truncated final row is read as complete, so a dropped connection can turn
  `...,100.0` into `...,1`. A front end must require a final line ending.
- A trailing comma on every row is rejected as an extra field; leading zeros in
  ids are accepted (they give the same number).
- A reader that returns `Interrupted` forever would spin, as with std's own
  helpers; a reader that returns `Ok(0)` in the middle of a row would split it.
- Moving `tracing-subscriber` into the runtime tree (Task 7) needs a fresh look:
  `env-filter` pulls in more crates, and ANSI output should stay off.

## 2026-11-14: Task 6: Output writer

**Decision**: `write_accounts` writes the accounts in the order it is given, and
the order of the rows is left unspecified.
**Reason**: The task says the order of the rows does not matter, so sorting would
cost a pass over the accounts for no benefit, and the pipeline's merge step (Task
8) does not have to order anything. Tests compare the rows as a set.

**Decision**: Every total is checked before the header is written, and a total
that cannot be represented fails the call with an error naming the client, with
nothing written.
**Reason**: A half-written report with a wrong or missing row is worse than no
report. The `total()` of an account built through its methods always fits, so
this is a guard that should never fire; it is tested with a test-only
`Account::with_balances`, the same pattern as `Amount::from_units`.

**Decision**: Rows are written with `csv::Writer`: no spaces after commas, `\n`
line endings, amounts through `Amount`'s four-decimal `Display`, and negative
amounts shown with a minus sign.
**Reason**: The brief says spacing and the display of round values do not matter.
A negative `available` or `total` can legitimately occur (a chargeback of a
deposit that was already withdrawn) and the brief does not say how to show it.

**Test strength**: 11 of 12 targeted mutants are killed. The survivor is
equivalent: ignoring the error from writing the header. The csv writer buffers,
so writing a few bytes cannot fail by itself and a failing destination is
reported when the buffer is flushed, which is tested.

**Process**: no reviewer agents for this task, as agreed: it has no rules logic.
Inline spec check, quality gates and mutation checks only.

**For Task 7**: the engine needs a way to hand over its accounts, for example
`Engine::into_accounts()`; add it test-first there.

## D1 — 2026-11-14 — Task 7: Sequential end-to-end CLI

**Status**: Accepted
**Decision**: `RUST_LOG` names a single level (`off`, `error`, `warn`, `info`, `debug`, `trace`, in any case); numbers and per-target directives are not accepted.
**Reason**: It keeps `env-filter` and its regex crates out of the runtime tree, and tracing's own parser accepts `0` to `5` and `+3` and reads an empty string as `error`.
**Alternatives considered**: `EnvFilter` (more dependencies for directives nobody needs here) and `LevelFilter::from_str` (the surprises above).

## D2 — 2026-11-14 — Task 7: Sequential end-to-end CLI

**Status**: Accepted
**Decision**: Usage messages, fatal errors and notices go through a helper that ignores write failures, and tracing's `log_internal_errors` is off.
**Reason**: `eprintln!` panics on a closed stderr, which replaced exit codes 1 and 2 by 101; two reviewers reproduced it with a pipe nobody reads.
**Alternatives considered**: Leaving the defaults, which made the exit code depend on whether stderr could be written.

## D3 — 2026-11-14 — Task 7: Sequential end-to-end CLI

**Status**: Accepted
**Decision**: `run` writes the report only after the whole input was read; a failure while writing can still leave part of the report on stdout.
**Reason**: A report built from half an input looks right but is wrong, while bytes already written to stdout cannot be taken back.
**Alternatives considered**: Building the whole report in memory first, which cannot prevent a short write either.

## D4 — 2026-11-14 — Task 7: Sequential end-to-end CLI

**Status**: Accepted; amended by D9 (a refusal that means broken bookkeeping is not held back by the cap) and by D24 (with several workers the 20 logged are the first 20 recorded)
**Decision**: A refused transaction is logged with its client id, tx id and a fixed reason, the first 20 individually and then one summary count, which is also logged when the run ends in an error.
**Reason**: It is the same flood protection as for skipped rows (2 million refused withdrawals gave 21 lines), and the summary matters most when something went wrong.
**Alternatives considered**: One line per refusal.

## D5 — 2026-11-14 — Task 7: Sequential end-to-end CLI

**Status**: Accepted
**Decision**: `tracing-subscriber` 0.3.23 with only the `fmt` and `std` features is a runtime dependency.
**Reason**: The binary needs it, and without `ansi`, `env-filter` and `tracing-log` only five small crates join the runtime tree, all checked against RustSec (the ANSI log-poisoning advisory is fixed from 0.3.20).
**Alternatives considered**: A hand-written logger, which is more code to maintain.

## D6 — 2026-11-14 — Task 7: Sequential end-to-end CLI

**Status**: Accepted
**Decision**: These limits of the command line are accepted and documented: a closed or read-only stdout gives exit code 0 with no report; `-h` and `--help` are taken as file names; opening a FIFO with no writer blocks; there is no time or byte budget per input; the `RUST_LOG` value is echoed in full in the notice about it.
**Reason**: Rust's standard library treats a closed stdout as success and detecting it would need `unsafe`, which the crate forbids; the brief defines no flags; the rest are concerns of a network front end, not of a batch tool.
**Alternatives considered**: Duplicating stdout's file descriptor to surface the error (not tried, needs platform code), and a `--help` flag (outside the brief).

## D7 — 2026-11-14 — Task 7: Sequential end-to-end CLI

**Status**: Accepted
**Decision**: The CRLF variant of the messy fixture is generated at test time from the LF file.
**Reason**: Git can rewrite line endings in committed files, which would silently turn the CRLF test into a second LF test.
**Alternatives considered**: Committing a CRLF file.

## D8 — 2026-11-14 — Task 8: Sharded concurrent pipeline

**Status**: Accepted
**Decision**: The router, the workers and `run_pipeline` live in a new `sharded` module, and the logging of refused transactions in a new `refusals` module. `pipeline` stays the streaming reader.
**Reason**: The reader and the concurrent layer change for different reasons, and the sequential `run` and the workers both need the refusal log.
**Alternatives considered**: Growing `pipeline.rs`, which was already over a thousand lines with its tests.

## D9 — 2026-11-14 — Task 8: Sharded concurrent pipeline

**Status**: Accepted
**Decision**: All workers share one `RefusalLog`, an atomic counter, so the cap of 20 logged refusals and the summary are per run and not per worker. A refusal that means the engine's own balances are inconsistent (`InsufficientHeld`) is never held back by the cap and is logged at error level, but it is still counted: it takes one of the 20 slots if it comes first and is in the total of the summary. This amends D4.
**Reason**: With a log per worker, 8 workers would log up to 160 lines. Input cannot cause `InsufficientHeld`, so it must not be hidden behind ordinary refusals. The cost is that a broken engine could log without a cap.
**Alternatives considered**: One log per worker, and a second counter with its own cap for the broken case (more code for a case that should never happen).

## D10 — 2026-11-14 — Task 8: Sharded concurrent pipeline

**Status**: Accepted
**Decision**: `run_pipeline` and `run_pipeline_from_reader` are thin wrappers over a private `run_with_workers`, which takes a function that opens the input and a function that spawns a worker.
**Reason**: Tests need workers that are slow, panic or return early, to prove backpressure, shutdown and the error mapping, and none of that can be provoked through the public functions.
**Alternatives considered**: Making the hooks public, which would commit the API to a test need.

## D11 — 2026-11-14 — Task 8: Sharded concurrent pipeline

**Status**: Accepted; the reason for the capacity limit is corrected by D26
**Decision**: The shard count and the channel capacity are `NonZeroUsize`, and both are lowered to a maximum (65,536 shards, the number of client ids; 1,048,576 queued transactions per channel) instead of failing.
**Reason**: Zero would panic in the channel or divide by zero in `shard_of`, and `usize::MAX` would panic in the channel or in `Vec::with_capacity`. A shard past the 65,536th could never receive a transaction.
**Alternatives considered**: Returning an error for out-of-range values, which gives callers one more case to handle for no benefit.

## D12 — 2026-11-14 — Task 8: Sharded concurrent pipeline

**Status**: Accepted
**Decision**: `PipelineError` has a `Stopped` variant for a task that ended without panicking (a cancelled worker, or a worker that went away before the input ended). When several things go wrong, a worker panic is reported first, then a stopped worker, then the outcome of the reader.
**Reason**: Calling every `JoinError` a panic would mislead whoever reads the message, and a failed worker is the likelier cause of a read or routing failure than the reverse.
**Alternatives considered**: One catch-all failure, which hides which part broke.

## D13 — 2026-11-14 — Task 8: Sharded concurrent pipeline

**Status**: Accepted
**Decision**: `run_with_workers`, which both entry points use, holds a guard that aborts the workers when it is dropped, whichever way that happens. Aborting the workers closes their inboxes, which stops the reader at its next send.
**Reason**: Dropping a future does not stop tasks it spawned, so a timeout or a `select!` around the pipeline left the workers and the reader running.
**Alternatives considered**: Documenting that the future must be awaited to the end, which a caller cannot always do.

## D14 — 2026-11-14 — Task 8: Sharded concurrent pipeline

**Status**: Accepted
**Decision**: `run_pipeline_from_reader` opens the input (reads the header) inside the blocking task, and the input is passed to `run_with_workers` as a function that builds it there.
**Reason**: Reading the header on an async thread blocks it for as long as the peer takes to send one, which on a current-thread runtime stalls everything.
**Alternatives considered**: Calling `read_transactions` before the pipeline, as the first version did.

## D15 — 2026-11-14 — Task 8: Sharded concurrent pipeline

**Status**: Accepted; since batching (D26) a send is one batch of up to 256 rows, and the 1024 counts rows routed, whether sent or still buffered
**Decision**: After every 1024 transactions routed the reader checks that no worker has dropped its inbox, and stops with `Stopped` if one has. Rows skipped as bad are not routed and do not count towards the 1024.
**Reason**: A worker that is gone shows when the reader sends to its shard. When the rest of the input names other shards, the reader would read to the end first, which for a long input is a long wait for a result that is already lost.
**Alternatives considered**: Checking on every row, which costs an atomic load per shard per row, and detecting it only through the workers' join handles, which the reader cannot see.

## D16 — 2026-11-14 — Task 8: Sharded concurrent pipeline

**Status**: Accepted; the part about moving `macros` to the runtime features in Task 9 is superseded by D23
**Decision**: `tokio` is a runtime dependency with `rt-multi-thread` and `sync` only. `macros` and `time` are dev-dependency features, and `macros` moves to the runtime features in Task 9, when the CLI gets `#[tokio::main]`.
**Reason**: The library needs neither the macros nor the timers, and a dependency feature should not be enabled before something uses it.
**Alternatives considered**: Enabling `macros` now.

## D17 — 2026-11-14 — Task 8: Sharded concurrent pipeline

**Status**: Accepted
**Decision**: A stream that is dropped before its end (`Transactions`) logs its summary of skipped rows when it is dropped.
**Reason**: The reader is now stopped early when a worker goes away, and the count of skipped rows is the part of the story the caller would otherwise lose.
**Alternatives considered**: Leaving the summary to `next`, which a caller that gives up never reaches.

## D18 — 2026-11-14 — Task 8: Sharded concurrent pipeline

**Status**: Accepted
**Decision**: The generator of the concurrency tests makes chargebacks about 3% of the rows, and a test checks that at least a quarter of the generated transactions are applied and that some but not all accounts end up locked.
**Reason**: A chargeback locks an account for good, and a locked account refuses the rest of its deposits and withdrawals, so too many chargebacks leave a run with little left to compare between worker counts. The first weights made them 8% of the rows. With 3%, about a third of the generated transactions are applied and 8 of 12 accounts end locked (seed 1, 3,000 rows), which the test pins as a floor (the figures are for the first, hand-written generator; see D21 for the current ones).
**Alternatives considered**: Leaving the weights and trusting the comparison.

## D19 — 2026-11-14 — Task 8: Sharded concurrent pipeline

**Status**: Accepted
**Decision**: These limits of the pipeline are accepted and documented: the engine's memory per stream is unbounded (deposit history, one entry per deposit); the blocking pool is finite (about 512 threads), so readers stuck in `read` can starve it; the runtime waits for blocking threads when it shuts down; a hot client runs at the speed of one worker; the cap on logged refusals is per stream, not per process; a reader that stalls holds its thread, so a front end would need read deadlines; and the case where the reader's task ends without panicking has no test, because it cannot be provoked without shutting down the runtime.
**Reason**: They are concerns of a server handling many untrusted streams, not of a batch tool that reads one file. Fixing them would need an external store, an async reader and deadlines.
**Alternatives considered**: An async reader (`csv-async`) and per-stream limits, deferred with the server scenario.

## D20 — 2026-11-14 — Task 8: Sharded concurrent pipeline

**Status**: Open
**Decision**: The randomized tests compare the sharded pipeline with a single `Engine` fed the same transactions. They do not compare the engine with an independent model of the rules, which the "Open for Task 8" note under Task 5 asked for (an `i128` reference model); no such model exists in the suite.
**Reason**: Comparing with one `Engine` proves that sharding changes nothing, which is what Task 8 adds. The rules themselves are covered by the table-driven tests of Tasks 2 to 4, and a reference model would be a second implementation of them. Nobody has decided whether it is worth building.
**Alternatives considered**: Building the reference model in Task 8, which would grow a task about concurrency into one about the rules.

## D21 — 2026-11-15 — Task 8: Sharded concurrent pipeline

**Status**: Accepted
**Decision**: The seeded test generator uses `fastrand` 2.5.0, a dev-dependency, instead of a hand-written xorshift64* generator.
**Reason**: There is less code to read and trust, and the crate gives a range API (`rng.u16(0..n)`) instead of a modulo. It has no dependencies of its own and is not in the release tree. Version 2.5.0 was published on 2026-07-19 and is not yanked, and RustSec has no advisory for the crate. After the swap, seeds 1 to 12 apply between 767 and 945 of 3,000 generated transactions (26% to 32%) and leave between 6 and 10 of 12 accounts locked, so the quality test now checks all twelve seeds against a floor of 20% applied, with some but not all accounts locked.
**Cost**: A seed replays the same input only while `Cargo.lock` keeps the same version of `fastrand`, and the generated input differs from before. The tests compare the pipeline with one engine, so no expected value had to change, and the mutants that change the pipeline's output were killed again with the new generator.
**Alternatives considered**: `rand`, with a larger tree and a seeded stream that is not stable across versions; `proptest`, which shrinks failures but is heavy and awkward with async tests; keeping the hand-written generator, whose stream never changes.

## D22 — 2026-11-15 — Task 8: Sharded concurrent pipeline

**Status**: Accepted
**Decision**: A worker that has gone away is not replaced. The run stops with `PipelineError::WorkerPanicked` or `Stopped`, no accounts are returned, and the CLI exits with code 1.
**Reason**: A worker's state is its `Engine`, the accounts and deposit history of its clients, and it is lost with the worker, together with the transactions still queued in its inbox. We also cannot tell whether the transaction it was applying took effect. A fresh worker would start empty and report balances that look valid and are wrong: withdrawals refused for insufficient funds, disputes refused for unknown deposits. A panic also means a broken invariant, so replaying the same input would probably fail the same way. For a batch tool that reads one file, stopping and rerunning is correct.
**Cost**: One failed worker loses the whole run, however much of the input was already processed.
**Alternatives considered**: Respawning with an empty engine, which gives wrong balances; keeping every transaction of each shard in memory to replay it, which gives up the bounded memory of the streaming design; snapshotting the engines and replaying from the last snapshot, or a durable per-shard log, which only a long-running server with many streams would need. The last two are deferred with the server scenario (D19).

## D23 — 2026-11-15 — Task 9: Switch the CLI to the concurrent pipeline

**Status**: Accepted (corrected during the review of Task 9; the first version gave a reason that was wrong, see below)
**Decision**: `main` builds the Tokio runtime with `tokio::runtime::Builder` and calls `block_on`, instead of using `#[tokio::main]`, and sets the number of worker threads explicitly, to the same number as the shards. Tokio's `macros` feature stays a dev-dependency feature. This supersedes the plan's wording and the second half of D16.
**Reason**: Left unset, tokio reads `TOKIO_WORKER_THREADS` and panics on a value it cannot use (empty, `0`, negative, not a number, not UTF-8), which gave exit code 101 in the first version of this change; D2 keeps the exit codes to 0, 1 and 2. Setting the count means tokio never reads the variable. Building the runtime by hand also keeps `tokio-macros` out of the release build.
**Accepted limit**: The first version of this entry said a hand-built runtime turns a failure to start into an ordinary error. That was wrong. With only the `rt-multi-thread` and `sync` features there is no driver to fail, and if the operating system refuses to start a thread, tokio panics (in `build`, and in `spawn_blocking` for the reader), which gives exit code 101 however the runtime is built. That is accepted: it comes from the environment and not from the input, and nothing reaches stdout. The `map_err` in `main` is a safeguard and has no test.
**Alternatives considered**: `#[tokio::main]`, which is what the plan said and which reads the same environment variable; catching the panic, which is not worth it for an environment that cannot run the program anyway.

## D24 — 2026-11-15 — Task 9: Switch the CLI to the concurrent pipeline

**Status**: Accepted
**Decision**: `run` is async and uses the sharded pipeline. It takes the shard count and the channel capacity as parameters, and the command line passes one worker per core (`default_shards()`, from `available_parallelism()`, minimum 1) and a channel of 1024 transactions per worker (`DEFAULT_CHANNEL_CAPACITY`). The sequential path is removed. `RunError::Read` becomes `RunError::Pipeline`, and the report is written by a small `report` function that does nothing when the pipeline failed.
**Reason**: The binary has one path, so what the tests exercise is what ships. The sequential implementation is not needed as a reference: the concurrency tests compare the pipeline with one `Engine` (see D20 for what that does and does not prove).
**Cost**: Workers record refusals concurrently, so the log lines are in no fixed order, and which refusals fall in the first 20 can differ between runs. The rows of the report do not differ. Two reviewers timed both versions independently on generated files of about 3 million rows and 5,000 clients (80 to 90 MB, 10 cores, release build). They found 2.0 to 2.5 s for the sequential path against 2.4 to 3.0 s for this one, and 5.3 to 5.5 s of CPU time against about 2.1 s, with identical reports. I did not run the measurement myself. Parsing the CSV costs the same in both versions. The likely cause of the difference is that the concurrent version adds a channel send, and often a thread wake-up, for every row on the reader's critical path, which costs more than the apply work it moves to the workers; the system time (about 0.03 s sequential against 1.2 to 2.1 s concurrent in one reviewer's runs) points to the wake-ups. That was not confirmed by profiling. Batching fixed it, see D26. Nothing tests that the binary starts more than one worker: the wiring of `default_shards()` in `main` is unchecked.
**Alternatives considered**: Keeping the sequential `run` beside the new one, which leaves dead code in the binary and two paths to keep the same.

## D25 — 2026-11-15 — Task 9: Switch the CLI to the concurrent pipeline

**Status**: Accepted; since batching (D26), "the next send to it" is the next batch for its shard, up to 256 of its rows later, and the bound of 1024 rows routed is unchanged
**Decision**: The command line does nothing extra about a worker that goes away or about blocking threads at shutdown. A dead worker is noticed at the next send to it, or within 1024 transactions routed (D15), and the run exits with code 1 and nothing on stdout. No shutdown timeout is set on the runtime.
**Reason**: `run` awaits the reader to its end on every path, and the future is never dropped, so no blocking thread outlives it and the runtime has nothing to wait for. A reader that hangs inside `read` hangs the run itself, which D6 and D19 already accept.
**Alternatives considered**: `Runtime::shutdown_timeout`, which only helps when a task outlives the run, and none does.

## D26 — 2026-11-15 — Task 8: Sharded concurrent pipeline

**Status**: Accepted
**Decision**: The router sends transactions to a worker in batches of up to 256 instead of one message per transaction. `capacity` still counts transactions: the channel holds `capacity / batch` batches (at least one), and a capacity below 256 gives batches of that size, so a capacity of 1 still means one transaction at a time. The router sends what it has buffered when the input ends and when it fails to read, so the workers see every transaction that was read; a worker that is gone, or a panic in the input, skips that, and only the refusal counts in the summary are affected, since the run fails anyway. If the input fails and the flush shows a worker to be gone, the worker is what is reported, as D12 says.
**Reason**: One message per transaction made the concurrent pipeline slower than the sequential one it replaced (D24). I measured it on a generated file of 3 million rows and 5,000 clients (80 MB, release build, a 10-core machine, five runs of each build alternating, `RUST_LOG=off`, reports with the same rows):

| Build | Wall s (median) | CPU s (user + sys) | Peak MB |
|---|---|---|---|
| Sequential (Task 8 commit) | 2.13 to 2.14 | 2.1 | 108 |
| One message per transaction | 2.66 and 3.15 (two sessions) | 5.4 and 6.6 | 126 to 129 |
| Batches of 16 | 1.84 | 2.75 | 127 |
| Batches of 64 | 1.73 | 2.30 | 126 |
| Batches of 256 | 1.72 | 2.13 | 127 |
| Batches of 1024 | 1.71 | 2.03 | 129 |

From 64 up the time is the same within noise, so 256 sits on the plateau, with CPU time equal to the sequential build. The gain over sequential is the apply work moving to the workers, and it is small because the reader, which parses the CSV, remains the limit; a parallel parser would be needed to go further.
**Cost**: The router buffers up to 255 transactions for each shard. A buffer grows as rows arrive, so a shard that gets few rows holds little, and only a shard that has filled a batch keeps room for a full one (4 KiB with 16-byte transactions, which a reviewer measured), so the memory is proportional to the input. The first version reserved a full buffer on a shard's first row; a reviewer measured 442 MB of peak memory against 154 MB for a file of 65,536 rows naming every client id, with a library caller asking for 65,536 shards (the command line is not affected, as it uses one shard per core), and growing the buffers on demand removed that. A worker can be behind by `capacity` plus two batches (the channel, the batch the router waits to send, and the batch the worker is applying). A transaction waits for its batch to fill; that does not matter for a file, but a front end serving live streams would need a flush timeout. AC 29 now says the bound excludes the router's one batch.
**Correction to D11**: the capacity limit no longer protects the channel from a panic, because the limit is divided by the batch size before it reaches the channel. It still bounds the memory a stalled worker can pin, which is why it stays.
**Alternatives considered**: One message per transaction (slower than sequential); a batch of 1024 or more (the same time, more memory in flight); parsing in parallel (a different design, not needed for a file); an unbounded channel (no backpressure).

## D27 — 2026-11-15 — Task 10: README and final verification

**Status**: Accepted
**Decision**: The spec, the plan and this decision log stay in the repository, and the README points to them in a section on how the project was built.
**Reason**: The author wants a reviewer to be able to follow the way the work was done: the spec before the code, the plan with a commit for each task, the decisions with their alternatives, and what the independent reviews found. The documents are written in the author's own words and do not reproduce the task statement or its example rows.
**Alternatives considered**: Removing them before submission, which was the plan's default at the end of Task 10.

## D28 — 2026-11-15 — Task 10: README and final verification

**Status**: Accepted
**Decision**: The README states the measured memory and time of the large-input check, and gives the deposit history as the one limit on memory.
**Reason**: I ran the release build on generated files (5,000 clients, `RUST_LOG=off`, median of three runs, a 10-core machine):

| Input | Size | Wall s | CPU s | Peak MB |
|---|---|---|---|---|
| 1 million rows, with deposits | 26 MB | 0.58 | 0.70 | 29 |
| 3 million rows, with deposits | 80 MB | 1.75 | 2.16 | 125 |
| 9 million rows, with deposits | 243 MB | 5.27 | 6.75 | 521 |
| 9 million rows, no deposits | 257 MB | 5.02 | 5.41 | 4 |

The sequential build (commit `a327fa2`) took 6.74 s and 422 MB on the 9-million-row file with deposits, and 5.07 s and 3 MB on the one without. Memory therefore does not depend on the size of the input: the file with no deposits peaks at 4 MB however long it is. It grows with the number of deposits recorded, at roughly 45 to 105 bytes each (about 100 bytes at the largest size, where about 5 million deposits are recorded), and the concurrent build uses about 100 MB more than the sequential one at that size. The concurrent build is faster on the largest file (5.3 s against 6.7 s) and the same where nothing is applied, because parsing is the limit there.
**Alternatives considered**: Not measuring a file larger than 3 million rows, which would have left the growth with deposits unquantified.
