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
    /// No deposit with this id exists for this client.
    UnknownTransaction,
    /// A dispute targeted a deposit that is already disputed or charged back.
    NotDisputable,
    /// A resolve or chargeback targeted a deposit that is not under dispute.
    NotUnderDispute,
    /// More held funds were to be released or charged back than the account
    /// holds. The engine never asks for this, so seeing it means its
    /// bookkeeping is broken.
    InsufficientHeld,
    /// A deposit or withdrawal was attempted on a locked account.
    AccountLocked,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NonPositiveAmount => "amount must be greater than zero",
            Self::DuplicateTransaction => "this client already has a deposit with this id",
            Self::InsufficientFunds => "insufficient available funds",
            Self::Overflow => "balance would overflow",
            Self::UnknownTransaction => "this client has no deposit with this id",
            Self::NotDisputable => "deposit is already disputed or charged back",
            Self::NotUnderDispute => "deposit is not under dispute",
            Self::InsufficientHeld => "not enough held funds",
            Self::AccountLocked => "account is locked",
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
            BalanceError::AccountLocked => Rejection::AccountLocked,
            BalanceError::InsufficientHeld => Rejection::InsufficientHeld,
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
    #[cfg(test)]
    pub(crate) fn account(&self, client: ClientId) -> Option<&Account> {
        self.accounts.get(&client)
    }

    /// Hands over every account, in no particular order.
    pub fn into_accounts(self) -> impl Iterator<Item = (ClientId, Account)> {
        self.accounts.into_iter()
    }

    /// Applies one transaction, or explains why it was refused.
    pub fn apply(&mut self, transaction: Transaction) -> Result<(), Rejection> {
        match transaction {
            Transaction::Deposit { client, tx, amount } => self.deposit(client, tx, amount),
            Transaction::Withdrawal { client, amount, .. } => self.withdraw(client, amount),
            Transaction::Dispute { client, tx } => self.dispute(client, tx),
            Transaction::Resolve { client, tx } => self.resolve(client, tx),
            Transaction::Chargeback { client, tx } => self.chargeback(client, tx),
        }
    }

    fn deposit(&mut self, client: ClientId, tx: TxId, amount: Amount) -> Result<(), Rejection> {
        if !amount.is_positive() {
            return Err(Rejection::NonPositiveAmount);
        }
        let key = (client, tx);
        if self.deposits.contains_key(&key) {
            return Err(Rejection::DuplicateTransaction);
        }
        // Credit an existing account in place. A new account is stored only
        // after its first credit succeeded, so a refused deposit leaves no
        // trace: no new account and no record.
        match self.accounts.get_mut(&client) {
            Some(account) => account.credit(amount)?,
            None => {
                let mut account = Account::default();
                account.credit(amount)?;
                self.accounts.insert(client, account);
            }
        }
        self.deposits.insert(
            key,
            DepositRecord {
                amount,
                state: DisputeState::Normal,
            },
        );
        Ok(())
    }

    fn dispute(&mut self, client: ClientId, tx: TxId) -> Result<(), Rejection> {
        let (account, record) = self.account_and_deposit(client, tx)?;
        if record.state != DisputeState::Normal {
            return Err(Rejection::NotDisputable);
        }
        // Mark the deposit only after the funds were actually held.
        account.hold(record.amount)?;
        record.state = DisputeState::Disputed;
        Ok(())
    }

    fn resolve(&mut self, client: ClientId, tx: TxId) -> Result<(), Rejection> {
        let (account, record) = self.account_and_deposit(client, tx)?;
        if record.state != DisputeState::Disputed {
            return Err(Rejection::NotUnderDispute);
        }
        account.release(record.amount)?;
        record.state = DisputeState::Normal;
        Ok(())
    }

    fn chargeback(&mut self, client: ClientId, tx: TxId) -> Result<(), Rejection> {
        let (account, record) = self.account_and_deposit(client, tx)?;
        if record.state != DisputeState::Disputed {
            return Err(Rejection::NotUnderDispute);
        }
        // As with a dispute, mark the deposit only after the account changed.
        account.charge_back(record.amount)?;
        record.state = DisputeState::ChargedBack;
        Ok(())
    }

    /// Finds a client's account together with one of their deposits.
    ///
    /// Deposits are keyed by `(client, tx)`, so a deposit that belongs to
    /// another client is simply not found here.
    fn account_and_deposit(
        &mut self,
        client: ClientId,
        tx: TxId,
    ) -> Result<(&mut Account, &mut DepositRecord), Rejection> {
        let record = self
            .deposits
            .get_mut(&(client, tx))
            .ok_or(Rejection::UnknownTransaction)?;
        // A recorded deposit always has an account; report a missing one as an
        // unknown deposit instead of panicking.
        let account = self
            .accounts
            .get_mut(&client)
            .ok_or(Rejection::UnknownTransaction)?;
        Ok((account, record))
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

    fn dispute(client: ClientId, tx: TxId) -> Transaction {
        Transaction::Dispute { client, tx }
    }

    fn resolve(client: ClientId, tx: TxId) -> Transaction {
        Transaction::Resolve { client, tx }
    }

    fn chargeback(client: ClientId, tx: TxId) -> Transaction {
        Transaction::Chargeback { client, tx }
    }

    fn is_locked(engine: &Engine, client: ClientId) -> bool {
        engine
            .account(client)
            .unwrap_or_else(|| panic!("client {client} has no account"))
            .is_locked()
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
    fn into_accounts_hands_over_every_client_even_one_whose_first_withdrawal_was_refused() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(2, 1, 20_000), deposit(1, 2, 10_000)]);
        assert_eq!(
            engine.apply(withdrawal(7, 3, 5_000)),
            Err(Rejection::InsufficientFunds)
        );
        let mut accounts: Vec<(ClientId, Account)> = engine.into_accounts().collect();
        accounts.sort_by_key(|(client, _)| *client);
        let clients: Vec<ClientId> = accounts.iter().map(|(client, _)| *client).collect();
        assert_eq!(clients, [1, 2, 7]);
        assert_eq!(accounts[0].1.available(), Amount::from_units(10_000));
        assert_eq!(accounts[1].1.available(), Amount::from_units(20_000));
        assert_eq!(accounts[2].1.available(), Amount::ZERO);
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
    fn dispute_holds_the_deposited_amount() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, 50_000), dispute(1, 1)]);
        assert_balances(&engine, 1, 0, 50_000);
    }

    #[test]
    fn dispute_holds_only_the_disputed_deposit() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[deposit(1, 1, 50_000), deposit(1, 2, 20_000), dispute(1, 2)],
        );
        assert_balances(&engine, 1, 50_000, 20_000);
    }

    #[test]
    fn dispute_after_a_withdrawal_can_leave_available_negative() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, 50_000),
                withdrawal(1, 2, 50_000),
                dispute(1, 1),
            ],
        );
        assert_balances(&engine, 1, -50_000, 50_000);
    }

    #[test]
    fn dispute_of_an_unknown_tx_is_rejected_and_creates_no_account() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, 50_000)]);
        assert_eq!(
            engine.apply(dispute(1, 2)),
            Err(Rejection::UnknownTransaction)
        );
        assert_eq!(
            engine.apply(dispute(3, 1)),
            Err(Rejection::UnknownTransaction)
        );
        assert_balances(&engine, 1, 50_000, 0);
        assert!(engine.account(3).is_none());
    }

    #[test]
    fn dispute_naming_another_client_is_rejected_and_changes_nothing() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, 50_000), deposit(2, 2, 70_000)]);
        assert_eq!(
            engine.apply(dispute(2, 1)),
            Err(Rejection::UnknownTransaction)
        );
        assert_balances(&engine, 1, 50_000, 0);
        assert_balances(&engine, 2, 70_000, 0);
    }

    #[test]
    fn disputing_twice_is_rejected_and_holds_the_funds_once() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, 50_000), dispute(1, 1)]);
        assert_eq!(engine.apply(dispute(1, 1)), Err(Rejection::NotDisputable));
        assert_balances(&engine, 1, 0, 50_000);
    }

    #[test]
    fn a_withdrawal_id_cannot_be_disputed() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[deposit(1, 1, 50_000), withdrawal(1, 2, 10_000)],
        );
        assert_eq!(
            engine.apply(dispute(1, 2)),
            Err(Rejection::UnknownTransaction)
        );
        assert_balances(&engine, 1, 40_000, 0);
    }

    #[test]
    fn dispute_that_would_overflow_held_is_rejected_and_state_is_unchanged() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, i64::MAX),
                withdrawal(1, 2, i64::MAX),
                dispute(1, 1),
                deposit(1, 3, i64::MAX),
            ],
        );
        assert_balances(&engine, 1, 0, i64::MAX);
        assert_eq!(engine.apply(dispute(1, 3)), Err(Rejection::Overflow));
        assert_balances(&engine, 1, 0, i64::MAX);
        // The refused dispute must not have marked deposit 3 as disputed.
        assert_eq!(engine.apply(resolve(1, 3)), Err(Rejection::NotUnderDispute));
    }

    #[test]
    fn dispute_that_would_underflow_available_is_rejected_and_state_is_unchanged() {
        // Charging back a deposit that was already withdrawn leaves available at
        // -MAX on a locked account, and disputing another deposit of MAX would
        // push it past the smallest amount.
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, i64::MAX),
                withdrawal(1, 2, i64::MAX),
                deposit(1, 3, i64::MAX),
                withdrawal(1, 4, i64::MAX),
                dispute(1, 1),
                chargeback(1, 1),
            ],
        );
        assert_balances(&engine, 1, -i64::MAX, 0);
        assert_eq!(engine.apply(dispute(1, 3)), Err(Rejection::Overflow));
        assert_balances(&engine, 1, -i64::MAX, 0);
        // The refused dispute must not have marked deposit 3 as disputed.
        assert_eq!(engine.apply(resolve(1, 3)), Err(Rejection::NotUnderDispute));
    }

    #[test]
    fn resolve_releases_the_held_funds() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[deposit(1, 1, 50_000), dispute(1, 1), resolve(1, 1)],
        );
        assert_balances(&engine, 1, 50_000, 0);
    }

    #[test]
    fn resolved_deposit_can_be_disputed_again() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, 50_000),
                dispute(1, 1),
                resolve(1, 1),
                dispute(1, 1),
            ],
        );
        assert_balances(&engine, 1, 0, 50_000);
    }

    #[test]
    fn resolve_of_a_deposit_not_under_dispute_is_rejected() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, 50_000)]);
        assert_eq!(engine.apply(resolve(1, 1)), Err(Rejection::NotUnderDispute));
        assert_balances(&engine, 1, 50_000, 0);
    }

    #[test]
    fn resolve_of_an_unknown_tx_or_wrong_client_is_rejected_and_changes_nothing() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, 50_000), dispute(1, 1)]);
        assert_eq!(
            engine.apply(resolve(1, 9)),
            Err(Rejection::UnknownTransaction)
        );
        assert_eq!(
            engine.apply(resolve(2, 1)),
            Err(Rejection::UnknownTransaction)
        );
        assert_balances(&engine, 1, 0, 50_000);
        assert!(engine.account(2).is_none());
    }

    #[test]
    fn withdrawal_cannot_spend_held_funds() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, 50_000), dispute(1, 1)]);
        assert_eq!(
            engine.apply(withdrawal(1, 2, 10_000)),
            Err(Rejection::InsufficientFunds)
        );
        assert_balances(&engine, 1, 0, 50_000);
    }

    #[test]
    fn resolve_naming_a_client_with_its_own_held_funds_changes_nothing() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, 50_000),
                dispute(1, 1),
                deposit(2, 2, 50_000),
                dispute(2, 2),
            ],
        );
        // Client 2 holds enough to cover tx 1, but tx 1 is not client 2's deposit.
        assert_eq!(
            engine.apply(resolve(2, 1)),
            Err(Rejection::UnknownTransaction)
        );
        assert_balances(&engine, 1, 0, 50_000);
        assert_balances(&engine, 2, 0, 50_000);
    }

    #[test]
    fn duplicate_deposit_id_is_rejected_while_the_original_is_disputed() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, 50_000), dispute(1, 1)]);
        assert_eq!(
            engine.apply(deposit(1, 1, 70_000)),
            Err(Rejection::DuplicateTransaction)
        );
        assert_balances(&engine, 1, 0, 50_000);
        // The original record is intact: resolving still releases its amount.
        apply_all(&mut engine, &[resolve(1, 1)]);
        assert_balances(&engine, 1, 50_000, 0);
    }

    #[test]
    fn balance_errors_map_to_the_matching_rejections() {
        let cases = [
            (
                BalanceError::NonPositiveAmount,
                Rejection::NonPositiveAmount,
            ),
            (
                BalanceError::InsufficientFunds,
                Rejection::InsufficientFunds,
            ),
            (BalanceError::InsufficientHeld, Rejection::InsufficientHeld),
            (BalanceError::AccountLocked, Rejection::AccountLocked),
            (BalanceError::Overflow, Rejection::Overflow),
        ];
        for (error, expected) in cases {
            assert_eq!(Rejection::from(error), expected);
        }
    }

    #[test]
    fn chargeback_removes_the_held_funds_and_locks_the_account() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[deposit(1, 1, 50_000), dispute(1, 1), chargeback(1, 1)],
        );
        assert_balances(&engine, 1, 0, 0);
        assert!(is_locked(&engine, 1));
    }

    #[test]
    fn chargeback_only_removes_the_charged_back_deposit() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, 50_000),
                deposit(1, 2, 30_000),
                dispute(1, 1),
                chargeback(1, 1),
            ],
        );
        assert_balances(&engine, 1, 30_000, 0);
        assert!(is_locked(&engine, 1));
    }

    #[test]
    fn chargeback_of_an_already_withdrawn_deposit_leaves_a_negative_balance() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, 50_000),
                withdrawal(1, 2, 50_000),
                dispute(1, 1),
                chargeback(1, 1),
            ],
        );
        assert_balances(&engine, 1, -50_000, 0);
        assert!(is_locked(&engine, 1));
    }

    #[test]
    fn chargeback_of_a_deposit_not_under_dispute_is_rejected_and_does_not_lock() {
        let mut engine = Engine::default();
        apply_all(&mut engine, &[deposit(1, 1, 50_000)]);
        assert_eq!(
            engine.apply(chargeback(1, 1)),
            Err(Rejection::NotUnderDispute)
        );
        assert_balances(&engine, 1, 50_000, 0);
        assert!(!is_locked(&engine, 1));
    }

    #[test]
    fn chargeback_of_an_unknown_tx_or_wrong_client_is_rejected_and_changes_nothing() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, 50_000),
                dispute(1, 1),
                deposit(2, 2, 50_000),
                dispute(2, 2),
            ],
        );
        for transaction in [chargeback(1, 9), chargeback(2, 1), chargeback(7, 7)] {
            assert_eq!(
                engine.apply(transaction),
                Err(Rejection::UnknownTransaction)
            );
        }
        assert_balances(&engine, 1, 0, 50_000);
        assert_balances(&engine, 2, 0, 50_000);
        assert!(!is_locked(&engine, 1));
        assert!(!is_locked(&engine, 2));
        assert!(engine.account(7).is_none());
    }

    #[test]
    fn a_charged_back_deposit_is_final() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[deposit(1, 1, 50_000), dispute(1, 1), chargeback(1, 1)],
        );
        assert_eq!(engine.apply(dispute(1, 1)), Err(Rejection::NotDisputable));
        assert_eq!(engine.apply(resolve(1, 1)), Err(Rejection::NotUnderDispute));
        assert_eq!(
            engine.apply(chargeback(1, 1)),
            Err(Rejection::NotUnderDispute)
        );
        assert_balances(&engine, 1, 0, 0);
    }

    #[test]
    fn locked_account_rejects_deposits_and_withdrawals() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, 50_000),
                deposit(1, 2, 30_000),
                dispute(1, 1),
                chargeback(1, 1),
            ],
        );
        // 30_000 is available, so only the lock can be the reason for refusing.
        assert_eq!(
            engine.apply(deposit(1, 3, 10_000)),
            Err(Rejection::AccountLocked)
        );
        assert_eq!(
            engine.apply(withdrawal(1, 4, 10_000)),
            Err(Rejection::AccountLocked)
        );
        assert_balances(&engine, 1, 30_000, 0);
        // The refused deposit left no record behind.
        assert_eq!(
            engine.apply(dispute(1, 3)),
            Err(Rejection::UnknownTransaction)
        );
    }

    #[test]
    fn locked_account_still_handles_disputes_for_its_other_deposits() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, 50_000),
                deposit(1, 2, 30_000),
                dispute(1, 1),
                chargeback(1, 1),
            ],
        );
        apply_all(&mut engine, &[dispute(1, 2)]);
        assert_balances(&engine, 1, 0, 30_000);
        apply_all(&mut engine, &[resolve(1, 2)]);
        assert_balances(&engine, 1, 30_000, 0);
        apply_all(&mut engine, &[dispute(1, 2), chargeback(1, 2)]);
        assert_balances(&engine, 1, 0, 0);
        assert!(is_locked(&engine, 1));
    }

    #[test]
    fn rejected_chargeback_does_not_touch_other_held_funds_or_lock() {
        let mut engine = Engine::default();
        // Deposit 2 is disputed and holds enough to cover deposit 1, which is not.
        apply_all(
            &mut engine,
            &[deposit(1, 1, 50_000), deposit(1, 2, 50_000), dispute(1, 2)],
        );
        assert_eq!(
            engine.apply(chargeback(1, 1)),
            Err(Rejection::NotUnderDispute)
        );
        assert_balances(&engine, 1, 50_000, 50_000);
        assert!(!is_locked(&engine, 1));
    }

    #[test]
    fn rejected_chargeback_of_a_charged_back_deposit_does_not_touch_other_held_funds() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, 50_000),
                deposit(1, 2, 50_000),
                dispute(1, 1),
                chargeback(1, 1),
                dispute(1, 2),
            ],
        );
        assert_balances(&engine, 1, 0, 50_000);
        assert_eq!(
            engine.apply(chargeback(1, 1)),
            Err(Rejection::NotUnderDispute)
        );
        assert_balances(&engine, 1, 0, 50_000);
    }

    #[test]
    fn locked_account_refuses_deposits_whatever_its_balances() {
        let mut engine = Engine::default();
        // Client 1 ends locked with negative available funds.
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, 50_000),
                withdrawal(1, 2, 50_000),
                dispute(1, 1),
                chargeback(1, 1),
            ],
        );
        // Client 2 ends locked with exactly zero available funds.
        apply_all(
            &mut engine,
            &[deposit(2, 1, 50_000), dispute(2, 1), chargeback(2, 1)],
        );
        // Client 3 ends locked while another deposit is still held.
        apply_all(
            &mut engine,
            &[
                deposit(3, 1, 50_000),
                deposit(3, 2, 30_000),
                dispute(3, 1),
                dispute(3, 2),
                chargeback(3, 1),
            ],
        );
        for client in [1, 2, 3] {
            assert!(is_locked(&engine, client));
            assert_eq!(
                engine.apply(deposit(client, 9, 10_000)),
                Err(Rejection::AccountLocked)
            );
        }
        assert_balances(&engine, 1, -50_000, 0);
        assert_balances(&engine, 2, 0, 0);
        assert_balances(&engine, 3, 0, 30_000);
    }

    #[test]
    fn locking_one_client_does_not_affect_another() {
        let mut engine = Engine::default();
        apply_all(
            &mut engine,
            &[
                deposit(1, 1, 50_000),
                dispute(1, 1),
                chargeback(1, 1),
                deposit(2, 2, 70_000),
                withdrawal(2, 3, 20_000),
            ],
        );
        assert_balances(&engine, 2, 50_000, 0);
        assert!(!is_locked(&engine, 2));
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
                Rejection::UnknownTransaction,
                "this client has no deposit with this id",
            ),
            (
                Rejection::NotDisputable,
                "deposit is already disputed or charged back",
            ),
            (Rejection::NotUnderDispute, "deposit is not under dispute"),
            (Rejection::InsufficientHeld, "not enough held funds"),
            (Rejection::AccountLocked, "account is locked"),
        ];
        for (rejection, message) in cases {
            assert_eq!(rejection.to_string(), message);
        }
    }
}
