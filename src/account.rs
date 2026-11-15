//! Client accounts and the deposit records needed to dispute them.

use crate::amount::Amount;

/// Why a balance change was refused. The account is left unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BalanceError {
    /// The amount to add or remove was zero or negative.
    NonPositiveAmount,
    /// The account is locked, so funds can no longer move in or out.
    AccountLocked,
    /// The available balance is smaller than the amount to remove.
    InsufficientFunds,
    /// The held balance is smaller than the amount to release or charge back.
    InsufficientHeld,
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

    /// Builds an account with exactly these balances, skipping every check the
    /// methods above make.
    ///
    /// Only available to tests, which sometimes need an account that real
    /// transactions could never produce, such as one whose total overflows.
    #[cfg(test)]
    pub(crate) fn with_balances(available: Amount, held: Amount, locked: bool) -> Account {
        Account {
            available,
            held,
            locked,
        }
    }

    /// A locked account takes no more deposits and allows no more withdrawals.
    fn ensure_unlocked(&self) -> Result<(), BalanceError> {
        if self.locked {
            Err(BalanceError::AccountLocked)
        } else {
            Ok(())
        }
    }

    /// Adds `amount` to the available balance.
    ///
    /// Fails with [`BalanceError::NonPositiveAmount`] if `amount` is zero or
    /// negative, [`BalanceError::AccountLocked`] if the account is locked, or
    /// [`BalanceError::Overflow`] if the new available balance or the new total
    /// (available plus held) does not fit.
    pub(crate) fn credit(&mut self, amount: Amount) -> Result<(), BalanceError> {
        ensure_positive(amount)?;
        self.ensure_unlocked()?;
        let available = self
            .available
            .checked_add(amount)
            .ok_or(BalanceError::Overflow)?;
        // The total has to stay representable as well, so that `total()` never
        // fails for an account built through these methods.
        available
            .checked_add(self.held)
            .ok_or(BalanceError::Overflow)?;
        self.available = available;
        Ok(())
    }

    /// Removes `amount` from the available balance.
    ///
    /// Fails with [`BalanceError::NonPositiveAmount`] if `amount` is zero or
    /// negative, [`BalanceError::AccountLocked`] if the account is locked, or
    /// [`BalanceError::InsufficientFunds`] if less than `amount` is available.
    pub(crate) fn debit(&mut self, amount: Amount) -> Result<(), BalanceError> {
        ensure_positive(amount)?;
        self.ensure_unlocked()?;
        // For a positive amount, ending below zero and underflowing the
        // representation both mean the same thing: not enough funds.
        self.available = self
            .available
            .checked_sub(amount)
            .filter(|remaining| *remaining >= Amount::ZERO)
            .ok_or(BalanceError::InsufficientFunds)?;
        Ok(())
    }

    /// Moves `amount` from available to held, because of a dispute.
    ///
    /// Available may become negative; the total does not change. Allowed on a
    /// locked account: the lock only stops deposits and withdrawals.
    ///
    /// Fails with [`BalanceError::NonPositiveAmount`] if `amount` is not
    /// positive, and with [`BalanceError::Overflow`] if either balance would not
    /// fit (available below the smallest amount, or held above the largest).
    pub(crate) fn hold(&mut self, amount: Amount) -> Result<(), BalanceError> {
        ensure_positive(amount)?;
        // Work out both new balances before storing either, so a failure on the
        // second cannot leave the first already changed.
        let available = self
            .available
            .checked_sub(amount)
            .ok_or(BalanceError::Overflow)?;
        let held = self
            .held
            .checked_add(amount)
            .ok_or(BalanceError::Overflow)?;
        self.available = available;
        self.held = held;
        Ok(())
    }

    /// Moves `amount` from held back to available, when a dispute is resolved.
    ///
    /// Fails with [`BalanceError::InsufficientHeld`] if less than `amount` is
    /// held, so the held balance can never go negative. Also fails with
    /// [`BalanceError::NonPositiveAmount`] if `amount` is not positive, and with
    /// [`BalanceError::Overflow`] if available would not fit the result. Allowed
    /// on a locked account.
    pub(crate) fn release(&mut self, amount: Amount) -> Result<(), BalanceError> {
        ensure_positive(amount)?;
        let held = self
            .held
            .checked_sub(amount)
            .filter(|remaining| *remaining >= Amount::ZERO)
            .ok_or(BalanceError::InsufficientHeld)?;
        let available = self
            .available
            .checked_add(amount)
            .ok_or(BalanceError::Overflow)?;
        self.held = held;
        self.available = available;
        Ok(())
    }

    /// Removes `amount` from the held balance for good, because of a
    /// chargeback, and locks the account. Allowed on an account that is
    /// already locked.
    ///
    /// Fails with [`BalanceError::NonPositiveAmount`] or
    /// [`BalanceError::InsufficientHeld`]. On failure nothing changes, and in
    /// particular the account is not locked.
    pub(crate) fn charge_back(&mut self, amount: Amount) -> Result<(), BalanceError> {
        ensure_positive(amount)?;
        // `total()` stays representable because of two things that must keep
        // holding: (a) the new held balance is at least zero (the filter below),
        // and (b) the old total fitted, which `credit`, `hold` and `release`
        // guarantee. The new total then lies between `available` and the old one.
        self.held = self
            .held
            .checked_sub(amount)
            .filter(|remaining| *remaining >= Amount::ZERO)
            .ok_or(BalanceError::InsufficientHeld)?;
        self.locked = true;
        Ok(())
    }
}

/// Balance changes only ever move a strictly positive amount.
fn ensure_positive(amount: Amount) -> Result<(), BalanceError> {
    if amount.is_positive() {
        Ok(())
    } else {
        Err(BalanceError::NonPositiveAmount)
    }
}

/// Where a deposit stands in the dispute process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisputeState {
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
pub(crate) struct DepositRecord {
    pub(crate) amount: Amount,
    pub(crate) state: DisputeState,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Amounts are written in ten-thousandths: 10_000 is 1.0, 15_000 is 1.5.

    #[test]
    fn new_account_is_empty_and_unlocked() {
        let account = Account::default();
        assert_eq!(account.available(), Amount::ZERO);
        assert_eq!(account.held(), Amount::ZERO);
        assert_eq!(account.total(), Some(Amount::ZERO));
        assert!(!account.is_locked());
    }

    #[test]
    fn credit_increases_available_and_total() {
        let mut account = Account::default();
        account.credit(Amount::from_units(15_000)).unwrap();
        account.credit(Amount::from_units(22_500)).unwrap();
        assert_eq!(account.available(), Amount::from_units(37_500));
        assert_eq!(account.total(), Some(Amount::from_units(37_500)));
    }

    #[test]
    fn credit_that_would_overflow_is_rejected_and_changes_nothing() {
        let mut account = Account::default();
        account.credit(Amount::from_units(i64::MAX)).unwrap();
        assert_eq!(
            account.credit(Amount::from_units(1)),
            Err(BalanceError::Overflow)
        );
        assert_eq!(account.available(), Amount::from_units(i64::MAX));
    }

    #[test]
    fn debit_decreases_available_and_total() {
        let mut account = Account::default();
        account.credit(Amount::from_units(30_000)).unwrap();
        account.debit(Amount::from_units(15_000)).unwrap();
        assert_eq!(account.available(), Amount::from_units(15_000));
        assert_eq!(account.total(), Some(Amount::from_units(15_000)));
    }

    #[test]
    fn debit_of_the_exact_balance_leaves_zero() {
        let mut account = Account::default();
        account.credit(Amount::from_units(20_000)).unwrap();
        account.debit(Amount::from_units(20_000)).unwrap();
        assert_eq!(account.available(), Amount::ZERO);
    }

    #[test]
    fn debit_of_more_than_available_is_rejected_and_changes_nothing() {
        let mut account = Account::default();
        account.credit(Amount::from_units(20_000)).unwrap();
        assert_eq!(
            account.debit(Amount::from_units(20_001)),
            Err(BalanceError::InsufficientFunds)
        );
        assert_eq!(account.available(), Amount::from_units(20_000));
    }

    #[test]
    fn credit_rejects_zero_and_negative_amounts_and_changes_nothing() {
        let mut account = Account::default();
        account.credit(Amount::from_units(50_000)).unwrap();
        for bad in [0, -30_000] {
            assert_eq!(
                account.credit(Amount::from_units(bad)),
                Err(BalanceError::NonPositiveAmount)
            );
        }
        assert_eq!(account.available(), Amount::from_units(50_000));
    }

    #[test]
    fn debit_rejects_zero_and_negative_amounts_and_changes_nothing() {
        let mut account = Account::default();
        account.credit(Amount::from_units(50_000)).unwrap();
        for bad in [0, -30_000] {
            assert_eq!(
                account.debit(Amount::from_units(bad)),
                Err(BalanceError::NonPositiveAmount)
            );
        }
        assert_eq!(account.available(), Amount::from_units(50_000));
    }

    #[test]
    fn credit_that_would_overflow_the_total_is_rejected_and_changes_nothing() {
        let mut account = Account::default();
        account.credit(Amount::from_units(i64::MAX)).unwrap();
        account.hold(Amount::from_units(i64::MAX)).unwrap();
        // Available (0) plus 1 fits, but available plus held plus 1 does not.
        assert_eq!(
            account.credit(Amount::from_units(1)),
            Err(BalanceError::Overflow)
        );
        assert_eq!(account.available(), Amount::ZERO);
        assert_eq!(account.held(), Amount::from_units(i64::MAX));
    }

    #[test]
    fn hold_moves_funds_from_available_to_held() {
        let mut account = Account::default();
        account.credit(Amount::from_units(50_000)).unwrap();
        account.hold(Amount::from_units(20_000)).unwrap();
        assert_eq!(account.available(), Amount::from_units(30_000));
        assert_eq!(account.held(), Amount::from_units(20_000));
        assert_eq!(account.total(), Some(Amount::from_units(50_000)));
    }

    #[test]
    fn hold_can_drive_available_negative_but_keeps_the_total() {
        let mut account = Account::default();
        account.credit(Amount::from_units(10_000)).unwrap();
        account.hold(Amount::from_units(15_000)).unwrap();
        assert_eq!(account.available(), Amount::from_units(-5_000));
        assert_eq!(account.held(), Amount::from_units(15_000));
        assert_eq!(account.total(), Some(Amount::from_units(10_000)));
    }

    #[test]
    fn hold_that_would_overflow_held_is_rejected_and_changes_nothing() {
        let mut account = Account::default();
        account.credit(Amount::from_units(i64::MAX)).unwrap();
        account.hold(Amount::from_units(i64::MAX)).unwrap();
        // Available (0) can go to -1, but held (MAX) cannot grow: neither may change.
        assert_eq!(
            account.hold(Amount::from_units(1)),
            Err(BalanceError::Overflow)
        );
        assert_eq!(account.available(), Amount::ZERO);
        assert_eq!(account.held(), Amount::from_units(i64::MAX));
    }

    #[test]
    fn release_moves_funds_from_held_back_to_available() {
        let mut account = Account::default();
        account.credit(Amount::from_units(50_000)).unwrap();
        account.hold(Amount::from_units(20_000)).unwrap();
        account.release(Amount::from_units(20_000)).unwrap();
        assert_eq!(account.available(), Amount::from_units(50_000));
        assert_eq!(account.held(), Amount::ZERO);
    }

    #[test]
    fn release_of_more_than_held_is_rejected_and_changes_nothing() {
        let mut account = Account::default();
        account.credit(Amount::from_units(50_000)).unwrap();
        account.hold(Amount::from_units(20_000)).unwrap();
        assert_eq!(
            account.release(Amount::from_units(20_001)),
            Err(BalanceError::InsufficientHeld)
        );
        assert_eq!(account.available(), Amount::from_units(30_000));
        assert_eq!(account.held(), Amount::from_units(20_000));
    }

    #[test]
    fn hold_and_release_reject_zero_and_negative_amounts() {
        let mut account = Account::default();
        account.credit(Amount::from_units(50_000)).unwrap();
        account.hold(Amount::from_units(20_000)).unwrap();
        for bad in [0, -1] {
            assert_eq!(
                account.hold(Amount::from_units(bad)),
                Err(BalanceError::NonPositiveAmount)
            );
            assert_eq!(
                account.release(Amount::from_units(bad)),
                Err(BalanceError::NonPositiveAmount)
            );
        }
        assert_eq!(account.available(), Amount::from_units(30_000));
        assert_eq!(account.held(), Amount::from_units(20_000));
    }

    #[test]
    fn credit_while_funds_are_held_only_changes_available() {
        let mut account = Account::default();
        account.credit(Amount::from_units(50_000)).unwrap();
        account.hold(Amount::from_units(20_000)).unwrap();
        account.credit(Amount::from_units(10_000)).unwrap();
        assert_eq!(account.available(), Amount::from_units(40_000));
        assert_eq!(account.held(), Amount::from_units(20_000));
        assert_eq!(account.total(), Some(Amount::from_units(60_000)));
    }

    #[test]
    fn charge_back_removes_held_funds_and_locks_the_account() {
        let mut account = Account::default();
        account.credit(Amount::from_units(50_000)).unwrap();
        account.hold(Amount::from_units(20_000)).unwrap();
        account.charge_back(Amount::from_units(20_000)).unwrap();
        assert_eq!(account.available(), Amount::from_units(30_000));
        assert_eq!(account.held(), Amount::ZERO);
        assert_eq!(account.total(), Some(Amount::from_units(30_000)));
        assert!(account.is_locked());
    }

    #[test]
    fn charge_back_of_more_than_held_is_rejected_and_does_not_lock() {
        let mut account = Account::default();
        account.credit(Amount::from_units(50_000)).unwrap();
        account.hold(Amount::from_units(20_000)).unwrap();
        assert_eq!(
            account.charge_back(Amount::from_units(20_001)),
            Err(BalanceError::InsufficientHeld)
        );
        assert_eq!(account.available(), Amount::from_units(30_000));
        assert_eq!(account.held(), Amount::from_units(20_000));
        assert!(!account.is_locked());
    }

    #[test]
    fn charge_back_rejects_zero_and_negative_amounts_and_does_not_lock() {
        let mut account = Account::default();
        account.credit(Amount::from_units(50_000)).unwrap();
        account.hold(Amount::from_units(20_000)).unwrap();
        for bad in [0, -1] {
            assert_eq!(
                account.charge_back(Amount::from_units(bad)),
                Err(BalanceError::NonPositiveAmount)
            );
        }
        assert_eq!(account.held(), Amount::from_units(20_000));
        assert!(!account.is_locked());
    }

    #[test]
    fn locked_account_refuses_credit_and_debit() {
        let mut account = Account::default();
        account.credit(Amount::from_units(50_000)).unwrap();
        account.hold(Amount::from_units(20_000)).unwrap();
        account.charge_back(Amount::from_units(20_000)).unwrap();
        assert_eq!(
            account.credit(Amount::from_units(10_000)),
            Err(BalanceError::AccountLocked)
        );
        assert_eq!(
            account.debit(Amount::from_units(10_000)),
            Err(BalanceError::AccountLocked)
        );
        assert_eq!(account.available(), Amount::from_units(30_000));
    }

    #[test]
    fn locked_account_still_allows_hold_and_release() {
        let mut account = Account::default();
        account.credit(Amount::from_units(50_000)).unwrap();
        account.hold(Amount::from_units(20_000)).unwrap();
        account.hold(Amount::from_units(10_000)).unwrap();
        account.charge_back(Amount::from_units(20_000)).unwrap();
        assert!(account.is_locked());
        account.release(Amount::from_units(10_000)).unwrap();
        account.hold(Amount::from_units(5_000)).unwrap();
        assert_eq!(account.available(), Amount::from_units(25_000));
        assert_eq!(account.held(), Amount::from_units(5_000));
    }
}
