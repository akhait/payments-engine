//! Writing the final state of the accounts as CSV.

use std::fmt;
use std::io::{self, Write};

use crate::account::Account;
use crate::amount::Amount;
use crate::transaction::ClientId;

/// The columns of the report, in order.
const COLUMNS: [&str; 5] = ["client", "available", "held", "total", "locked"];

/// The report could not be written.
#[derive(Debug)]
pub enum WriteError {
    /// Writing to the destination failed.
    Io(io::Error),
    /// An account's total does not fit in an amount, so it cannot be reported
    /// correctly. Nothing was written.
    TotalOverflow(ClientId),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "could not write the report: {error}"),
            Self::TotalOverflow(client) => write!(
                f,
                "the total of client {client} is too large to report correctly"
            ),
        }
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::TotalOverflow(_) => None,
        }
    }
}

impl From<io::Error> for WriteError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<csv::Error> for WriteError {
    fn from(error: csv::Error) -> Self {
        Self::Io(error.into())
    }
}

/// Writes the header and one row per account, in the order the accounts come.
///
/// Amounts have four decimals and `locked` is `true` or `false`. If any
/// account's total cannot be represented, nothing at all is written.
pub fn write_accounts<W: Write>(
    out: W,
    accounts: impl IntoIterator<Item = (ClientId, Account)>,
) -> Result<(), WriteError> {
    let accounts: Vec<(ClientId, Account)> = accounts.into_iter().collect();

    // Check every total before writing anything, so that a failure leaves the
    // output untouched instead of half a report.
    accounts
        .iter()
        .try_for_each(|(client, account)| total_of(*client, account).map(|_| ()))?;

    let mut writer = csv::Writer::from_writer(out);
    writer.write_record(COLUMNS)?;
    for (client, account) in &accounts {
        writer.write_record([
            client.to_string(),
            account.available().to_string(),
            account.held().to_string(),
            total_of(*client, account)?.to_string(),
            account.is_locked().to_string(),
        ])?;
    }
    writer.flush()?;
    Ok(())
}

/// An account's total, or an error naming the client if it cannot be shown.
fn total_of(client: ClientId, account: &Account) -> Result<Amount, WriteError> {
    account.total().ok_or(WriteError::TotalOverflow(client))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amount::Amount;

    // Amounts are written in ten-thousandths: 10_000 is 1.0, 15_000 is 1.5.

    fn account(available: i64, held: i64, locked: bool) -> Account {
        Account::with_balances(
            Amount::from_units(available),
            Amount::from_units(held),
            locked,
        )
    }

    /// The report for these accounts, as text.
    fn render(accounts: Vec<(ClientId, Account)>) -> String {
        let mut out = Vec::new();
        write_accounts(&mut out, accounts).expect("the report is written");
        String::from_utf8(out).expect("the report is UTF-8")
    }

    /// A destination that always fails, like a full disk.
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk full"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("disk full"))
        }
    }

    #[test]
    fn writes_the_header_and_one_row_per_account() {
        let report = render(vec![
            (1, account(15_000, 0, false)),
            (2, account(20_000, 0, false)),
        ]);
        assert_eq!(
            report,
            "client,available,held,total,locked\n\
             1,1.5000,0.0000,1.5000,false\n\
             2,2.0000,0.0000,2.0000,false\n"
        );
    }

    #[test]
    fn writes_only_the_header_when_there_are_no_accounts() {
        assert_eq!(render(vec![]), "client,available,held,total,locked\n");
    }

    #[test]
    fn rows_are_written_in_the_order_the_accounts_come() {
        let report = render(vec![
            (30, account(0, 0, false)),
            (2, account(0, 0, false)),
            (65_535, account(0, 0, false)),
            (1, account(0, 0, false)),
        ]);
        let clients: Vec<&str> = report
            .lines()
            .skip(1)
            .map(|line| line.split(',').next().expect("a client column"))
            .collect();
        assert_eq!(clients, ["30", "2", "65535", "1"]);
    }

    #[test]
    fn shows_held_funds_and_the_lock() {
        let report = render(vec![(3, account(30_000, 20_000, true))]);
        assert_eq!(
            report,
            "client,available,held,total,locked\n3,3.0000,2.0000,5.0000,true\n"
        );
    }

    #[test]
    fn total_is_available_plus_held_even_when_available_is_negative() {
        let report = render(vec![(4, account(-50_000, 20_000, true))]);
        assert_eq!(
            report,
            "client,available,held,total,locked\n4,-5.0000,2.0000,-3.0000,true\n"
        );
    }

    #[test]
    fn writes_every_possible_client() {
        let accounts: Vec<_> = (0..=u16::MAX)
            .rev()
            .map(|client| (client, account(10_000, 0, false)))
            .collect();
        let report = render(accounts);
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines.len(), 65_537);
        assert_eq!(lines[1], "65535,1.0000,0.0000,1.0000,false");
        assert_eq!(lines[65_536], "0,1.0000,0.0000,1.0000,false");
    }

    #[test]
    fn a_total_that_overflows_is_an_error_and_nothing_is_written() {
        let broken =
            Account::with_balances(Amount::from_units(i64::MAX), Amount::from_units(1), false);
        let mut out = Vec::new();
        let result = write_accounts(&mut out, vec![(1, account(10_000, 0, false)), (7, broken)]);
        assert!(matches!(result, Err(WriteError::TotalOverflow(7))));
        assert!(out.is_empty(), "wrote {} bytes", out.len());
    }

    #[test]
    fn a_failing_destination_is_reported() {
        let result = write_accounts(FailingWriter, vec![(1, account(10_000, 0, false))]);
        assert!(matches!(result, Err(WriteError::Io(_))));
    }

    #[test]
    fn write_errors_describe_themselves() {
        let overflow = WriteError::TotalOverflow(7);
        assert_eq!(
            overflow.to_string(),
            "the total of client 7 is too large to report correctly"
        );
        assert!(std::error::Error::source(&overflow).is_none());

        let io = WriteError::Io(io::Error::other("disk full"));
        assert!(io.to_string().starts_with("could not write the report"));
        assert!(std::error::Error::source(&io).is_some());
    }
}
