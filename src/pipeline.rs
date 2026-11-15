//! Reading transactions from CSV input.

use std::fmt;
use std::io::{self, Read};

use csv::{ReaderBuilder, StringRecord, Trim};

use crate::transaction::{MissingAmount, RawRow, Transaction};

/// The longest row accepted, in bytes. A valid row is around 60 bytes; the
/// limit leaves room for padding. Longer input ends the stream with an error,
/// which keeps the memory used per stream small whatever is sent.
pub const MAX_ROW_BYTES: usize = 1024;

/// How many skipped rows are logged one by one. After that they are only
/// counted, so bad input cannot flood the log.
const MAX_LOGGED_SKIPS: u64 = 20;

/// The columns every input must have, each exactly once.
const REQUIRED_COLUMNS: [&str; 4] = ["type", "client", "tx", "amount"];

/// The input could not be read at all, as opposed to one row being bad.
#[derive(Debug)]
pub struct ReadError(ReadErrorKind);

#[derive(Debug)]
enum ReadErrorKind {
    /// Reading failed, or the header could not be read.
    Input(csv::Error),
    /// The header lacks a required column, or repeats it.
    Header(&'static str),
}

impl ReadError {
    fn input(error: csv::Error) -> Self {
        ReadError(ReadErrorKind::Input(error))
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            ReadErrorKind::Input(error) => write!(f, "could not read the input: {error}"),
            ReadErrorKind::Header(column) => write!(
                f,
                "the header row must contain exactly one `{column}` column"
            ),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.0 {
            ReadErrorKind::Input(error) => Some(error),
            ReadErrorKind::Header(_) => None,
        }
    }
}

/// A lazy stream of the transactions in some CSV input.
///
/// Rows are read one at a time and no row may be longer than
/// [`MAX_ROW_BYTES`], so the memory used does not depend on the size of the
/// input. A bad row is logged and skipped. If reading the input fails, or a row
/// is too long, the error is yielded once and the stream ends.
///
/// An input that stops in the middle of a row, without a final line ending, is
/// read as if that row were complete: CSV cannot tell a truncated row from a
/// finished one.
pub struct Transactions<R: Read> {
    reader: csv::Reader<RowLimit<SkipBom<R>>>,
    columns: Columns,
    record: StringRecord,
    /// The four required fields of the row being parsed, reused for every row.
    narrow: StringRecord,
    /// How many rows have been skipped so far.
    skipped: u64,
    finished: bool,
}

/// Starts reading transactions from `reader`.
///
/// Fails if the header row cannot be read, or if it lacks one of the columns
/// `type`, `client`, `tx` and `amount` (or repeats it). Their order does not
/// matter, and other columns are ignored. Input with nothing in it at all has
/// no header and no transactions, which is not an error. A UTF-8 byte order
/// mark at the start is skipped, however the first reads are split.
pub fn read_transactions<R: Read>(reader: R) -> Result<Transactions<R>, ReadError> {
    let mut reader = ReaderBuilder::new()
        // The input pads fields with spaces after the commas.
        .trim(Trim::All)
        // Disputes, resolves and chargebacks may leave out the amount column.
        .flexible(true)
        // Every field is a keyword or a number, so none needs quoting. Without
        // quote handling a bad row stays one line, and a stray quote cannot pull
        // the rows after it into a single record.
        .quoting(false)
        .from_reader(RowLimit::new(SkipBom::new(reader)));
    let headers = reader.headers().map_err(ReadError::input)?.clone();
    check_headers(&headers)?;
    Ok(Transactions {
        reader,
        columns: Columns::new(&headers),
        record: StringRecord::new(),
        narrow: StringRecord::new(),
        skipped: 0,
        finished: false,
    })
}

/// Checks that the header names every required column exactly once.
///
/// Without this, input with a missing or differently spelled header would
/// "succeed" with every row skipped.
fn check_headers(headers: &StringRecord) -> Result<(), ReadError> {
    // Input with nothing in it at all has no transactions, and that is fine.
    if headers.is_empty() {
        return Ok(());
    }
    for column in REQUIRED_COLUMNS {
        if headers.iter().filter(|name| *name == column).count() != 1 {
            return Err(ReadError(ReadErrorKind::Header(column)));
        }
    }
    Ok(())
}

/// Where the required columns are in the header.
///
/// A row is cut down to the four fields it needs before it is parsed, so that
/// the work for a row does not grow with the number of columns in the header,
/// which the input chooses.
struct Columns {
    /// How many columns the header has.
    width: usize,
    /// The position of `type`, `client`, `tx` and `amount`, in that order.
    positions: [usize; 4],
    /// A header that names just the required columns, in that order.
    names: StringRecord,
}

impl Columns {
    /// Finds the required columns in a header that `check_headers` accepted. For
    /// input with nothing in it, which has no rows, the positions are not used.
    fn new(headers: &StringRecord) -> Self {
        let mut positions = [0; 4];
        for (position, column) in positions.iter_mut().zip(REQUIRED_COLUMNS) {
            *position = headers
                .iter()
                .position(|name| name == column)
                .unwrap_or_default();
        }
        Columns {
            width: headers.len(),
            positions,
            names: StringRecord::from(REQUIRED_COLUMNS.to_vec()),
        }
    }
}

/// The bytes that may start a UTF-8 text file to say which encoding it uses.
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Drops a UTF-8 byte order mark at the very start of the input.
///
/// The csv crate strips one too, but only if it arrives within the first read.
/// A mark delivered on its own, as a slow sender may do, was taken for the end
/// of the input and made the whole stream look empty. Here the first three
/// bytes are always collected before anything is passed on.
struct SkipBom<R> {
    inner: R,
    /// The first bytes of the input, held back while looking for a mark.
    head: [u8; 3],
    /// How many bytes of `head` are filled in, and how many were handed on.
    head_len: usize,
    head_pos: usize,
    /// Whether the start of the input has been looked at yet.
    checked: bool,
}

impl<R> SkipBom<R> {
    fn new(inner: R) -> Self {
        SkipBom {
            inner,
            head: [0; 3],
            head_len: 0,
            head_pos: 0,
            checked: false,
        }
    }
}

impl<R: Read> Read for SkipBom<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.checked {
            // Stop early only at the end of the input.
            while self.head_len < self.head.len() {
                match self.inner.read(&mut self.head[self.head_len..]) {
                    Ok(0) => break,
                    Ok(count) => self.head_len += count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
            self.checked = true;
            if self.head[..self.head_len] == UTF8_BOM {
                self.head_len = 0;
            }
        }
        if self.head_pos < self.head_len {
            let count = (self.head_len - self.head_pos).min(buf.len());
            buf[..count].copy_from_slice(&self.head[self.head_pos..self.head_pos + count]);
            self.head_pos += count;
            return Ok(count);
        }
        self.inner.read(buf)
    }
}

/// Passes input through while enforcing [`MAX_ROW_BYTES`].
///
/// The csv crate has no limit of its own: it keeps growing its buffers for as
/// long as a row goes on, so a single endless line could use all the memory.
/// This wrapper cuts such input off with an error instead. It also retries a
/// read that a signal interrupted, which the csv crate would report as a failure.
struct RowLimit<R> {
    inner: R,
    /// Bytes seen since the last line ending.
    row_len: usize,
    /// Set once the limit was passed, so that every later read fails as well.
    exceeded: bool,
}

impl<R> RowLimit<R> {
    fn new(inner: R) -> Self {
        RowLimit {
            inner,
            row_len: 0,
            exceeded: false,
        }
    }
}

impl<R: Read> Read for RowLimit<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.exceeded {
            return Err(row_too_long());
        }
        let count = loop {
            match self.inner.read(buf) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        for (index, &byte) in buf[..count].iter().enumerate() {
            if byte == b'\n' || byte == b'\r' {
                self.row_len = 0;
                continue;
            }
            self.row_len += 1;
            if self.row_len > MAX_ROW_BYTES {
                // Hand over what came before this byte, so the rows already
                // read are not lost, and fail on the next read.
                self.exceeded = true;
                return if index == 0 {
                    Err(row_too_long())
                } else {
                    Ok(index)
                };
            }
        }
        Ok(count)
    }
}

/// The error for a row over the limit. Fixed text: it never holds input.
fn row_too_long() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "a row is longer than the allowed maximum",
    )
}

/// Why a row that was read cannot become a transaction.
///
/// The messages are fixed text on purpose: they are logged, and a bad row can
/// hold anything, including very long values or line breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowError {
    /// The row could not be read, for example because it is not valid UTF-8.
    Unreadable,
    /// A field is missing, out of range or not of the expected form.
    InvalidField,
    /// A deposit or withdrawal has no amount.
    MissingAmount,
}

impl fmt::Display for RowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Unreadable => "the row could not be read",
            Self::InvalidField => "a field is missing, out of range or not valid",
            Self::MissingAmount => "a deposit or withdrawal has no amount",
        };
        f.write_str(message)
    }
}

fn parse_row(
    record: &StringRecord,
    columns: &Columns,
    narrow: &mut StringRecord,
) -> Result<Transaction, RowError> {
    // Rows may have any length, but fields beyond the header would be dropped
    // silently (a decimal comma written as an extra column, for instance).
    if record.len() > columns.width {
        return Err(RowError::InvalidField);
    }
    // Keep just the required fields. A row may stop early, such as a dispute with
    // no amount: a field it does not have is read as an empty one. A required
    // field that was missing is then empty and still fails to parse, and an empty
    // amount means there is none.
    narrow.clear();
    for position in columns.positions {
        narrow.push_field(record.get(position).unwrap_or(""));
    }
    let row: RawRow = narrow
        .deserialize(Some(&columns.names))
        .map_err(|_| RowError::InvalidField)?;
    Transaction::try_from(row).map_err(|MissingAmount| RowError::MissingAmount)
}

impl<R: Read> Transactions<R> {
    /// Counts a skipped row and logs it, but only for the first few: after
    /// that, bad input would otherwise turn into a flood of log lines.
    fn skip(&mut self, row: u64, reason: RowError) {
        self.skipped += 1;
        if self.skipped <= MAX_LOGGED_SKIPS {
            tracing::warn!(row, %reason, "skipping row");
        }
    }

    /// Ends the stream, and says how many rows were skipped in all.
    fn finish(&mut self) {
        self.finished = true;
        if self.skipped > 0 {
            tracing::warn!(skipped = self.skipped, "rows skipped in total");
        }
    }
}

/// A caller that gives up early, for instance because a worker is gone, still
/// gets to know how many rows were skipped before that.
impl<R: Read> Drop for Transactions<R> {
    fn drop(&mut self) {
        if !self.finished {
            self.finish();
        }
    }
}

impl<R: Read> Iterator for Transactions<R> {
    type Item = Result<Transaction, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.finished {
            match self.reader.read_record(&mut self.record) {
                Ok(false) => self.finish(),
                Ok(true) => match parse_row(&self.record, &self.columns, &mut self.narrow) {
                    Ok(transaction) => return Some(Ok(transaction)),
                    Err(reason) => self.skip(row_of(self.record.position()), reason),
                },
                // The input itself is failing, so there is nothing sensible left
                // to read: report it once and stop.
                Err(error) if error.is_io_error() => {
                    self.finish();
                    return Some(Err(ReadError::input(error)));
                }
                // Any other read error concerns this one row, which the reader
                // has already consumed.
                Err(error) => self.skip(row_of(error.position()), RowError::Unreadable),
            }
        }
        None
    }
}

/// The number of a data row, counting from 1 and leaving out the header.
///
/// This is the position among the rows read, so blank lines do not count and
/// the numbering does not depend on the line endings. It is 0 if the reader
/// did not say.
fn row_of(position: Option<&csv::Position>) -> u64 {
    position.map_or(0, csv::Position::record)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::amount::Amount;
    use crate::test_support::capture_logs;
    use crate::transaction::{ClientId, TxId};

    // Amounts are written in ten-thousandths: 10_000 is 1.0, 15_000 is 1.5.

    const HEADER: &str = "type,client,tx,amount\n";

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

    /// Reads all transactions, failing the test on a fatal error.
    fn read_bytes(input: &[u8]) -> Vec<Transaction> {
        read_transactions(input)
            .expect("the header is readable")
            .map(|row| row.expect("no fatal error"))
            .collect()
    }

    fn read(input: &str) -> Vec<Transaction> {
        read_bytes(input.as_bytes())
    }

    /// The row numbers named by the "skipping row" log lines, in order.
    fn logged_rows(logs: &str) -> Vec<u64> {
        logs.lines()
            .filter(|line| line.contains("skipping row"))
            .filter_map(|line| {
                line.split_whitespace()
                    .find_map(|word| word.strip_prefix("row="))
                    .and_then(|number| number.parse().ok())
            })
            .collect()
    }

    /// Hands the data out in pieces of at most `chunk` bytes, the way a network
    /// stream can.
    struct Chunked<'a> {
        data: &'a [u8],
        chunk: usize,
    }

    impl Read for Chunked<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let count = self.data.len().min(self.chunk).min(buf.len());
            buf[..count].copy_from_slice(&self.data[..count]);
            self.data = &self.data[count..];
            Ok(count)
        }
    }

    /// Read sizes that put the end of a read in awkward places: one byte, a few
    /// bytes, and sizes around the row limit.
    const CHUNK_SIZES: [usize; 9] = [1, 2, 3, 4, 59, 1023, 1024, 1025, 4096];

    /// Everything the reader yields for `input` delivered in reads of `chunk`
    /// bytes. A failure to start reading counts as a single error item.
    fn read_chunked(input: &[u8], chunk: usize) -> Vec<Result<Transaction, ReadError>> {
        match read_transactions(Chunked { data: input, chunk }) {
            Ok(rows) => rows.collect(),
            Err(error) => vec![Err(error)],
        }
    }

    /// The transactions of a chunked read that is expected to have no errors.
    fn transactions_of(results: Vec<Result<Transaction, ReadError>>) -> Vec<Transaction> {
        results
            .into_iter()
            .map(|row| row.expect("no error expected"))
            .collect()
    }

    /// Hands out `data` once and then fails, like a connection that drops.
    struct FailingReader {
        data: &'static [u8],
    }

    impl Read for FailingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.data.is_empty() {
                return Err(io::Error::other("connection lost"));
            }
            let count = self.data.len().min(buf.len());
            buf[..count].copy_from_slice(&self.data[..count]);
            self.data = &self.data[count..];
            Ok(count)
        }
    }

    /// Fails once with `Interrupted`, the way a signal can interrupt a read,
    /// and then behaves.
    struct InterruptedOnce {
        data: &'static [u8],
        interrupted: bool,
    }

    impl Read for InterruptedOnce {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = self.data.len().min(buf.len());
            buf[..count].copy_from_slice(&self.data[..count]);
            self.data = &self.data[count..];
            Ok(count)
        }
    }

    /// Hands out `first`, then fails once with `Interrupted`, then hands out
    /// `rest`: a read interrupted in the middle of the input.
    struct InterruptedMidway {
        first: &'static [u8],
        rest: &'static [u8],
        interrupted: bool,
    }

    impl Read for InterruptedMidway {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let source = if !self.first.is_empty() {
                &mut self.first
            } else if !self.interrupted {
                self.interrupted = true;
                return Err(io::ErrorKind::Interrupted.into());
            } else {
                &mut self.rest
            };
            let count = source.len().min(buf.len());
            buf[..count].copy_from_slice(&source[..count]);
            *source = &source[count..];
            Ok(count)
        }
    }

    /// Counts how many bytes have been taken from the wrapped reader.
    struct CountingReader<R> {
        inner: R,
        bytes_read: Arc<AtomicUsize>,
    }

    impl<R: Read> Read for CountingReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let count = self.inner.read(buf)?;
            self.bytes_read.fetch_add(count, Ordering::Relaxed);
            Ok(count)
        }
    }

    #[test]
    fn reads_every_transaction_type() {
        let input = format!(
            "{HEADER}deposit,1,1,1.5\nwithdrawal,1,2,0.5\ndispute,1,1,\nresolve,1,1,\nchargeback,1,1,\n"
        );
        assert_eq!(
            read(&input),
            vec![
                deposit(1, 1, 15_000),
                withdrawal(1, 2, 5_000),
                Transaction::Dispute { client: 1, tx: 1 },
                Transaction::Resolve { client: 1, tx: 1 },
                Transaction::Chargeback { client: 1, tx: 1 },
            ]
        );
    }

    #[test]
    fn accepts_padding_around_the_header_and_fields() {
        let input = "type, client, tx, amount\ndeposit, 1, 1, 1.0\n  withdrawal ,2 , 2,  0.5 \n";
        assert_eq!(
            read(input),
            vec![deposit(1, 1, 10_000), withdrawal(2, 2, 5_000)]
        );
    }

    #[test]
    fn accepts_rows_without_the_amount_field_for_disputes_resolves_and_chargebacks() {
        let input = format!("{HEADER}dispute,1,1\nresolve,1,1\nchargeback,1,1\n");
        assert_eq!(
            read(&input),
            vec![
                Transaction::Dispute { client: 1, tx: 1 },
                Transaction::Resolve { client: 1, tx: 1 },
                Transaction::Chargeback { client: 1, tx: 1 },
            ]
        );
    }

    #[test]
    fn ignores_a_well_formed_amount_given_on_a_dispute_row() {
        let input = format!("{HEADER}dispute,1,1,5.0\n");
        assert_eq!(
            read(&input),
            vec![Transaction::Dispute { client: 1, tx: 1 }]
        );
    }

    #[test]
    fn accepts_the_largest_client_and_tx_ids() {
        let input = format!("{HEADER}deposit,65535,4294967295,1.0\n");
        assert_eq!(read(&input), vec![deposit(65_535, 4_294_967_295, 10_000)]);
    }

    #[test]
    fn accepts_crlf_line_endings() {
        let input = "type,client,tx,amount\r\ndeposit,1,1,1.0\r\nwithdrawal,1,2,0.5\r\n";
        assert_eq!(
            read(input),
            vec![deposit(1, 1, 10_000), withdrawal(1, 2, 5_000)]
        );
    }

    #[test]
    fn reads_columns_in_any_order() {
        let input = "client , type,amount,tx\n1,deposit,2.0,7\n";
        assert_eq!(read(input), vec![deposit(1, 7, 20_000)]);
    }

    #[test]
    fn ignores_extra_columns_named_in_the_header() {
        let input = "type,client,tx,amount,note\ndeposit,1,1,1.0,hello\n";
        assert_eq!(read(input), vec![deposit(1, 1, 10_000)]);
    }

    #[test]
    fn empty_input_and_header_only_input_give_no_transactions() {
        assert_eq!(read(""), vec![]);
        assert_eq!(read(HEADER), vec![]);
    }

    #[test]
    fn input_whose_header_lacks_a_required_column_is_rejected() {
        // (input, the column the error should name)
        let cases = [
            ("deposit,1,1,1.0\n", "type"),                // no header row at all
            ("Type,Client,Tx,Amount\n", "type"),          // the names are case sensitive
            ("type,client,tx\n", "amount"),               // a column is missing
            ("type,client,client,tx,amount\n", "client"), // a column is repeated
        ];
        for (input, column) in cases {
            let Err(error) = read_transactions(input.as_bytes()) else {
                panic!("input {input:?} should be rejected");
            };
            assert!(
                error.to_string().contains(&format!("`{column}`")),
                "input {input:?} gave: {error}"
            );
        }
    }

    #[test]
    fn skips_malformed_rows_and_keeps_reading() {
        let bad_rows = [
            "transfer,1,1,1.0",         // unknown type
            "DEPOSIT,1,1,1.0",          // the type is case sensitive
            "deposit,70000,1,1.0",      // client above u16
            "deposit,-1,1,1.0",         // negative client
            "deposit,1,4294967296,1.0", // tx above u32
            "deposit,abc,1,1.0",        // client is not a number
            "deposit,1,abc,1.0",        // tx is not a number
            "deposit,,1,1.0",           // empty client
            "deposit,0x10,1,1.0",       // hexadecimal client
            "deposit,1,0x1F,1.0",       // hexadecimal tx
            "deposit,+1,2,1.0",         // signed client
            "deposit,1,1,-1.0",         // negative amount
            "deposit,1,1,1.00001",      // more than four decimals
            "deposit,1,1,abc",          // amount is not a number
            "deposit,1,1,",             // deposit without an amount
            "deposit,1,1",              // deposit without the amount field
            "withdrawal,1,1,",          // withdrawal without an amount
            "deposit,1",                // too few fields
            "deposit,1,2,1,5",          // decimal comma written as an extra column
            "deposit,1,1,1.0,garbage",  // extra field
            "dispute,1,1,abc",          // a malformed amount is still a bad field
            "dispute,1,1,-1",           // so is a negative one
            "\"deposit\",1,1,1.0",      // quoting is not supported
            "deposit,\"2\",1,1.0",      // quoting is not supported
            "deposit,1,\"2,1.0",        // a stray quote must not swallow what follows
        ];
        for bad in bad_rows {
            let input = format!("{HEADER}{bad}\ndeposit,9,9,1.0\n");
            assert_eq!(read(&input), vec![deposit(9, 9, 10_000)], "row {bad:?}");
        }
    }

    #[test]
    fn a_stray_quote_costs_only_its_own_row() {
        let input = format!(
            "{HEADER}\"deposit\",1,1,1.0\ndeposit,\"2\",2,1.0\ndeposit,3,\"3,1.0\ndeposit,4,4,1.0\ndeposit,5,5,1.0\n"
        );
        assert_eq!(
            read(&input),
            vec![deposit(4, 4, 10_000), deposit(5, 5, 10_000)]
        );
    }

    #[test]
    fn keeps_good_rows_around_bad_ones_in_order_and_ignores_blank_lines() {
        let input = format!("{HEADER}deposit,1,1,1.0\nbogus\n\n\nwithdrawal,1,2,0.5\n");
        assert_eq!(
            read(&input),
            vec![deposit(1, 1, 10_000), withdrawal(1, 2, 5_000)]
        );
    }

    #[test]
    fn skips_rows_that_are_not_valid_utf8() {
        let input = b"type,client,tx,amount\ndeposit,1,1,\xff\xfe\ndeposit,2,2,1.0\n";
        assert_eq!(read_bytes(input), vec![deposit(2, 2, 10_000)]);
    }

    #[test]
    fn logs_the_data_row_number_of_each_skipped_row() {
        // Rows are numbered among the data rows: the header is not counted, and
        // neither are blank lines. The numbering must not depend on the line endings.
        let cases: [(&[u8], Vec<u64>); 4] = [
            (b"type,client,tx,amount\nbad\ndeposit,1,1,1.0\nbad\n", vec![1, 3]),
            (
                b"type,client,tx,amount\r\nbad1\r\ndeposit,1,1,1.0\r\nbad4\r\nbad5\r\ndeposit,1,2,1.0\r\nbad7\r\n",
                vec![1, 3, 4, 6],
            ),
            (
                b"type,client,tx,amount\n\n\ndeposit,1,1,1.0\n\n\ndeposit,1,x,1.0\n",
                vec![2],
            ),
            (b"type,client,tx,amount\n\n\n\ndeposit,1,1,\xff\n", vec![1]),
        ];
        for (input, expected) in cases {
            let (_, logs) = capture_logs(|| read_bytes(input));
            assert_eq!(logged_rows(&logs), expected, "input {input:?}");
        }
    }

    #[test]
    fn logs_a_fixed_reason_and_never_the_row_content() {
        let input = format!(
            "{HEADER}SECRETTYPE,1,1,1.0\ndeposit,SECRETID,1,1.0\ndeposit,1,1,SECRETAMOUNT\n"
        );
        let (_, logs) = capture_logs(|| read(&input));
        assert!(logs.contains("skipping row"), "logs: {logs}");
        assert!(!logs.contains("SECRET"), "logs: {logs}");
    }

    #[test]
    fn logs_only_the_first_skipped_rows_and_then_one_summary() {
        let input = format!("{HEADER}{}", "bad\n".repeat(100));
        let (_, logs) = capture_logs(|| read(&input));
        let individual = logs.lines().filter(|l| l.contains("skipping row")).count();
        assert_eq!(individual, MAX_LOGGED_SKIPS as usize, "logs: {logs}");
        let summaries: Vec<_> = logs
            .lines()
            .filter(|l| l.contains("rows skipped in total"))
            .collect();
        assert_eq!(summaries.len(), 1, "logs: {logs}");
        assert!(summaries[0].contains("skipped=100"), "logs: {logs}");
    }

    #[test]
    fn logs_nothing_for_clean_input() {
        let input = format!("{HEADER}deposit,1,1,1.0\nwithdrawal,1,2,0.5\n");
        let (_, logs) = capture_logs(|| read(&input));
        assert!(logs.is_empty(), "logs: {logs}");
    }

    #[test]
    fn an_io_error_after_some_rows_is_reported_and_ends_the_stream() {
        let data = b"type,client,tx,amount\ndeposit,1,1,1.0\ndeposit,2,2,2.0\n";
        let mut rows = read_transactions(FailingReader { data }).expect("header is readable");
        assert_eq!(rows.next().unwrap().unwrap(), deposit(1, 1, 10_000));
        assert_eq!(rows.next().unwrap().unwrap(), deposit(2, 2, 20_000));
        assert!(rows.next().unwrap().is_err());
        assert!(rows.next().is_none());
    }

    #[test]
    fn an_io_error_while_reading_the_header_is_reported() {
        let result = read_transactions(FailingReader { data: b"" });
        assert!(result.is_err());
    }

    #[test]
    fn a_read_interrupted_once_is_retried() {
        let reader = InterruptedOnce {
            data: b"type,client,tx,amount\ndeposit,1,1,1.0\n",
            interrupted: false,
        };
        let rows: Vec<_> = read_transactions(reader)
            .expect("header is readable")
            .map(|row| row.expect("no fatal error"))
            .collect();
        assert_eq!(rows, vec![deposit(1, 1, 10_000)]);
    }

    #[test]
    fn a_row_longer_than_the_limit_ends_the_stream_with_an_error() {
        let long = "a".repeat(MAX_ROW_BYTES + 1);
        let input = format!("{HEADER}deposit,1,1,1.0\n{long}\ndeposit,2,2,1.0\n");
        let mut rows = read_transactions(input.as_bytes()).expect("header is readable");
        assert_eq!(rows.next().unwrap().unwrap(), deposit(1, 1, 10_000));
        assert!(rows.next().unwrap().is_err());
        assert!(rows.next().is_none());
    }

    #[test]
    fn a_row_exactly_at_the_limit_is_accepted_and_one_byte_more_is_not() {
        let core = "deposit,1,1,1.0";
        let at_limit = format!("{core}{}", " ".repeat(MAX_ROW_BYTES - core.len()));
        assert_eq!(at_limit.len(), MAX_ROW_BYTES);
        assert_eq!(
            read(&format!("{HEADER}{at_limit}\n")),
            vec![deposit(1, 1, 10_000)]
        );

        let input = format!("{HEADER}{at_limit} \n");
        let mut rows = read_transactions(input.as_bytes()).expect("header is readable");
        assert!(rows.next().unwrap().is_err());
    }

    #[test]
    fn many_short_rows_are_not_mistaken_for_one_long_row() {
        let mut input = String::from(HEADER);
        for tx in 1..=1_000u32 {
            input.push_str(&format!("deposit,1,{tx},1.0\n"));
        }
        assert_eq!(read(&input).len(), 1_000);
    }

    #[test]
    fn rows_ended_by_a_lone_carriage_return_are_not_mistaken_for_one_long_row() {
        let mut input = String::from("type,client,tx,amount\r");
        for tx in 1..=1_000u32 {
            input.push_str(&format!("deposit,1,{tx},1.0\r"));
        }
        assert_eq!(read(&input).len(), 1_000);
    }

    #[test]
    fn an_oversize_row_ends_the_stream_whatever_the_read_sizes() {
        let long = "a".repeat(MAX_ROW_BYTES + 1);
        let input = format!("{HEADER}deposit,1,1,1.0\n{long}\ndeposit,2,2,1.0\n");
        for chunk in CHUNK_SIZES {
            let results = read_chunked(input.as_bytes(), chunk);
            assert_eq!(results.len(), 2, "chunk {chunk}");
            assert_eq!(
                results[0].as_ref().unwrap(),
                &deposit(1, 1, 10_000),
                "chunk {chunk}"
            );
            assert!(results[1].is_err(), "chunk {chunk}");
        }
    }

    #[test]
    fn a_row_at_the_limit_is_accepted_whatever_the_read_sizes() {
        let core = "deposit,1,1,1.0";
        let at_limit = format!("{core}{}", " ".repeat(MAX_ROW_BYTES - core.len()));
        let input = format!("{HEADER}{at_limit}\ndeposit,2,2,1.0\n");
        for chunk in CHUNK_SIZES {
            assert_eq!(
                transactions_of(read_chunked(input.as_bytes(), chunk)),
                vec![deposit(1, 1, 10_000), deposit(2, 2, 10_000)],
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn the_start_of_an_oversize_row_is_never_taken_for_a_row() {
        // The first 1024 bytes of this row are a valid deposit; one byte more follows.
        let core = "deposit,1,2,1.0";
        let row = format!("{core}{}x", " ".repeat(MAX_ROW_BYTES - core.len()));
        let input = format!("{HEADER}{row}\n");
        for chunk in CHUNK_SIZES {
            let results = read_chunked(input.as_bytes(), chunk);
            assert!(results[0].is_err(), "chunk {chunk}");
        }
    }

    #[test]
    fn the_limits_are_the_documented_ones() {
        assert_eq!(MAX_ROW_BYTES, 1024);
        assert_eq!(MAX_LOGGED_SKIPS, 20);
    }

    #[test]
    fn a_byte_order_mark_is_ignored_however_the_input_is_split() {
        let mut input = vec![0xEF, 0xBB, 0xBF];
        input.extend_from_slice(HEADER.as_bytes());
        input.extend_from_slice(b"deposit,1,1,1.0\ndeposit,2,2,2.0\n");
        for chunk in CHUNK_SIZES {
            assert_eq!(
                transactions_of(read_chunked(&input, chunk)),
                vec![deposit(1, 1, 10_000), deposit(2, 2, 20_000)],
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn a_byte_order_mark_on_its_own_is_an_empty_input() {
        for chunk in CHUNK_SIZES {
            assert_eq!(
                transactions_of(read_chunked(&[0xEF, 0xBB, 0xBF], chunk)),
                vec![],
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn a_byte_order_mark_does_not_count_towards_the_row_limit() {
        let name_len = "type,client,tx,amount".len();
        let header = format!(
            "type,client,tx,amount{}",
            " ".repeat(MAX_ROW_BYTES - name_len)
        );
        let mut input = vec![0xEF, 0xBB, 0xBF];
        input.extend_from_slice(format!("{header}\ndeposit,1,1,1.0\n").as_bytes());
        for chunk in CHUNK_SIZES {
            assert_eq!(
                transactions_of(read_chunked(&input, chunk)),
                vec![deposit(1, 1, 10_000)],
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn rows_that_stop_before_extra_header_columns_are_still_read() {
        let input = "type,client,tx,amount,note\ndispute,1,1\ndispute,2,2,\ndispute,3,3,5.0\ndeposit,1,4,1.0\ndeposit,1,5,1.0,hello\n";
        assert_eq!(
            read(input),
            vec![
                Transaction::Dispute { client: 1, tx: 1 },
                Transaction::Dispute { client: 2, tx: 2 },
                Transaction::Dispute { client: 3, tx: 3 },
                deposit(1, 4, 10_000),
                deposit(1, 5, 10_000),
            ]
        );

        // The extra column may also sit before the amount.
        let input = "type,client,tx,note,amount\ndispute,1,1\ndeposit,1,2,x,1.0\n";
        assert_eq!(
            read(input),
            vec![
                Transaction::Dispute { client: 1, tx: 1 },
                deposit(1, 2, 10_000)
            ]
        );
    }

    #[test]
    fn a_short_row_still_needs_the_columns_it_cannot_do_without() {
        // The missing amount of a deposit, and missing ids, are still errors.
        let input = "type,client,tx,amount,note\ndeposit,1,1\ndeposit,1\ndeposit,9,9,1.0,x\n";
        assert_eq!(read(input), vec![deposit(9, 9, 10_000)]);
    }

    #[test]
    fn a_header_without_a_tx_column_is_rejected() {
        let Err(error) = read_transactions("type,client,amount\n".as_bytes()) else {
            panic!("a header without `tx` should be rejected");
        };
        assert!(error.to_string().contains("`tx`"), "got: {error}");
    }

    #[test]
    fn the_summary_is_logged_even_when_the_stream_ends_with_an_error() {
        let data = b"type,client,tx,amount\nbad\nbad\n";
        let (first, logs) = capture_logs(|| {
            let mut rows = read_transactions(FailingReader { data }).expect("header is readable");
            // Stop at the first item, as a caller that gives up on an error would.
            rows.next()
        });
        assert!(first.expect("an item").is_err());
        assert!(logs.contains("skipped=2"), "logs: {logs}");
    }

    #[test]
    fn a_row_is_narrowed_to_the_four_required_columns_whatever_the_header_width() {
        // 500 extra columns before the required ones: the work for a row must not
        // grow with them, so the row is cut down to the four fields it needs.
        let mut names = vec!["junk"; 500];
        names.extend(["type", "client", "tx", "amount"]);
        let headers = StringRecord::from(names);
        check_headers(&headers).expect("the header is fine");
        let columns = Columns::new(&headers);

        let mut fields = vec!["x"; 500];
        fields.extend(["dispute", "1", "2"]); // stops before the amount column
        let record = StringRecord::from(fields);
        let mut narrow = StringRecord::new();
        assert_eq!(
            parse_row(&record, &columns, &mut narrow),
            Ok(Transaction::Dispute { client: 1, tx: 2 })
        );
        assert_eq!(narrow.len(), 4);

        // A row wider than the header is still a bad row.
        let mut too_many = vec!["x"; 504];
        too_many.push("y");
        assert_eq!(
            parse_row(&StringRecord::from(too_many), &columns, &mut narrow),
            Err(RowError::InvalidField)
        );
    }

    #[test]
    fn the_summary_is_logged_when_the_stream_is_dropped_before_its_end() {
        let data = "type,client,tx,amount\nbad\nbad\nbad\ndeposit,1,1,1.0\ndeposit,1,2,1.0\n";
        let (first, logs) = capture_logs(|| {
            let mut rows = read_transactions(data.as_bytes()).expect("header is readable");
            let first = rows.next();
            drop(rows);
            first
        });
        assert!(first.expect("an item").is_ok());
        assert!(logs.contains("skipped=3"), "logs: {logs}");
    }

    #[test]
    fn the_summary_is_logged_once_when_a_finished_stream_is_dropped() {
        let data = "type,client,tx,amount\nbad\ndeposit,1,1,1.0\n";
        let ((), logs) = capture_logs(|| {
            let rows = read_transactions(data.as_bytes()).expect("header is readable");
            assert_eq!(rows.count(), 1);
        });
        let summaries = logs.matches("rows skipped in total").count();
        assert_eq!(summaries, 1, "logs: {logs}");
    }

    #[test]
    fn no_summary_is_logged_when_a_dropped_stream_skipped_nothing() {
        let data = "type,client,tx,amount\ndeposit,1,1,1.0\ndeposit,1,2,1.0\n";
        let (_, logs) = capture_logs(|| {
            let mut rows = read_transactions(data.as_bytes()).expect("header is readable");
            rows.next()
        });
        assert_eq!(logs, "");
    }

    #[test]
    fn logs_say_why_a_row_was_skipped() {
        let cases: [(&[u8], RowError); 3] = [
            (
                b"type,client,tx,amount\nbogus,1,1,1.0\n",
                RowError::InvalidField,
            ),
            (
                b"type,client,tx,amount\ndeposit,1,1,\n",
                RowError::MissingAmount,
            ),
            (
                b"type,client,tx,amount\ndeposit,1,1,\xff\n",
                RowError::Unreadable,
            ),
        ];
        for (input, reason) in cases {
            let (_, logs) = capture_logs(|| read_bytes(input));
            assert!(logs.contains(&reason.to_string()), "logs: {logs}");
        }
    }

    #[test]
    fn a_read_interrupted_in_the_middle_of_the_input_is_retried() {
        let reader = InterruptedMidway {
            first: b"type,client,tx,amount\ndeposit,1,1,1.0\n",
            rest: b"deposit,2,2,2.0\n",
            interrupted: false,
        };
        let rows: Vec<_> = read_transactions(reader)
            .expect("header is readable")
            .map(|row| row.expect("no fatal error"))
            .collect();
        assert_eq!(rows, vec![deposit(1, 1, 10_000), deposit(2, 2, 20_000)]);
    }

    #[test]
    fn the_byte_order_mark_wrapper_retries_an_interrupted_read_itself() {
        // Called directly, so that the retry in `RowLimit` cannot hide a missing one.
        let reader = InterruptedOnce {
            data: b"abcdef",
            interrupted: false,
        };
        let mut buf = [0u8; 8];
        let count = SkipBom::new(reader)
            .read(&mut buf)
            .expect("the interrupted read is retried");
        assert_eq!(&buf[..count], b"abc");
    }

    #[test]
    fn an_endless_row_is_cut_off_after_a_bounded_number_of_bytes() {
        let bytes_read = Arc::new(AtomicUsize::new(0));
        let hostile = HEADER.as_bytes().chain(io::repeat(b'a').take(10 << 20));
        let reader = CountingReader {
            inner: hostile,
            bytes_read: Arc::clone(&bytes_read),
        };
        let mut rows = read_transactions(reader).expect("header is readable");
        assert!(rows.next().unwrap().is_err());
        let consumed = bytes_read.load(Ordering::Relaxed);
        assert!(
            consumed < 64 * 1024,
            "read {consumed} bytes of one endless row"
        );
    }

    #[test]
    fn reading_the_first_row_does_not_read_the_whole_input() {
        let mut input = String::from(HEADER);
        for tx in 1..=100_000u32 {
            input.push_str(&format!("deposit,1,{tx},1.0\n"));
        }
        let total = input.len();
        let bytes_read = Arc::new(AtomicUsize::new(0));
        let reader = CountingReader {
            inner: input.as_bytes(),
            bytes_read: Arc::clone(&bytes_read),
        };
        let mut rows = read_transactions(reader).expect("header is readable");
        assert_eq!(rows.next().unwrap().unwrap(), deposit(1, 1, 10_000));
        let consumed = bytes_read.load(Ordering::Relaxed);
        assert!(
            consumed < total / 10,
            "read {consumed} of {total} bytes for one row"
        );
    }

    #[test]
    fn a_read_error_describes_itself_and_exposes_its_cause() {
        let Err(error) = read_transactions(FailingReader { data: b"" }) else {
            panic!("reading the header should fail");
        };
        assert!(error.to_string().starts_with("could not read the input"));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn row_errors_have_fixed_readable_messages() {
        let cases = [
            (RowError::Unreadable, "the row could not be read"),
            (
                RowError::InvalidField,
                "a field is missing, out of range or not valid",
            ),
            (
                RowError::MissingAmount,
                "a deposit or withdrawal has no amount",
            ),
        ];
        for (error, message) in cases {
            assert_eq!(error.to_string(), message);
        }
    }
}
