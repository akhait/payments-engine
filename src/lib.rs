#![forbid(unsafe_code)]

pub mod account;
pub mod amount;
pub mod engine;
pub mod output;
pub mod pipeline;
mod refusals;
pub mod sharded;
#[cfg(test)]
mod test_support;
pub mod transaction;

use std::fmt;
use std::io::{Read, Write};
use std::num::NonZeroUsize;

use crate::account::Account;
use crate::output::{WriteError, write_accounts};
use crate::sharded::{PipelineError, run_pipeline_from_reader};
use crate::transaction::ClientId;

/// How many transactions each worker's channel holds when the command line
/// runs the pipeline.
pub const DEFAULT_CHANNEL_CAPACITY: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

/// How many workers the command line starts, and how many threads its runtime
/// gets: one per core the operating system reports, and one if it cannot tell.
pub fn default_shards() -> NonZeroUsize {
    std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
}

/// A run that did not produce a report.
#[derive(Debug)]
pub enum RunError {
    /// The input could not be turned into accounts: see [`PipelineError`].
    Pipeline(PipelineError),
    /// The accounts were computed but the report could not be written.
    Write(WriteError),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pipeline(error) => error.fmt(f),
            Self::Write(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Pipeline(error) => Some(error),
            Self::Write(error) => Some(error),
        }
    }
}

/// Reads transactions from `input`, applies them with `shards` workers, and
/// writes the report to `output`, with its rows in no particular order.
///
/// Nothing is written unless the whole input could be read and every worker
/// finished. A failure while writing the report can still leave part of it
/// behind. A transaction the engine refuses is logged and does not stop the
/// run. Each worker's channel holds at most `capacity` transactions. `shards` and
/// `capacity` are lowered to the maxima that
/// [`run_pipeline`](crate::sharded::run_pipeline) documents.
///
/// Must be awaited inside a Tokio runtime, where `input` is read on a thread
/// that may block (see [`run_pipeline_from_reader`]). `output` is written on
/// the thread that awaits this function, after the pipeline is done.
pub async fn run<R, W>(
    input: R,
    output: W,
    shards: NonZeroUsize,
    capacity: NonZeroUsize,
) -> Result<(), RunError>
where
    R: Read + Send + 'static,
    W: Write,
{
    report(
        run_pipeline_from_reader(input, shards, capacity).await,
        output,
    )
}

/// Writes the accounts of a pipeline that succeeded. If it failed, the output
/// is left untouched and the failure is returned.
fn report<W: Write>(
    accounts: Result<Vec<(ClientId, Account)>, PipelineError>,
    output: W,
) -> Result<(), RunError> {
    let accounts = accounts.map_err(RunError::Pipeline)?;
    write_accounts(output, accounts).map_err(RunError::Write)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::pipeline::MAX_ROW_BYTES;
    use crate::test_support::{FailingWriter, a_read_error, nz};

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

    async fn run_with_shards(input: &str, shards: usize) -> Result<String, RunError> {
        let mut out = Vec::new();
        run(
            Cursor::new(input.as_bytes().to_vec()),
            &mut out,
            nz(shards),
            nz(4),
        )
        .await?;
        Ok(in_client_order(
            &String::from_utf8(out).expect("the report is UTF-8"),
        ))
    }

    async fn run_to_string(input: &str) -> Result<String, RunError> {
        run_with_shards(input, 4).await
    }

    #[tokio::test]
    async fn reports_the_accounts_after_applying_every_transaction() {
        let input =
            format!("{HEADER}deposit,1,1,5.0\ndeposit,2,2,3.0\nwithdrawal,1,3,1.5\ndispute,2,2\n");
        assert_eq!(
            run_to_string(&input).await.unwrap(),
            format!("{REPORT_HEADER}1,3.5000,0.0000,3.5000,false\n2,0.0000,3.0000,3.0000,false\n")
        );
    }

    #[tokio::test]
    async fn a_refused_transaction_does_not_stop_the_run() {
        let input = format!("{HEADER}deposit,1,1,5.0\nwithdrawal,1,2,9.0\ndeposit,1,3,1.0\n");
        assert_eq!(
            run_to_string(&input).await.unwrap(),
            format!("{REPORT_HEADER}1,6.0000,0.0000,6.0000,false\n")
        );
    }

    #[tokio::test]
    async fn a_bad_row_does_not_stop_the_run() {
        let input = format!("{HEADER}deposit,1,1,5.0\nbogus\ndeposit,1,3,1.0\n");
        assert_eq!(
            run_to_string(&input).await.unwrap(),
            format!("{REPORT_HEADER}1,6.0000,0.0000,6.0000,false\n")
        );
    }

    #[tokio::test]
    async fn empty_input_gives_a_report_with_no_accounts() {
        assert_eq!(run_to_string("").await.unwrap(), REPORT_HEADER);
        assert_eq!(run_to_string(HEADER).await.unwrap(), REPORT_HEADER);
    }

    #[tokio::test]
    async fn a_bad_header_is_an_error_and_nothing_is_written() {
        let mut out = Vec::new();
        let result = run(
            Cursor::new(b"deposit,1,1,1.0\n".to_vec()),
            &mut out,
            nz(2),
            nz(4),
        )
        .await;
        assert!(matches!(
            result,
            Err(RunError::Pipeline(PipelineError::Read(_)))
        ));
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn an_oversize_row_is_an_error_and_nothing_is_written_even_after_valid_rows() {
        let input = format!(
            "{HEADER}deposit,1,1,1.0\n{}\ndeposit,2,2,1.0\n",
            "a".repeat(MAX_ROW_BYTES + 1)
        );
        let mut out = Vec::new();
        let result = run(Cursor::new(input.into_bytes()), &mut out, nz(2), nz(4)).await;
        assert!(matches!(
            result,
            Err(RunError::Pipeline(PipelineError::Read(_)))
        ));
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn a_failing_destination_is_reported() {
        let input = format!("{HEADER}deposit,1,1,1.0\n");
        let result = run(Cursor::new(input.into_bytes()), FailingWriter, nz(2), nz(4)).await;
        assert!(matches!(result, Err(RunError::Write(_))));
    }

    /// One of each kind of pipeline error. The match makes this fail to compile
    /// when a kind is added, as a reminder to list it.
    fn every_kind_of_pipeline_error() -> Vec<PipelineError> {
        let all = vec![
            PipelineError::Read(a_read_error()),
            PipelineError::WorkerPanicked,
            PipelineError::RouterPanicked,
            PipelineError::Stopped,
        ];
        for error in &all {
            match error {
                PipelineError::Read(_)
                | PipelineError::WorkerPanicked
                | PipelineError::RouterPanicked
                | PipelineError::Stopped => {}
            }
        }
        all
    }

    #[test]
    fn a_failed_pipeline_leaves_the_output_untouched() {
        for failure in every_kind_of_pipeline_error() {
            let message = failure.to_string();
            let mut out = Vec::new();
            let result = report(Err(failure), &mut out);
            let error = result.expect_err("a failed pipeline gives no report");
            assert!(out.is_empty(), "{message}: bytes were written");
            assert_eq!(error.to_string(), message);
            assert!(matches!(error, RunError::Pipeline(_)));
        }
    }

    #[test]
    fn run_errors_describe_themselves_and_expose_their_cause() {
        let read = RunError::Pipeline(PipelineError::Read(a_read_error()));
        assert!(read.to_string().contains("`type`"), "got: {read}");
        assert!(std::error::Error::source(&read).is_some());

        let panicked = RunError::Pipeline(PipelineError::WorkerPanicked);
        assert_eq!(panicked.to_string(), "a worker task panicked");
        assert!(std::error::Error::source(&panicked).is_some());

        let mut out = FailingWriter;
        let write = report(Ok(vec![(1, Account::default())]), &mut out).unwrap_err();
        assert!(write.to_string().starts_with("could not write the report"));
        assert!(std::error::Error::source(&write).is_some());
    }

    /// Rows for `clients` clients, each depositing, withdrawing and disputing,
    /// with the clients in descending order.
    fn many_clients(clients: u16) -> String {
        let mut input = String::from(HEADER);
        for (n, client) in (1..=clients).rev().enumerate() {
            let tx = (n as u32) * 3;
            input.push_str(&format!("deposit,{client},{},10.0\n", tx + 1));
            input.push_str(&format!(
                "withdrawal,{client},{},{}.5\n",
                tx + 2,
                client % 9
            ));
            input.push_str(&format!("dispute,{client},{}\n", tx + 1));
        }
        input
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_report_is_the_same_for_every_shard_count() {
        let input = many_clients(300);
        let one = run_with_shards(&input, 1).await.unwrap();
        assert_eq!(one.lines().count(), 301);
        for shards in [2, 3, 8, 64] {
            let report = run_with_shards(&input, shards).await.unwrap();
            assert_eq!(report, one, "{shards} shards");
        }
    }
}
