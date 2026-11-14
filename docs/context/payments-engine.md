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
