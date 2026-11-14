//! Transactions as the engine sees them, and the raw input rows they come from.

use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};

use crate::amount::Amount;

/// Identifies a client. The input format guarantees a 16-bit id.
pub type ClientId = u16;

/// Identifies a transaction. The input format guarantees a 32-bit id.
pub type TxId = u32;

/// One instruction for the engine.
///
/// Deposits and withdrawals carry an amount. Disputes, resolves and chargebacks
/// refer to an earlier deposit by `tx` and carry no amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transaction {
    Deposit {
        client: ClientId,
        tx: TxId,
        amount: Amount,
    },
    Withdrawal {
        client: ClientId,
        tx: TxId,
        amount: Amount,
    },
    Dispute {
        client: ClientId,
        tx: TxId,
    },
    Resolve {
        client: ClientId,
        tx: TxId,
    },
    Chargeback {
        client: ClientId,
        tx: TxId,
    },
}

impl Transaction {
    /// The client the transaction names.
    pub fn client(&self) -> ClientId {
        match *self {
            Transaction::Deposit { client, .. }
            | Transaction::Withdrawal { client, .. }
            | Transaction::Dispute { client, .. }
            | Transaction::Resolve { client, .. }
            | Transaction::Chargeback { client, .. } => client,
        }
    }

    /// The transaction id the transaction names: its own for a deposit or
    /// withdrawal, the deposit it refers to for the rest.
    pub fn tx(&self) -> TxId {
        match *self {
            Transaction::Deposit { tx, .. }
            | Transaction::Withdrawal { tx, .. }
            | Transaction::Dispute { tx, .. }
            | Transaction::Resolve { tx, .. }
            | Transaction::Chargeback { tx, .. } => tx,
        }
    }
}

/// The kind of a row, as written in the input's `type` column.
///
/// Matching is exact: the input uses lower case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RowKind {
    Deposit,
    Withdrawal,
    Dispute,
    Resolve,
    Chargeback,
}

/// One input row exactly as it was read, before it is checked for consistency.
///
/// The `amount` column is optional because disputes, resolves and chargebacks
/// leave it empty or leave it out altogether.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) struct RawRow {
    #[serde(rename = "type")]
    pub kind: RowKind,
    #[serde(deserialize_with = "digits_only")]
    pub client: ClientId,
    #[serde(deserialize_with = "digits_only")]
    pub tx: TxId,
    #[serde(default)]
    pub amount: Option<Amount>,
}

/// Reads an id written with ASCII digits only.
///
/// The csv crate's own integer parsing also accepts a leading `+` and a `0x`
/// prefix, which would give one client several spellings.
fn digits_only<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: fmt::Display,
{
    struct DigitsOnly<T>(PhantomData<T>);

    impl<T> Visitor<'_> for DigitsOnly<T>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        type Value = T;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a number written with digits only")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<T, E> {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(E::custom("not a plain number"));
            }
            value.parse().map_err(E::custom)
        }
    }

    deserializer.deserialize_str(DigitsOnly(PhantomData))
}

/// A deposit or withdrawal row that carries no amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MissingAmount;

impl fmt::Display for MissingAmount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a deposit or withdrawal has no amount")
    }
}

impl std::error::Error for MissingAmount {}

impl TryFrom<RawRow> for Transaction {
    type Error = MissingAmount;

    /// Turns a row into a transaction.
    ///
    /// A deposit or withdrawal needs an amount. A well-formed amount given on a
    /// dispute, resolve or chargeback is ignored: those refer to an earlier
    /// deposit and the amount that counts is the one recorded there. A malformed
    /// amount still makes the row invalid, like any other bad field.
    fn try_from(row: RawRow) -> Result<Self, Self::Error> {
        let RawRow {
            kind,
            client,
            tx,
            amount,
        } = row;
        match kind {
            RowKind::Deposit => Ok(Transaction::Deposit {
                client,
                tx,
                amount: amount.ok_or(MissingAmount)?,
            }),
            RowKind::Withdrawal => Ok(Transaction::Withdrawal {
                client,
                tx,
                amount: amount.ok_or(MissingAmount)?,
            }),
            RowKind::Dispute => Ok(Transaction::Dispute { client, tx }),
            RowKind::Resolve => Ok(Transaction::Resolve { client, tx }),
            RowKind::Chargeback => Ok(Transaction::Chargeback { client, tx }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_transaction_names_its_client_and_tx() {
        let amount = Amount::from_units(10_000);
        let transactions = [
            Transaction::Deposit {
                client: 1,
                tx: 10,
                amount,
            },
            Transaction::Withdrawal {
                client: 2,
                tx: 20,
                amount,
            },
            Transaction::Dispute { client: 3, tx: 30 },
            Transaction::Resolve { client: 4, tx: 40 },
            Transaction::Chargeback { client: 5, tx: 50 },
        ];
        let named: Vec<(ClientId, TxId)> = transactions
            .iter()
            .map(|transaction| (transaction.client(), transaction.tx()))
            .collect();
        assert_eq!(named, [(1, 10), (2, 20), (3, 30), (4, 40), (5, 50)]);
    }
}
