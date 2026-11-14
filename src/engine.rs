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

    // Amounts are written in ten-thousandths: 10_000 is 1.0, 15_000 is 1.5.

    fn deposit(client: ClientId, tx: TxId, units: i64) -> Transaction {
        Transaction::Deposit {
            client,
            tx,
            amount: Amount::from_units(units),
        }
    }

    fn withdrawal(client: ClientId, tx: TxId, units: i64) -> Transaction {
        Transaction::Withdrawal {
            client,
            tx,
            amount: Amount::from_units(units),
        }
    }

    fn apply_all(engine: &mut Engine, transactions: &[Transaction]) {
        for transaction in transactions {
            engine
                .apply(*transaction)
                .unwrap_or_else(|why| panic!("{transaction:?} was rejected: {why}"));
        }
    }

    /// Checks available and held, and that total is exactly their sum.
    fn assert_balances(engine: &Engine, client: ClientId, available: i64, held: i64) {
        let account = engine
            .account(client)
            .unwrap_or_else(|| panic!("client {client} has no account"));
        assert_eq!(
            account.available(),
            Amount::from_units(available),
            "available"
        );
        assert_eq!(account.held(), Amount::from_units(held), "held");
        assert_eq!(
            account.total(),
            Some(Amount::from_units(available + held)),
            "total"
        );
    }

    #[test]
    fn deposits_increase_available_and_total() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, 10_000), deposit(1, 2, 20_000)]);
        assert_balances(&engine, 1, 30_000, 0);
    }

    #[test]
    fn withdrawal_decreases_available_and_total() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[deposit(1, 1, 30_000), withdrawal(1, 2, 15_000)],
        );
        assert_balances(&engine, 1, 15_000, 0);
    }

    #[test]
    fn withdrawing_the_exact_balance_is_allowed() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[deposit(1, 1, 20_000), withdrawal(1, 2, 20_000)],
        );
        assert_balances(&engine, 1, 0, 0);
    }

    #[test]
    fn withdrawal_exceeding_available_is_rejected_and_changes_nothing() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(2, 1, 20_000)]);
        assert_eq!(
            engine.apply(withdrawal(2, 2, 30_000)),
            Err(Rejection::InsufficientFunds)
        );
        assert_balances(&engine, 2, 20_000, 0);
    }

    #[test]
    fn first_withdrawal_it_cannot_cover_still_creates_the_account() {
        let mut engine = Engine::default();
        assert_eq!(
            engine.apply(withdrawal(7, 1, 10_000)),
            Err(Rejection::InsufficientFunds)
        );
        assert_balances(&engine, 7, 0, 0);
    }

    #[test]
    fn zero_amounts_are_rejected_without_creating_an_account() {
        let mut engine = Engine::default();
        assert_eq!(
            engine.apply(deposit(1, 1, 0)),
            Err(Rejection::NonPositiveAmount)
        );
        assert_eq!(
            engine.apply(withdrawal(2, 2, 0)),
            Err(Rejection::NonPositiveAmount)
        );
        assert!(engine.account(1).is_none());
        assert!(engine.account(2).is_none());
    }

    #[test]
    fn negative_amounts_are_rejected_without_creating_an_account() {
        let mut engine = Engine::default();
        assert_eq!(
            engine.apply(deposit(1, 1, -10_000)),
            Err(Rejection::NonPositiveAmount)
        );
        assert_eq!(
            engine.apply(withdrawal(2, 2, -10_000)),
            Err(Rejection::NonPositiveAmount)
        );
        assert!(engine.account(1).is_none());
        assert!(engine.account(2).is_none());
    }

    #[test]
    fn duplicate_deposit_id_is_rejected_and_changes_nothing() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, 10_000)]);
        assert_eq!(
            engine.apply(deposit(1, 1, 50_000)),
            Err(Rejection::DuplicateTransaction)
        );
        assert_balances(&engine, 1, 10_000, 0);
    }

    #[test]
    fn deposit_that_would_overflow_is_rejected_and_changes_nothing() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, i64::MAX)]);
        assert_eq!(engine.apply(deposit(1, 2, 1)), Err(Rejection::Overflow));
        assert_balances(&engine, 1, i64::MAX, 0);
    }

    #[test]
    fn rejected_deposit_does_not_consume_its_tx_id() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, i64::MAX)]);
        assert_eq!(engine.apply(deposit(1, 2, 1)), Err(Rejection::Overflow));
        apply_all(&mut engine, &[withdrawal(1, 3, 10_000), deposit(1, 2, 1)]);
    }

    #[test]
    fn clients_have_independent_accounts() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, 50_000),
                deposit(2, 2, 70_000),
                withdrawal(1, 3, 20_000),
            ],
        );
        assert_balances(&engine, 1, 30_000, 0);
        assert_balances(&engine, 2, 70_000, 0);
    }

    #[test]
    fn same_tx_id_used_by_different_clients_is_not_a_duplicate() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, 50_000), deposit(2, 1, 70_000)]);
        assert_balances(&engine, 1, 50_000, 0);
        assert_balances(&engine, 2, 70_000, 0);
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
