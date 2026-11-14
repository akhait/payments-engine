//! Logging of the transactions the engine refuses.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::engine::Rejection;
use crate::transaction::Transaction;

/// How many refusals are logged one by one. The rest are only counted, so a
/// hostile input cannot flood the log.
pub(crate) const MAX_LOGGED_REJECTIONS: u64 = 20;

/// Counts refused transactions and logs the first few.
///
/// It can be shared between threads: one log is meant to serve a whole run, so
/// the cap holds however many workers there are.
#[derive(Debug, Default)]
pub(crate) struct RefusalLog {
    refused: AtomicU64,
}

impl RefusalLog {
    /// Counts a refusal and logs it if it is among the first few. Returns
    /// whether this one was logged.
    ///
    /// A refusal that means the engine's own balances are inconsistent
    /// ([`Rejection::InsufficientHeld`]) is always logged, at error level, and
    /// is not held back by the cap: input cannot cause it, so it must not be
    /// hidden behind ordinary refusals.
    ///
    /// Only the client and tx ids and the reason are logged, never amounts.
    pub(crate) fn record(&self, transaction: &Transaction, reason: Rejection) -> bool {
        let before = self.refused.fetch_add(1, Ordering::Relaxed);
        let broken_bookkeeping = reason == Rejection::InsufficientHeld;
        if before >= MAX_LOGGED_REJECTIONS && !broken_bookkeeping {
            return false;
        }
        let (client, tx) = (transaction.client(), transaction.tx());
        if broken_bookkeeping {
            tracing::error!(
                client,
                tx,
                %reason,
                "transaction rejected, the engine's balances are inconsistent"
            );
        } else {
            tracing::warn!(client, tx, %reason, "transaction rejected");
        }
        true
    }

    /// Logs how many refusals there were in all, if there were any.
    pub(crate) fn log_summary(&self) {
        let rejected = self.refused.load(Ordering::Relaxed);
        if rejected > 0 {
            tracing::warn!(rejected, "transactions rejected in total");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::amount::Amount;
    use crate::test_support::capture_logs;

    fn deposit(client: u16, tx: u32) -> Transaction {
        Transaction::Deposit {
            client,
            tx,
            amount: Amount::from_units(10_000),
        }
    }

    #[test]
    fn the_first_refusals_are_logged_with_client_tx_and_reason() {
        let log = RefusalLog::default();
        let (logged, logs) = capture_logs(|| {
            [
                log.record(&deposit(1, 7), Rejection::InsufficientFunds),
                log.record(&deposit(2, 8), Rejection::AccountLocked),
            ]
        });
        assert_eq!(logged, [true, true]);
        let first = logs.lines().next().expect("a first log line");
        assert!(first.trim_start().starts_with("WARN "), "{first}");
        assert!(first.contains("client=1 tx=7 "), "{first}");
        assert!(first.contains("insufficient available funds"), "{first}");
        let second = logs.lines().nth(1).expect("a second log line");
        assert!(second.contains("client=2 tx=8 "), "{second}");
        assert!(second.contains("account is locked"), "{second}");
    }

    #[test]
    fn only_the_first_refusals_are_logged_and_the_rest_are_counted() {
        let log = RefusalLog::default();
        let (logged, logs) = capture_logs(|| {
            let logged: Vec<bool> = (1..=25)
                .map(|tx| log.record(&deposit(1, tx), Rejection::InsufficientFunds))
                .collect();
            log.log_summary();
            logged
        });
        assert_eq!(logged.iter().filter(|&&was_logged| was_logged).count(), 20);
        assert!(logged[..20].iter().all(|&was_logged| was_logged));
        assert!(logged[20..].iter().all(|&was_logged| !was_logged));
        assert!(
            logs.contains("tx=20 ") && !logs.contains("tx=21 "),
            "{logs}"
        );
        let summary = logs
            .lines()
            .find(|line| line.contains("transactions rejected in total"))
            .expect("a summary line");
        assert!(summary.contains("rejected=25"), "{summary}");
    }

    #[test]
    fn nothing_is_logged_when_nothing_was_refused() {
        let (_, logs) = capture_logs(|| RefusalLog::default().log_summary());
        assert_eq!(logs, "");
    }

    #[test]
    fn a_single_refusal_still_gets_a_summary() {
        let log = RefusalLog::default();
        let (_, logs) = capture_logs(|| {
            log.record(&deposit(1, 1), Rejection::Overflow);
            log.log_summary();
        });
        assert!(logs.contains("rejected=1"), "{logs}");
    }

    #[test]
    fn the_cap_holds_when_many_threads_record_refusals_at_once() {
        // The threads wait for each other at a barrier, so that they start
        // recording together instead of one after another. Every thread logs
        // on its own thread, so the lines are counted per thread and added up.
        const THREADS: usize = 8;
        let log = Arc::new(RefusalLog::default());
        let barrier = Arc::new(std::sync::Barrier::new(THREADS));
        let threads: Vec<_> = (0..THREADS as u16)
            .map(|client| {
                let log = Arc::clone(&log);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    capture_logs(|| {
                        barrier.wait();
                        (1..=100)
                            .filter(|&tx| log.record(&deposit(client, tx), Rejection::Overflow))
                            .count()
                    })
                })
            })
            .collect();
        let mut logged_individually = 0;
        let mut lines = 0;
        for thread in threads {
            let (count, logs) = thread.join().expect("a recording thread");
            logged_individually += count;
            lines += logs
                .lines()
                .filter(|line| line.contains("transaction rejected"))
                .count();
        }
        assert_eq!(logged_individually, 20);
        assert_eq!(lines, 20);
        let (_, logs) = capture_logs(|| log.log_summary());
        assert!(logs.contains("rejected=800"), "{logs}");
    }

    #[test]
    fn broken_bookkeeping_is_logged_even_after_the_cap_is_reached() {
        let log = RefusalLog::default();
        let (logged, logs) = capture_logs(|| {
            for tx in 1..=25 {
                log.record(&deposit(1, tx), Rejection::InsufficientFunds);
            }
            log.record(&deposit(2, 99), Rejection::InsufficientHeld)
        });
        assert!(logged, "the refusal was not reported as logged");
        let line = logs
            .lines()
            .find(|line| line.contains("client=2 tx=99 "))
            .unwrap_or_else(|| panic!("the error was not logged: {logs}"));
        assert!(line.trim_start().starts_with("ERROR "), "{line}");
    }

    #[test]
    fn refusals_past_the_cap_are_not_logged_even_when_broken_bookkeeping_came_first() {
        let log = RefusalLog::default();
        let (logged, logs) = capture_logs(|| {
            log.record(&deposit(1, 1), Rejection::InsufficientHeld);
            (2..=30)
                .map(|tx| log.record(&deposit(1, tx), Rejection::InsufficientFunds))
                .filter(|&was_logged| was_logged)
                .count()
        });
        // One slot of the cap went to the error, so 19 ordinary ones fit.
        assert_eq!(logged, 19);
        assert_eq!(logs.lines().count(), 20, "{logs}");
    }

    #[test]
    fn a_refusal_that_means_broken_bookkeeping_is_logged_as_an_error() {
        let log = RefusalLog::default();
        let (_, logs) = capture_logs(|| log.record(&deposit(1, 1), Rejection::InsufficientHeld));
        let line = logs.lines().next().expect("a log line");
        assert!(line.trim_start().starts_with("ERROR "), "{line}");
        assert!(line.contains("not enough held funds"), "{line}");
    }
}
