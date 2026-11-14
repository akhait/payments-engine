//! The ledger rules: which transactions are accepted and what they change.

use std::collections::HashMap;
use std::fmt;

use crate::account::{Account, BalanceError, DepositRecord, DisputeState};
use crate::amount::Amount;
use crate::transaction::{ClientId, Transaction, TxId};

/// Why a transaction was not applied.
///
/// A rejected transaction changes nothing, with one exception: a withdrawal
/// refused for insufficient funds still creates the client's (empty) account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// A deposit or withdrawal amount was zero or negative.
    NonPositiveAmount,
    /// A deposit reused the id of an earlier deposit by the same client.
    DuplicateTransaction,
    /// A withdrawal asked for more than the available balance.
    InsufficientFunds,
    /// Applying the transaction would overflow a balance.
    Overflow,
    /// Disputes, resolves and chargebacks are not implemented yet.
    Unsupported,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NonPositiveAmount => "amount must be greater than zero",
            Self::DuplicateTransaction => "this client already has a deposit with this id",
            Self::InsufficientFunds => "insufficient available funds",
            Self::Overflow => "balance would overflow",
            Self::Unsupported => "transaction type is not supported yet",
        };
        f.write_str(message)
    }
}

impl std::error::Error for Rejection {}

impl From<BalanceError> for Rejection {
    fn from(error: BalanceError) -> Self {
        match error {
            BalanceError::NonPositiveAmount => Rejection::NonPositiveAmount,
            BalanceError::InsufficientFunds => Rejection::InsufficientFunds,
            BalanceError::Overflow => Rejection::Overflow,
        }
    }
}

/// Deposits are remembered per client: the same tx id used by two different
/// clients is two different deposits. Everything the engine knows about a
/// client therefore lives in one engine, so the result does not depend on how
/// clients are spread over several engines.
type DepositKey = (ClientId, TxId);

/// Holds every account and the deposit history, and applies transactions.
///
/// The engine is synchronous and knows nothing about threads or I/O. It
/// expects to see each client's transactions in chronological order.
#[derive(Debug, Default)]
pub struct Engine {
    accounts: HashMap<ClientId, Account>,
    deposits: HashMap<DepositKey, DepositRecord>,
}

impl Engine {
    /// Looks up a client's account, if the client has appeared.
    pub fn account(&self, client: ClientId) -> Option<&Account> {
        self.accounts.get(&client)
    }

    /// Applies one transaction, or explains why it was refused.
    pub fn apply(&mut self, transaction: Transaction) -> Result<(), Rejection> {
        match transaction {
            Transaction::Deposit { client, tx, amount } => self.deposit(client, tx, amount),
            Transaction::Withdrawal { client, amount, .. } => self.withdraw(client, amount),
            Transaction::Dispute { .. }
            | Transaction::Resolve { .. }
            | Transaction::Chargeback { .. } => Err(Rejection::Unsupported),
        }
    }

    fn deposit(&mut self, client: ClientId, tx: TxId, amount: Amount) -> Result<(), Rejection> {
        if !amount.is_positive() {
            return Err(Rejection::NonPositiveAmount);
        }
        // Check for a duplicate before creating the account, so a refused
        // deposit never leaves anything behind.
        let key = (client, tx);
        if self.deposits.contains_key(&key) {
            return Err(Rejection::DuplicateTransaction);
        }
        self.accounts.entry(client).or_default().credit(amount)?;
        // Credit first: a deposit is only remembered once its funds were added.
        self.deposits.insert(
            key,
            DepositRecord {
                amount,
                state: DisputeState::Normal,
            },
        );
        Ok(())
    }

    fn withdraw(&mut self, client: ClientId, amount: Amount) -> Result<(), Rejection> {
        if !amount.is_positive() {
            return Err(Rejection::NonPositiveAmount);
        }
        // The client exists from here on, even if the withdrawal is refused below.
        let account = self.accounts.entry(client).or_default();
        account.debit(amount)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: &str = "922337203685477.5807";

    fn amt(s: &str) -> Amount {
        s.parse().expect("valid amount in test")
    }

    fn deposit(client: ClientId, tx: TxId, amount: &str) -> Transaction {
        Transaction::Deposit {
            client,
            tx,
            amount: amt(amount),
        }
    }

    fn withdrawal(client: ClientId, tx: TxId, amount: &str) -> Transaction {
        Transaction::Withdrawal {
            client,
            tx,
            amount: amt(amount),
        }
    }

    fn negative(amount: &str) -> Amount {
        Amount::ZERO
            .checked_sub(amt(amount))
            .expect("a small amount cannot underflow")
    }

    fn apply_all(engine: &mut Engine, transactions: &[Transaction]) {
        for transaction in transactions {
            engine
                .apply(*transaction)
                .unwrap_or_else(|why| panic!("{transaction:?} was rejected: {why}"));
        }
    }

    fn assert_balances(
        engine: &Engine,
        client: ClientId,
        available: &str,
        held: &str,
        total: &str,
    ) {
        let account = engine
            .account(client)
            .unwrap_or_else(|| panic!("client {client} has no account"));
        assert_eq!(account.available(), amt(available), "available");
        assert_eq!(account.held(), amt(held), "held");
        assert_eq!(account.total(), Some(amt(total)), "total");
    }

    #[test]
    fn deposits_increase_available_and_total() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, "1.0"), deposit(1, 2, "2.0")]);
        assert_balances(&engine, 1, "3.0", "0", "3.0");
    }

    #[test]
    fn withdrawal_decreases_available_and_total() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[deposit(1, 1, "3.0"), withdrawal(1, 2, "1.5")],
        );
        assert_balances(&engine, 1, "1.5", "0", "1.5");
    }

    #[test]
    fn withdrawing_the_exact_balance_is_allowed() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[deposit(1, 1, "2.0"), withdrawal(1, 2, "2.0")],
        );
        assert_balances(&engine, 1, "0", "0", "0");
    }

    #[test]
    fn withdrawal_exceeding_available_is_rejected_and_changes_nothing() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(2, 1, "2.0")]);
        assert_eq!(
            engine.apply(withdrawal(2, 2, "3.0")),
            Err(Rejection::InsufficientFunds)
        );
        assert_balances(&engine, 2, "2.0", "0", "2.0");
    }

    #[test]
    fn first_withdrawal_it_cannot_cover_still_creates_the_account() {
        let mut engine = Engine::default();
        assert_eq!(
            engine.apply(withdrawal(7, 1, "1.0")),
            Err(Rejection::InsufficientFunds)
        );
        assert_balances(&engine, 7, "0", "0", "0");
    }

    #[test]
    fn zero_amounts_are_rejected_without_creating_an_account() {
        let mut engine = Engine::default();
        assert_eq!(
            engine.apply(deposit(1, 1, "0")),
            Err(Rejection::NonPositiveAmount)
        );
        assert_eq!(
            engine.apply(withdrawal(2, 2, "0.0")),
            Err(Rejection::NonPositiveAmount)
        );
        assert!(engine.account(1).is_none());
        assert!(engine.account(2).is_none());
    }

    #[test]
    fn duplicate_deposit_id_is_rejected_and_changes_nothing() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, "1.0")]);
        assert_eq!(
            engine.apply(deposit(1, 1, "5.0")),
            Err(Rejection::DuplicateTransaction)
        );
        assert_balances(&engine, 1, "1.0", "0", "1.0");
    }

    #[test]
    fn deposit_that_would_overflow_is_rejected_and_changes_nothing() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, MAX)]);
        assert_eq!(
            engine.apply(deposit(1, 2, "0.0001")),
            Err(Rejection::Overflow)
        );
        assert_balances(&engine, 1, MAX, "0", MAX);
    }

    #[test]
    fn rejected_deposit_does_not_consume_its_tx_id() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, MAX)]);
        assert_eq!(
            engine.apply(deposit(1, 2, "0.0001")),
            Err(Rejection::Overflow)
        );
        apply_all(
            &mut engine,
            &[withdrawal(1, 3, "1.0"), deposit(1, 2, "0.0001")],
        );
    }

    #[test]
    fn clients_have_independent_accounts() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, "5.0"),
                deposit(2, 2, "7.0"),
                withdrawal(1, 3, "2.0"),
            ],
        );
        assert_balances(&engine, 1, "3.0", "0", "3.0");
        assert_balances(&engine, 2, "7.0", "0", "7.0");
    }

    #[test]
    fn negative_amounts_are_rejected_without_creating_an_account() {
        let mut engine = Engine::default();
        let bad_deposit = Transaction::Deposit {
            client: 1,
            tx: 1,
            amount: negative("1.0"),
        };
        let bad_withdrawal = Transaction::Withdrawal {
            client: 2,
            tx: 2,
            amount: negative("1.0"),
        };
        assert_eq!(engine.apply(bad_deposit), Err(Rejection::NonPositiveAmount));
        assert_eq!(
            engine.apply(bad_withdrawal),
            Err(Rejection::NonPositiveAmount)
        );
        assert!(engine.account(1).is_none());
        assert!(engine.account(2).is_none());
    }

    #[test]
    fn same_tx_id_used_by_different_clients_is_not_a_duplicate() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, "5.0"), deposit(2, 1, "7.0")]);
        assert_balances(&engine, 1, "5.0", "0", "5.0");
        assert_balances(&engine, 2, "7.0", "0", "7.0");
    }

    #[test]
    fn dispute_resolve_and_chargeback_are_not_supported_yet() {
        let mut engine = Engine::default();
        let unsupported = [
            Transaction::Dispute { client: 1, tx: 1 },
            Transaction::Resolve { client: 1, tx: 1 },
            Transaction::Chargeback { client: 1, tx: 1 },
        ];
        for transaction in unsupported {
            assert_eq!(engine.apply(transaction), Err(Rejection::Unsupported));
        }
    }

    #[test]
    fn rejections_have_readable_messages() {
        let cases = [
            (
                Rejection::NonPositiveAmount,
                "amount must be greater than zero",
            ),
            (
                Rejection::DuplicateTransaction,
                "this client already has a deposit with this id",
            ),
            (Rejection::InsufficientFunds, "insufficient available funds"),
            (Rejection::Overflow, "balance would overflow"),
            (
                Rejection::Unsupported,
                "transaction type is not supported yet",
            ),
        ];
        for (rejection, message) in cases {
            assert_eq!(rejection.to_string(), message);
        }
    }
}
