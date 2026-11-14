//! Transactions as the engine sees them.

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
