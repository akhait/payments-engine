//! The sharded pipeline against a single engine, on generated input.

use std::future::Future;
use std::io::Cursor;
use std::num::NonZeroUsize;
use std::time::Duration;

use payments_engine::account::Account;
use payments_engine::amount::Amount;
use payments_engine::engine::Engine;
use payments_engine::pipeline::{MAX_ROW_BYTES, read_transactions};
use payments_engine::sharded::{PipelineError, run_pipeline};
use payments_engine::transaction::{ClientId, Transaction};

type Accounts = Vec<(ClientId, Account)>;

/// A small deterministic generator (xorshift64*), so that no extra crate is
/// needed and a failing seed can be replayed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }
}

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("a non-zero number")
}

fn amount(text: &str) -> Amount {
    text.parse().expect("a valid amount")
}

const AMOUNTS: [&str; 7] = [
    "0",
    "0.0001",
    "0.5",
    "1.0",
    "2.5",
    "10.0",
    "922337203685477.5807",
];

/// Transactions of every kind over few clients and few tx ids, so that
/// clients are shared by many rows, ids repeat, and disputes often hit.
fn generated(seed: u64, count: usize, clients: u16, tx_ids: u32) -> Vec<Transaction> {
    let mut rng = Rng::new(seed);
    (0..count)
        .map(|_| {
            let client = rng.below(u64::from(clients)) as u16;
            let tx = 1 + rng.below(u64::from(tx_ids)) as u32;
            let amount = amount(AMOUNTS[rng.below(AMOUNTS.len() as u64) as usize]);
            // Chargebacks lock an account for good, so they are kept rare: with
            // more, nearly every account would be locked early and refuse the
            // rest of its rows, and the run would stop testing anything.
            match rng.below(100) {
                0..=44 => Transaction::Deposit { client, tx, amount },
                45..=64 => Transaction::Withdrawal { client, tx, amount },
                65..=84 => Transaction::Dispute { client, tx },
                85..=96 => Transaction::Resolve { client, tx },
                _ => Transaction::Chargeback { client, tx },
            }
        })
        .collect()
}

fn sorted(mut accounts: Accounts) -> Accounts {
    accounts.sort_by_key(|(client, _)| *client);
    accounts
}

/// What one engine gives when it sees the transactions in order.
fn reference(transactions: &[Transaction]) -> Accounts {
    let mut engine = Engine::default();
    for transaction in transactions {
        let _ = engine.apply(*transaction);
    }
    sorted(engine.into_accounts().collect())
}

/// Fails the test instead of hanging it if the pipeline ever stops finishing.
async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), future)
        .await
        .expect("the pipeline finished within 60 seconds")
}

async fn pipeline(transactions: &[Transaction], shards: usize, capacity: usize) -> Accounts {
    // The pipeline owns its input, while the callers keep their transactions
    // to compare against.
    let rows = Vec::from(transactions).into_iter().map(Ok);
    let accounts = within(run_pipeline(rows, nz(shards), nz(capacity)))
        .await
        .expect("the pipeline succeeds");
    sorted(accounts)
}

/// How many of the transactions the engine applies, and how many of the
/// accounts it leaves locked.
fn applied_and_locked(transactions: &[Transaction]) -> (usize, usize, usize) {
    let mut engine = Engine::default();
    let applied = transactions
        .iter()
        .filter(|transaction| engine.apply(**transaction).is_ok())
        .count();
    let accounts: Vec<_> = engine.into_accounts().collect();
    let locked = accounts
        .iter()
        .filter(|(_, account)| account.is_locked())
        .count();
    (applied, accounts.len(), locked)
}

#[test]
fn the_generated_input_applies_a_fair_share_and_does_not_lock_every_account() {
    let transactions = generated(1, 3_000, 12, 40);
    let (applied, accounts, locked) = applied_and_locked(&transactions);
    let total = transactions.len();
    assert!(
        applied * 100 >= total * 25,
        "only {applied} of {total} transactions were applied"
    );
    assert!(locked >= 1, "no account was ever locked");
    assert!(locked < accounts, "all {accounts} accounts ended up locked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_worker_and_eight_workers_give_identical_results() {
    for seed in 1..=20 {
        let transactions = generated(seed, 3_000, 12, 40);
        let one = pipeline(&transactions, 1, 64).await;
        let eight = pipeline(&transactions, 8, 64).await;
        assert_eq!(one, eight, "seed {seed}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_shard_count_and_channel_size_matches_a_single_engine() {
    for seed in 1..=3 {
        let transactions = generated(seed, 2_000, 30, 25);
        let expected = reference(&transactions);
        for shards in [1, 2, 3, 5, 8, 16] {
            for capacity in [1, 2, 64, 1024] {
                let actual = pipeline(&transactions, shards, capacity).await;
                assert_eq!(
                    actual, expected,
                    "seed {seed}, {shards} shards, capacity {capacity}"
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_input_matches_a_single_engine() {
    let transactions = generated(99, 200_000, 500, 400);
    assert_eq!(
        pipeline(&transactions, 8, 256).await,
        reference(&transactions)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_account_keeps_total_equal_to_available_plus_held() {
    for seed in 1..=10 {
        let transactions = generated(seed, 5_000, 20, 60);
        for (client, account) in pipeline(&transactions, 8, 16).await {
            let sum = account.available().checked_add(account.held());
            assert!(
                sum.is_some(),
                "seed {seed}, client {client}: total overflows"
            );
            assert_eq!(account.total(), sum, "seed {seed}, client {client}");
            assert!(
                account.held() >= Amount::ZERO,
                "seed {seed}, client {client}: held is negative"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_clients_transactions_are_applied_in_input_order() {
    let one = amount("1.0");
    let mut transactions = Vec::new();
    for round in 0..100u32 {
        for client in 0..100u16 {
            transactions.push(Transaction::Deposit {
                client,
                tx: round * 2 + 1,
                amount: one,
            });
        }
        for client in 0..100u16 {
            transactions.push(Transaction::Withdrawal {
                client,
                tx: round * 2 + 2,
                amount: one,
            });
        }
    }
    assert_eq!(transactions.len(), 20_000);

    let accounts = pipeline(&transactions, 8, 4).await;
    assert_eq!(accounts.len(), 100);
    for (client, account) in &accounts {
        // A withdrawal applied before its deposit would be refused and leave
        // money behind.
        assert_eq!(account.available(), Amount::ZERO, "client {client}");
        assert_eq!(account.held(), Amount::ZERO, "client {client}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_channel_of_one_does_not_deadlock() {
    let transactions = generated(7, 20_000, 40, 50);
    let expected = reference(&transactions);
    assert_eq!(pipeline(&transactions, 2, 1).await, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_input_gives_no_accounts() {
    assert_eq!(pipeline(&[], 4, 8).await, Accounts::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_error_ends_the_run_with_that_error() {
    let mut bytes = b"type,client,tx,amount\n".to_vec();
    for tx in 1..=1_000u32 {
        bytes.extend(format!("deposit,{},{tx},1.0\n", tx % 7).bytes());
    }
    bytes.extend(std::iter::repeat_n(b'a', MAX_ROW_BYTES + 1));
    bytes.push(b'\n');
    let rows = read_transactions(Cursor::new(bytes)).expect("the header is readable");
    let result = within(run_pipeline(rows, nz(4), nz(8))).await;
    assert!(
        matches!(result, Err(PipelineError::Read(_))),
        "got {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_panic_while_reading_is_reported_and_the_workers_stop() {
    let mut produced = 0u32;
    let rows = std::iter::from_fn(move || {
        produced += 1;
        assert!(produced <= 5, "the input source failed on purpose");
        Some(Ok(Transaction::Deposit {
            client: 1,
            tx: produced,
            amount: amount("1.0"),
        }))
    });
    let result = within(run_pipeline(rows, nz(4), nz(2))).await;
    assert!(
        matches!(result, Err(PipelineError::RouterPanicked)),
        "got {result:?}"
    );
}
