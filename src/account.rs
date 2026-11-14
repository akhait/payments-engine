//! Client accounts and the deposit records needed to dispute them.

use crate::amount::Amount;

/// Why a balance change was refused. The account is left unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalanceError {
    /// The amount to add or remove was zero or negative.
    NonPositiveAmount,
    /// The available balance is smaller than the amount to remove.
    InsufficientFunds,
    /// The result does not fit in an [`Amount`].
    Overflow,
}

/// A client's balances.
///
/// The fields are private so the only way to change a balance is through
/// methods that check for overflow. The total is never stored: it is always
/// `available + held`, so it cannot drift out of sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Account {
    available: Amount,
    held: Amount,
    locked: bool,
}

impl Account {
    /// Funds the client can use right now.
    pub fn available(&self) -> Amount {
        self.available
    }

    /// Funds set aside because of a dispute.
    pub fn held(&self) -> Amount {
        self.held
    }

    /// Whether the account is frozen.
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Available plus held, or `None` if that sum overflows.
    pub fn total(&self) -> Option<Amount> {
        self.available.checked_add(self.held)
    }

    /// Adds `amount` to the available balance.
    ///
    /// Fails with [`BalanceError::NonPositiveAmount`] if `amount` is zero or
    /// negative, or [`BalanceError::Overflow`] if the new available balance
    /// does not fit. (The check that available plus held also fits arrives
    /// together with disputes, the first way to make `held` non-zero.)
    pub(crate) fn credit(&mut self, amount: Amount) -> Result<(), BalanceError> {
        if !amount.is_positive() {
            return Err(BalanceError::NonPositiveAmount);
        }
        self.available = self
            .available
            .checked_add(amount)
            .ok_or(BalanceError::Overflow)?;
        Ok(())
    }

    /// Removes `amount` from the available balance.
    ///
    /// Fails with [`BalanceError::NonPositiveAmount`] if `amount` is zero or
    /// negative, or [`BalanceError::InsufficientFunds`] if less than `amount`
    /// is available.
    pub(crate) fn debit(&mut self, amount: Amount) -> Result<(), BalanceError> {
        if !amount.is_positive() {
            return Err(BalanceError::NonPositiveAmount);
        }
        // For a positive amount, ending below zero and underflowing the
        // representation both mean the same thing: not enough funds.
        self.available = self
            .available
            .checked_sub(amount)
            .filter(|remaining| *remaining >= Amount::ZERO)
            .ok_or(BalanceError::InsufficientFunds)?;
        Ok(())
    }
}

/// Where a deposit stands in the dispute process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisputeState {
    /// Not disputed, or a dispute that was resolved.
    Normal,
    /// The funds are currently held.
    Disputed,
    /// The dispute ended in a chargeback. This is final.
    ChargedBack,
}

/// What the engine remembers about a deposit so it can be disputed later.
///
/// The engine stores it under the deposit's `(client, tx)` pair, so the client
/// is part of the key and not repeated here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepositRecord {
    pub amount: Amount,
    pub state: DisputeState,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: &str = "922337203685477.5807";

    fn amt(s: &str) -> Amount {
        s.parse().expect("valid amount in test")
    }

    fn negative(s: &str) -> Amount {
        Amount::ZERO
            .checked_sub(amt(s))
            .expect("a small amount cannot underflow")
    }

    #[test]
    fn new_account_is_empty_and_unlocked() {
        let account = Account::default();
        assert_eq!(account.available(), amt("0"));
        assert_eq!(account.held(), amt("0"));
        assert_eq!(account.total(), Some(amt("0")));
        assert!(!account.is_locked());
    }

    #[test]
    fn credit_increases_available_and_total() {
        let mut account = Account::default();
        account.credit(amt("1.5")).unwrap();
        account.credit(amt("2.25")).unwrap();
        assert_eq!(account.available(), amt("3.75"));
        assert_eq!(account.total(), Some(amt("3.75")));
    }

    #[test]
    fn credit_that_would_overflow_is_rejected_and_changes_nothing() {
        let mut account = Account::default();
        account.credit(amt(MAX)).unwrap();
        assert_eq!(account.credit(amt("0.0001")), Err(BalanceError::Overflow));
        assert_eq!(account.available(), amt(MAX));
    }

    #[test]
    fn debit_decreases_available_and_total() {
        let mut account = Account::default();
        account.credit(amt("3.0")).unwrap();
        account.debit(amt("1.5")).unwrap();
        assert_eq!(account.available(), amt("1.5"));
        assert_eq!(account.total(), Some(amt("1.5")));
    }

    #[test]
    fn debit_of_the_exact_balance_leaves_zero() {
        let mut account = Account::default();
        account.credit(amt("2.0")).unwrap();
        account.debit(amt("2.0")).unwrap();
        assert_eq!(account.available(), amt("0"));
    }

    #[test]
    fn debit_of_more_than_available_is_rejected_and_changes_nothing() {
        let mut account = Account::default();
        account.credit(amt("2.0")).unwrap();
        assert_eq!(
            account.debit(amt("2.0001")),
            Err(BalanceError::InsufficientFunds)
        );
        assert_eq!(account.available(), amt("2.0"));
    }

    #[test]
    fn credit_rejects_zero_and_negative_amounts_and_changes_nothing() {
        let mut account = Account::default();
        account.credit(amt("5.0")).unwrap();
        for bad in [Amount::ZERO, negative("3.0")] {
            assert_eq!(account.credit(bad), Err(BalanceError::NonPositiveAmount));
        }
        assert_eq!(account.available(), amt("5.0"));
    }

    #[test]
    fn debit_rejects_zero_and_negative_amounts_and_changes_nothing() {
        let mut account = Account::default();
        account.credit(amt("5.0")).unwrap();
        for bad in [Amount::ZERO, negative("3.0")] {
            assert_eq!(account.debit(bad), Err(BalanceError::NonPositiveAmount));
        }
        assert_eq!(account.available(), amt("5.0"));
    }
}
