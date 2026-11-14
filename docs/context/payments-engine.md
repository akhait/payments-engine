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
