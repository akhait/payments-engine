#![forbid(unsafe_code)]

pub mod account;
pub mod amount;
pub mod engine;
pub mod output;
pub mod pipeline;
pub mod transaction;

use std::fmt;
use std::io::{Read, Write};

use tracing::level_filters::LevelFilter;

use crate::engine::Engine;
use crate::output::{WriteError, write_accounts};
use crate::pipeline::{ReadError, read_transactions};

/// How many refused transactions are logged one by one. The rest are counted,
/// so a hostile input cannot flood the log.
const MAX_LOGGED_REJECTIONS: u64 = 20;

/// The log level when `RUST_LOG` does not name one.
pub const DEFAULT_LOG_LEVEL: LevelFilter = LevelFilter::WARN;

/// A run that did not produce a report.
#[derive(Debug)]
pub enum RunError {
    Read(ReadError),
    Write(WriteError),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => error.fmt(f),
            Self::Write(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Write(error) => Some(error),
        }
    }
}

/// Reads transactions from `input`, applies them, and writes the report to
/// `output`.
///
/// Nothing is written unless the whole input could be read. A failure while
/// writing the report can still leave part of it behind. A transaction the
/// engine refuses is logged and does not stop the run.
pub fn run<R: Read, W: Write>(input: R, output: W) -> Result<(), RunError> {
    let mut engine = Engine::default();
    let mut rejected = 0u64;
    let read = apply_all(input, &mut engine, &mut rejected);
    if rejected > 0 {
        tracing::warn!(rejected, "transactions rejected in total");
    }
    read.map_err(RunError::Read)?;
    write_accounts(output, engine.into_accounts()).map_err(RunError::Write)
}

fn apply_all<R: Read>(input: R, engine: &mut Engine, rejected: &mut u64) -> Result<(), ReadError> {
    for item in read_transactions(input)? {
        let transaction = item?;
        if let Err(reason) = engine.apply(transaction) {
            *rejected += 1;
            if *rejected <= MAX_LOGGED_REJECTIONS {
                tracing::warn!(
                    client = transaction.client(),
                    tx = transaction.tx(),
                    %reason,
                    "transaction rejected"
                );
            }
        }
    }
    Ok(())
}

/// The level a `RUST_LOG` value names: one of `off`, `error`, `warn`, `info`,
/// `debug` and `trace`, in any case.
///
/// Numbers, which tracing's own parser also accepts, are not level names here.
pub fn parse_log_level(value: &str) -> Option<LevelFilter> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::OFF),
        "error" => Some(LevelFilter::ERROR),
        "warn" => Some(LevelFilter::WARN),
        "info" => Some(LevelFilter::INFO),
        "debug" => Some(LevelFilter::DEBUG),
        "trace" => Some(LevelFilter::TRACE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::pipeline::MAX_ROW_BYTES;

    const HEADER: &str = "type,client,tx,amount\n";
    const REPORT_HEADER: &str = "client,available,held,total,locked\n";

    /// The report with its rows in client order. The order of the rows is not
    /// specified, so the tests compare them in an order of their own.
    fn in_client_order(report: &str) -> String {
        let mut lines = report.lines();
        let mut text = String::new();
        if let Some(header) = lines.next() {
            text.push_str(header);
            text.push('\n');
        }
        let mut rows: Vec<&str> = lines.collect();
        rows.sort_by_key(|row| {
            row.split(',')
                .next()
                .and_then(|client| client.parse::<u32>().ok())
        });
        for row in rows {
            text.push_str(row);
            text.push('\n');
        }
        text
    }

    fn run_to_string(input: &str) -> Result<String, RunError> {
        let mut out = Vec::new();
        run(input.as_bytes(), &mut out)?;
        Ok(in_client_order(
            &String::from_utf8(out).expect("the report is UTF-8"),
        ))
    }

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
    fn reports_the_accounts_after_applying_every_transaction() {
        let input =
            format!("{HEADER}deposit,1,1,5.0\ndeposit,2,2,3.0\nwithdrawal,1,3,1.5\ndispute,2,2\n");
        assert_eq!(
            run_to_string(&input).unwrap(),
            format!("{REPORT_HEADER}1,3.5000,0.0000,3.5000,false\n2,0.0000,3.0000,3.0000,false\n")
        );
    }

    #[test]
    fn a_refused_transaction_does_not_stop_the_run() {
        let input = format!("{HEADER}deposit,1,1,5.0\nwithdrawal,1,2,9.0\ndeposit,1,3,1.0\n");
        assert_eq!(
            run_to_string(&input).unwrap(),
            format!("{REPORT_HEADER}1,6.0000,0.0000,6.0000,false\n")
        );
    }

    #[test]
    fn a_bad_row_does_not_stop_the_run() {
        let input = format!("{HEADER}deposit,1,1,5.0\nbogus\ndeposit,1,3,1.0\n");
        assert_eq!(
            run_to_string(&input).unwrap(),
            format!("{REPORT_HEADER}1,6.0000,0.0000,6.0000,false\n")
        );
    }

    #[test]
    fn empty_input_gives_a_report_with_no_accounts() {
        assert_eq!(run_to_string("").unwrap(), REPORT_HEADER);
        assert_eq!(run_to_string(HEADER).unwrap(), REPORT_HEADER);
    }

    #[test]
    fn a_bad_header_is_an_error_and_nothing_is_written() {
        let mut out = Vec::new();
        let result = run("deposit,1,1,1.0\n".as_bytes(), &mut out);
        assert!(matches!(result, Err(RunError::Read(_))));
        assert!(out.is_empty());
    }

    #[test]
    fn an_oversize_row_is_an_error_and_nothing_is_written_even_after_valid_rows() {
        let input = format!(
            "{HEADER}deposit,1,1,1.0\n{}\ndeposit,2,2,1.0\n",
            "a".repeat(MAX_ROW_BYTES + 1)
        );
        let mut out = Vec::new();
        let result = run(input.as_bytes(), &mut out);
        assert!(matches!(result, Err(RunError::Read(_))));
        assert!(out.is_empty());
    }

    #[test]
    fn a_failing_destination_is_reported() {
        let input = format!("{HEADER}deposit,1,1,1.0\n");
        let result = run(input.as_bytes(), FailingWriter);
        assert!(matches!(result, Err(RunError::Write(_))));
    }

    #[test]
    fn run_errors_describe_themselves_and_expose_their_cause() {
        let read = run("deposit,1,1,1.0\n".as_bytes(), Vec::new()).unwrap_err();
        assert!(read.to_string().contains("`type`"), "got: {read}");
        assert!(std::error::Error::source(&read).is_some());

        let input = format!("{HEADER}deposit,1,1,1.0\n");
        let write = run(input.as_bytes(), FailingWriter).unwrap_err();
        assert!(write.to_string().starts_with("could not write the report"));
        assert!(std::error::Error::source(&write).is_some());
    }

    #[test]
    fn parses_the_level_names_rust_log_may_hold() {
        let cases = [
            ("off", Some(LevelFilter::OFF)),
            ("error", Some(LevelFilter::ERROR)),
            ("warn", Some(LevelFilter::WARN)),
            ("info", Some(LevelFilter::INFO)),
            ("debug", Some(LevelFilter::DEBUG)),
            ("trace", Some(LevelFilter::TRACE)),
            (" DEBUG ", Some(LevelFilter::DEBUG)),
            ("", None),
            ("bogus", None),
            ("info,payments_engine=debug", None),
            ("Off", Some(LevelFilter::OFF)),
            ("WARN", Some(LevelFilter::WARN)),
            ("0", None),
            ("1", None),
            ("+3", None),
            ("9", None),
        ];
        for (value, expected) in cases {
            assert_eq!(parse_log_level(value), expected, "value {value:?}");
        }
    }

    #[test]
    fn the_default_log_level_is_warn() {
        assert_eq!(DEFAULT_LOG_LEVEL, LevelFilter::WARN);
    }
}
