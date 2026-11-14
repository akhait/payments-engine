# Payments Engine: decision log

Decisions made during implementation that are not obvious from the spec or the
diff.

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

**Open for Task 8**: both reviewers built a randomized comparison against an
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
  the caller drops the iterator early.
- A truncated final row is read as complete, so a dropped connection can turn
  `...,100.0` into `...,1`. A front end must require a final line ending.
- A trailing comma on every row is rejected as an extra field; leading zeros in
  ids are accepted (they give the same number).
- A reader that returns `Interrupted` forever would spin, as with std's own
  helpers; a reader that returns `Ok(0)` in the middle of a row would split it.
- Moving `tracing-subscriber` into the runtime tree (Task 7) needs a fresh look:
  `env-filter` pulls in more crates, and ANSI output should stay off.
