//! Runs the real binary, the way a user would.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const HEADER: &str = "client,available,held,total,locked\n";

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

struct Outcome {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_with(args: &[&OsStr], rust_log: Option<&str>) -> Outcome {
    let mut command = Command::new(env!("CARGO_BIN_EXE_payments_engine"));
    command
        .args(args)
        .env_remove("RUST_LOG")
        .stdin(Stdio::null());
    if let Some(level) = rust_log {
        command.env("RUST_LOG", level);
    }
    let output = command.output().expect("the binary runs");
    Outcome {
        code: output.status.code(),
        stdout: in_client_order(&String::from_utf8(output.stdout).expect("stdout is UTF-8")),
        stderr: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    }
}

fn run_on(path: &Path) -> Outcome {
    run_with(&[path.as_os_str()], None)
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Writes `bytes` to a file of its own under the build directory.
fn input_file(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cli-tests");
    fs::create_dir_all(&dir).expect("a directory for test inputs");
    let path = dir.join(name);
    fs::write(&path, bytes).expect("the input file is written");
    path
}

/// Runs the binary with a stderr that nobody reads, so every write to it fails.
#[cfg(unix)]
fn run_with_unwritable_stderr(args: &[&OsStr]) -> (Option<i32>, String) {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    let (ours, theirs) = UnixStream::pair().expect("a socket pair");
    drop(ours);
    let output = Command::new(env!("CARGO_BIN_EXE_payments_engine"))
        .args(args)
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stderr(Stdio::from(OwnedFd::from(theirs)))
        .output()
        .expect("the binary runs");
    (
        output.status.code(),
        in_client_order(&String::from_utf8(output.stdout).expect("stdout is UTF-8")),
    )
}

/// The one log line that mentions `needle`.
fn line_with<'a>(logs: &'a str, needle: &str) -> &'a str {
    logs.lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no log line mentions {needle:?}: {logs}"))
}

#[test]
fn no_argument_prints_the_usage_and_exits_with_2() {
    let outcome = run_with(&[], None);
    assert_eq!(outcome.code, Some(2));
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("usage"),
        "stderr: {}",
        outcome.stderr
    );
}

#[test]
fn two_arguments_print_the_usage_and_exit_with_2() {
    let outcome = run_with(&[OsStr::new("a.csv"), OsStr::new("b.csv")], None);
    assert_eq!(outcome.code, Some(2));
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("usage"),
        "stderr: {}",
        outcome.stderr
    );
}

#[test]
fn a_missing_file_exits_with_1_and_prints_nothing_to_stdout() {
    let missing = input_file("present.csv", b"").with_file_name("absent.csv");
    let outcome = run_on(&missing);
    assert_eq!(outcome.code, Some(1));
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("could not open"),
        "stderr: {}",
        outcome.stderr
    );
}

#[test]
fn reports_the_accounts_of_a_fixture_covering_every_transaction_type() {
    let outcome = run_on(&fixture("basic.csv"));
    assert_eq!(outcome.code, Some(0));
    let mut lines = outcome.stdout.lines();
    assert_eq!(lines.next(), Some("client,available,held,total,locked"));
    let mut rows: Vec<&str> = lines.collect();
    rows.sort();
    assert_eq!(
        rows,
        [
            "1,5.0000,0.0000,5.0000,false",
            "2,7.2500,0.0000,7.2500,false",
            "3,-2.5000,0.0000,-2.5000,true",
        ]
    );
}

#[test]
fn the_report_holds_nothing_but_csv() {
    let outcome = run_on(&fixture("basic.csv"));
    assert_eq!(
        outcome.stdout,
        format!(
            "{HEADER}1,5.0000,0.0000,5.0000,false\n2,7.2500,0.0000,7.2500,false\n3,-2.5000,0.0000,-2.5000,true\n"
        )
    );
}

#[test]
fn logs_each_refused_transaction_with_its_client_tx_and_reason() {
    let outcome = run_on(&fixture("basic.csv"));
    let refused_withdrawal = line_with(&outcome.stderr, "tx=6");
    assert!(
        refused_withdrawal.contains("client=1"),
        "{refused_withdrawal}"
    );
    assert!(
        refused_withdrawal.contains("insufficient available funds"),
        "{refused_withdrawal}"
    );
    let refused_deposit = line_with(&outcome.stderr, "tx=7");
    assert!(refused_deposit.contains("client=3"), "{refused_deposit}");
    assert!(
        refused_deposit.contains("account is locked"),
        "{refused_deposit}"
    );
    assert!(
        line_with(&outcome.stderr, "rejected=2").contains("transactions rejected in total"),
        "stderr: {}",
        outcome.stderr
    );
}

#[test]
fn a_run_without_problems_logs_nothing() {
    let input = input_file("clean.csv", b"type,client,tx,amount\ndeposit,1,1,1.0\n");
    let outcome = run_on(&input);
    assert_eq!(outcome.code, Some(0));
    assert_eq!(outcome.stderr, "");
}

#[test]
fn skipped_rows_are_logged_by_data_row_number_for_lf_and_crlf_input() {
    let lf = fs::read(fixture("messy.csv")).expect("the fixture exists");
    let crlf = String::from_utf8(lf.clone())
        .expect("the fixture is UTF-8")
        .replace('\n', "\r\n")
        .into_bytes();
    for (name, bytes) in [("messy-lf.csv", lf), ("messy-crlf.csv", crlf)] {
        let outcome = run_on(&input_file(name, &bytes));
        assert_eq!(outcome.code, Some(0), "{name}");
        assert_eq!(
            outcome.stdout,
            format!("{HEADER}1,-2.0000,5.0000,3.0000,false\n"),
            "{name}"
        );
        let skipped: Vec<&str> = outcome
            .stderr
            .lines()
            .filter(|line| line.contains("skipping row"))
            .collect();
        assert_eq!(skipped.len(), 3, "{name}: {}", outcome.stderr);
        for (line, row) in skipped.iter().zip([2, 3, 5]) {
            assert!(line.contains(&format!("row={row} ")), "{name}: {line}");
        }
        assert!(outcome.stderr.contains("skipped=3"), "{name}");
        assert!(!outcome.stderr.contains("SECRET"), "{name}");
    }
}

#[test]
fn an_oversize_row_exits_with_1_and_prints_nothing_to_stdout() {
    let mut bytes = b"type,client,tx,amount\ndeposit,1,1,1.0\n".to_vec();
    bytes.extend(std::iter::repeat_n(b'a', 5_000));
    bytes.extend_from_slice(b"\ndeposit,2,2,1.0\n");
    let outcome = run_on(&input_file("oversize.csv", &bytes));
    assert_eq!(outcome.code, Some(1));
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("longer than the allowed maximum"),
        "stderr: {}",
        outcome.stderr
    );
}

/// Rows for many clients, listed from the highest id down, with all the deposits
/// before all the withdrawals so that the rows of one client are far apart.
/// Every tenth client also has its deposit disputed.
fn many_clients_input(clients: u32) -> Vec<u8> {
    let mut text = String::from("type,client,tx,amount\n");
    for client in (1..=clients).rev() {
        text.push_str(&format!("deposit,{client},{},{client}.0\n", 2 * client));
    }
    for client in (1..=clients).rev() {
        text.push_str(&format!("withdrawal,{client},{},0.5\n", 2 * client + 1));
        if client % 10 == 0 {
            text.push_str(&format!("dispute,{client},{}\n", 2 * client));
        }
    }
    text.into_bytes()
}

#[test]
fn a_large_input_over_many_clients_gives_the_exact_report() {
    const CLIENTS: u32 = 2_000;
    let path = input_file("many-clients.csv", &many_clients_input(CLIENTS));

    let mut expected = String::from(HEADER);
    for client in 1..=CLIENTS {
        let total = f64::from(client) - 0.5;
        if client % 10 == 0 {
            // The deposit is held, so what is left of the withdrawal makes
            // `available` negative.
            expected.push_str(&format!(
                "{client},-0.5000,{client}.0000,{total:.4},false\n"
            ));
        } else {
            expected.push_str(&format!("{client},{total:.4},0.0000,{total:.4},false\n"));
        }
    }

    let first = run_on(&path);
    assert_eq!(first.code, Some(0));
    assert_eq!(first.stderr, "");
    assert_eq!(first.stdout, expected);
    // The order in which each engine lists its accounts (a hash map) differs from
    // run to run, and the rows are compared in client order.
    assert_eq!(run_on(&path).stdout, expected);
}

#[test]
fn an_error_after_many_valid_rows_still_prints_nothing_to_stdout() {
    let mut bytes = many_clients_input(2_000);
    bytes.extend(std::iter::repeat_n(b'a', 5_000));
    bytes.extend_from_slice(b"\ndeposit,1,999999,1.0\n");
    let outcome = run_on(&input_file("many-then-oversize.csv", &bytes));
    assert_eq!(outcome.code, Some(1));
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("longer than the allowed maximum"),
        "stderr: {}",
        outcome.stderr
    );
}

#[test]
fn a_header_without_the_required_columns_exits_with_1_and_prints_nothing_to_stdout() {
    let outcome = run_on(&input_file("no-header.csv", b"deposit,1,1,1.0\n"));
    assert_eq!(outcome.code, Some(1));
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("`type`"),
        "stderr: {}",
        outcome.stderr
    );
}

#[test]
fn empty_and_header_only_files_give_a_report_with_no_accounts() {
    for (name, bytes) in [
        ("empty.csv", &b""[..]),
        ("header-only.csv", &b"type,client,tx,amount\n"[..]),
    ] {
        let outcome = run_on(&input_file(name, bytes));
        assert_eq!(outcome.code, Some(0), "{name}");
        assert_eq!(outcome.stdout, HEADER, "{name}");
    }
}

#[test]
fn logs_are_plain_text_without_colour_codes() {
    let outcome = run_on(&fixture("basic.csv"));
    assert!(!outcome.stderr.is_empty());
    assert!(
        !outcome.stderr.contains('\u{1b}'),
        "stderr: {:?}",
        outcome.stderr
    );
}

#[test]
fn refused_transactions_are_logged_one_by_one_only_up_to_a_cap() {
    let mut input = String::from("type,client,tx,amount\n");
    for tx in 1..=30 {
        input.push_str(&format!("withdrawal,1,{tx},1.0\n"));
    }
    let outcome = run_on(&input_file("many-refusals.csv", input.as_bytes()));
    assert_eq!(outcome.code, Some(0));
    assert_eq!(
        outcome.stdout,
        format!("{HEADER}1,0.0000,0.0000,0.0000,false\n")
    );
    let individual = outcome
        .stderr
        .lines()
        .filter(|line| line.contains("transaction rejected"))
        .count();
    assert_eq!(individual, 20, "stderr: {}", outcome.stderr);
    let summary = line_with(&outcome.stderr, "transactions rejected in total");
    assert!(summary.contains("rejected=30"), "{summary}");
}

#[test]
fn refusals_of_many_clients_are_logged_up_to_one_cap_for_the_whole_run() {
    // Thirty clients, so that the refusals come from different workers whenever
    // the machine has more than one core.
    let mut input = String::from("type,client,tx,amount\n");
    for client in 1..=30 {
        input.push_str(&format!("withdrawal,{client},{client},1.0\n"));
    }
    let outcome = run_on(&input_file(
        "refusals-of-many-clients.csv",
        input.as_bytes(),
    ));
    assert_eq!(outcome.code, Some(0));
    let individual = outcome
        .stderr
        .lines()
        .filter(|line| line.contains("transaction rejected"))
        .count();
    assert_eq!(individual, 20, "stderr: {}", outcome.stderr);
    let summary = line_with(&outcome.stderr, "transactions rejected in total");
    assert!(summary.contains("rejected=30"), "{summary}");
}

/// Runs the binary on the basic fixture with `TOKIO_WORKER_THREADS` set.
fn run_basic_with_worker_threads(value: &OsStr) -> Outcome {
    let output = Command::new(env!("CARGO_BIN_EXE_payments_engine"))
        .arg(fixture("basic.csv"))
        .env_remove("RUST_LOG")
        .env("TOKIO_WORKER_THREADS", value)
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs");
    Outcome {
        code: output.status.code(),
        stdout: in_client_order(&String::from_utf8(output.stdout).expect("stdout is UTF-8")),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn a_bad_tokio_worker_threads_value_does_not_crash_the_run() {
    // Tokio reads this variable unless the worker count is set explicitly, and
    // it panics on a value it cannot use, which would give exit code 101.
    let expected = run_on(&fixture("basic.csv"));
    assert_eq!(expected.code, Some(0));
    for value in ["", "0", "-1", "abc", "99999999999999999999"] {
        let outcome = run_basic_with_worker_threads(OsStr::new(value));
        assert_eq!(outcome.code, Some(0), "value {value:?}: {}", outcome.stderr);
        assert_eq!(outcome.stdout, expected.stdout, "value {value:?}");
    }
}

#[cfg(unix)]
#[test]
fn a_tokio_worker_threads_value_that_is_not_utf8_does_not_crash_the_run() {
    use std::os::unix::ffi::OsStrExt;

    let outcome = run_basic_with_worker_threads(OsStr::from_bytes(b"\xff\xfe"));
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, run_on(&fixture("basic.csv")).stdout);
}

#[test]
fn the_example_in_the_readme_is_a_fixture_and_gives_the_report_shown_there() {
    let report = format!("{HEADER}3,3.5000,0.0000,3.5000,false\n7,-1.2500,5.0000,3.7500,false\n");
    let outcome = run_on(&fixture("example.csv"));
    assert_eq!(outcome.code, Some(0));
    assert_eq!(outcome.stdout, report);
    let refusal = line_with(&outcome.stderr, "transaction rejected");
    assert!(refusal.contains("client=3 tx=14 "), "{refusal}");

    // The README shows the same input and the same report, so that they cannot
    // drift apart from what the binary does.
    let readme = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))
        .expect("the README is readable");
    let input = fs::read_to_string(fixture("example.csv")).expect("the fixture is readable");
    assert!(
        readme.contains(&input),
        "the README does not show the fixture"
    );
    assert!(
        readme.contains(&report),
        "the README does not show the report"
    );
}

#[test]
fn an_empty_rust_log_counts_as_unset() {
    let outcome = run_with(&[fixture("basic.csv").as_os_str()], Some(""));
    assert_eq!(outcome.code, Some(0));
    assert!(
        !outcome.stderr.contains("RUST_LOG"),
        "stderr: {}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("transaction rejected"),
        "stderr: {}",
        outcome.stderr
    );
}

#[cfg(unix)]
#[test]
fn an_unwritable_stderr_does_not_turn_a_usage_error_into_a_crash() {
    let (code, stdout) = run_with_unwritable_stderr(&[]);
    assert_eq!(code, Some(2));
    assert_eq!(stdout, "");
}

#[cfg(unix)]
#[test]
fn an_unwritable_stderr_does_not_turn_a_missing_file_into_a_crash() {
    let missing = input_file("present3.csv", b"").with_file_name("absent3.csv");
    let (code, stdout) = run_with_unwritable_stderr(&[missing.as_os_str()]);
    assert_eq!(code, Some(1));
    assert_eq!(stdout, "");
}

#[cfg(unix)]
#[test]
fn warnings_that_cannot_be_written_do_not_stop_the_report() {
    let (code, stdout) = run_with_unwritable_stderr(&[fixture("basic.csv").as_os_str()]);
    assert_eq!(code, Some(0));
    assert_eq!(
        stdout,
        format!(
            "{HEADER}1,5.0000,0.0000,5.0000,false\n2,7.2500,0.0000,7.2500,false\n3,-2.5000,0.0000,-2.5000,true\n"
        )
    );
}

#[test]
fn the_refusal_summary_is_logged_even_when_the_run_ends_with_an_error() {
    let mut bytes = b"type,client,tx,amount\n".to_vec();
    for tx in 1..=30 {
        bytes.extend(format!("withdrawal,1,{tx},1.0\n").bytes());
    }
    bytes.extend(std::iter::repeat_n(b'a', 2_000));
    bytes.push(b'\n');
    let outcome = run_on(&input_file("refusals-then-oversize.csv", &bytes));
    assert_eq!(outcome.code, Some(1));
    assert_eq!(outcome.stdout, "");
    let summary = line_with(&outcome.stderr, "transactions rejected in total");
    assert!(summary.contains("rejected=30"), "{summary}");
}

#[test]
fn every_kind_of_refusal_is_logged_with_its_client_tx_and_reason() {
    use payments_engine::engine::Rejection;

    let input = "type,client,tx,amount\n\
        deposit,1,1,5.0\n\
        dispute,1,99\n\
        deposit,2,1,5.0\n\
        dispute,2,1\n\
        dispute,2,1\n\
        deposit,3,1,5.0\n\
        resolve,3,1\n\
        deposit,4,1,5.0\n\
        deposit,4,1,5.0\n\
        deposit,5,1,0\n\
        deposit,6,1,922337203685477.5807\n\
        deposit,6,2,0.0001\n\
        withdrawal,7,1,1.0\n\
        deposit,8,1,5.0\n\
        dispute,8,1\n\
        chargeback,8,1\n\
        deposit,8,2,1.0\n";
    let outcome = run_on(&input_file("every-refusal.csv", input.as_bytes()));
    assert_eq!(outcome.code, Some(0));
    let expected = [
        (1, 99, Rejection::UnknownTransaction),
        (2, 1, Rejection::NotDisputable),
        (3, 1, Rejection::NotUnderDispute),
        (4, 1, Rejection::DuplicateTransaction),
        (5, 1, Rejection::NonPositiveAmount),
        (6, 2, Rejection::Overflow),
        (7, 1, Rejection::InsufficientFunds),
        (8, 2, Rejection::AccountLocked),
    ];
    for (client, tx, reason) in expected {
        let line = line_with(&outcome.stderr, &format!("client={client} tx={tx} "));
        assert!(line.contains(&reason.to_string()), "{reason:?}: {line}");
    }
    let summary = line_with(&outcome.stderr, "transactions rejected in total");
    assert!(summary.contains("rejected=8"), "{summary}");
}

#[test]
fn the_first_refusals_are_the_ones_logged_and_a_single_refusal_still_gets_a_summary() {
    let mut input = String::from("type,client,tx,amount\n");
    for tx in 1..=30 {
        input.push_str(&format!("withdrawal,1,{tx},1.0\n"));
    }
    let outcome = run_on(&input_file("which-refusals.csv", input.as_bytes()));
    assert!(
        outcome.stderr.contains("tx=20 "),
        "stderr: {}",
        outcome.stderr
    );
    assert!(
        !outcome.stderr.contains("tx=21 "),
        "stderr: {}",
        outcome.stderr
    );

    let single = input_file(
        "one-refusal.csv",
        b"type,client,tx,amount\nwithdrawal,1,1,1.0\n",
    );
    let outcome = run_on(&single);
    let summary = line_with(&outcome.stderr, "transactions rejected in total");
    assert!(summary.contains("rejected=1"), "{summary}");
}

#[test]
fn warnings_are_plain_lines_that_start_with_their_level() {
    let outcome = run_on(&fixture("basic.csv"));
    assert!(!outcome.stderr.is_empty());
    for line in outcome.stderr.lines() {
        assert!(
            line.trim_start().starts_with("WARN "),
            "unexpected line: {line}"
        );
    }
}

#[test]
fn the_usage_line_names_the_argument() {
    let outcome = run_with(&[], None);
    assert!(
        outcome
            .stderr
            .contains("usage: payments_engine <transactions.csv>"),
        "stderr: {}",
        outcome.stderr
    );
}

#[test]
fn the_open_error_names_the_file_and_the_reason() {
    let missing = input_file("present4.csv", b"").with_file_name("absent-file-name.csv");
    let outcome = run_on(&missing);
    assert!(
        outcome.stderr.starts_with("error: could not open "),
        "stderr: {}",
        outcome.stderr
    );
    assert!(outcome.stderr.contains("absent-file-name.csv"));
    assert!(outcome.stderr.contains("No such file or directory"));
}

#[test]
fn a_directory_exits_with_1_and_prints_nothing_to_stdout() {
    let outcome = run_on(Path::new(env!("CARGO_TARGET_TMPDIR")));
    assert_eq!(outcome.code, Some(1));
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.starts_with("error: "),
        "stderr: {}",
        outcome.stderr
    );
}

#[test]
fn a_path_with_control_characters_is_shown_escaped() {
    let outcome = run_with(&[OsStr::new("x\ny\u{1b}[2Jz")], None);
    assert_eq!(outcome.code, Some(1));
    assert_eq!(
        outcome.stderr.lines().count(),
        1,
        "stderr: {:?}",
        outcome.stderr
    );
    assert!(
        !outcome.stderr.contains('\u{1b}'),
        "stderr: {:?}",
        outcome.stderr
    );
}

#[cfg(unix)]
#[test]
fn a_path_that_is_not_valid_utf8_is_handled() {
    use std::os::unix::ffi::OsStrExt;

    let outcome = run_with(&[OsStr::from_bytes(b"missing-\xff.csv")], None);
    assert_eq!(outcome.code, Some(1));
    assert!(
        outcome.stderr.starts_with("error: could not open"),
        "stderr: {}",
        outcome.stderr
    );
}

#[test]
fn rust_log_warn_keeps_the_warnings() {
    let outcome = run_with(&[fixture("basic.csv").as_os_str()], Some("warn"));
    assert_eq!(outcome.code, Some(0));
    assert!(
        outcome.stderr.contains("transaction rejected"),
        "stderr: {}",
        outcome.stderr
    );
}

#[test]
fn rust_log_error_hides_the_warnings() {
    let outcome = run_with(&[fixture("basic.csv").as_os_str()], Some("error"));
    assert_eq!(outcome.code, Some(0));
    assert_eq!(outcome.stderr, "");
}

#[test]
fn a_blank_rust_log_counts_as_unset() {
    let outcome = run_with(&[fixture("basic.csv").as_os_str()], Some("   "));
    assert!(
        !outcome.stderr.contains("RUST_LOG"),
        "stderr: {}",
        outcome.stderr
    );
    assert!(outcome.stderr.contains("transaction rejected"));
}

#[test]
fn level_numbers_are_not_level_names() {
    for value in ["0", "1", "3", "+3", "9"] {
        let outcome = run_with(&[fixture("basic.csv").as_os_str()], Some(value));
        assert_eq!(outcome.code, Some(0), "RUST_LOG={value}");
        assert!(
            outcome.stderr.contains("ignoring RUST_LOG"),
            "RUST_LOG={value}: {}",
            outcome.stderr
        );
        assert!(
            outcome.stderr.contains("transaction rejected"),
            "RUST_LOG={value}: {}",
            outcome.stderr
        );
    }
}

#[cfg(unix)]
#[test]
fn a_rust_log_that_is_not_valid_utf8_is_reported() {
    use std::os::unix::ffi::OsStrExt;

    let output = Command::new(env!("CARGO_BIN_EXE_payments_engine"))
        .arg(fixture("basic.csv"))
        .env("RUST_LOG", OsStr::from_bytes(b"\xff\xfe"))
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("RUST_LOG"), "stderr: {stderr}");
    assert!(stderr.contains("transaction rejected"), "stderr: {stderr}");
}

#[test]
fn rust_log_can_silence_warnings() {
    let outcome = run_with(&[fixture("basic.csv").as_os_str()], Some("off"));
    assert_eq!(outcome.code, Some(0));
    assert_eq!(outcome.stderr, "");
}

#[test]
fn rust_log_cannot_silence_a_fatal_error() {
    let outcome = run_with(
        &[input_file("no-header-quiet.csv", b"deposit,1,1,1.0\n").as_os_str()],
        Some("off"),
    );
    assert_eq!(outcome.code, Some(1));
    assert!(
        outcome.stderr.contains("`type`"),
        "stderr: {}",
        outcome.stderr
    );
}

#[test]
fn an_unusable_rust_log_is_reported_and_warnings_stay_on() {
    let outcome = run_with(&[fixture("basic.csv").as_os_str()], Some("bogus"));
    assert_eq!(outcome.code, Some(0));
    assert!(
        outcome.stderr.contains("RUST_LOG"),
        "stderr: {}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("transaction rejected"),
        "stderr: {}",
        outcome.stderr
    );
}
