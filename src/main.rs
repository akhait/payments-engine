#![forbid(unsafe_code)]

use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use tracing::level_filters::LevelFilter;

use payments_engine::{DEFAULT_CHANNEL_CAPACITY, default_shards, run};

/// The log level when `RUST_LOG` does not name one.
const DEFAULT_LOG_LEVEL: LevelFilter = LevelFilter::WARN;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(path), None) = (args.next(), args.next()) else {
        say("usage: payments_engine <transactions.csv>");
        return ExitCode::from(2);
    };

    init_logging();

    // Fatal errors go straight to stderr rather than through the log, so that
    // no RUST_LOG setting can hide them.
    match run_file(Path::new(&path)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            say(format_args!("error: {message}"));
            ExitCode::FAILURE
        }
    }
}

/// Prints a line to stderr and ignores a failure to do so. `eprintln!` would
/// panic on a closed stderr, replacing the exit code with 101.
fn say(message: impl fmt::Display) {
    let _ = writeln!(io::stderr(), "{message}");
}

fn run_file(path: &Path) -> Result<(), String> {
    // The path is shown with `{:?}`, which escapes control characters.
    let file = File::open(path).map_err(|error| format!("could not open {path:?}: {error}"))?;
    // The runtime is built by hand, so that `tokio-macros` stays out of the
    // release build, and its worker count is set explicitly: left unset, tokio reads
    // `TOKIO_WORKER_THREADS` and panics on a bad value, which would give exit
    // code 101. It is the same number as the shard count. `build` has no driver
    // to fail on here, so its error is a safeguard; if the operating system
    // refuses a thread, tokio panics instead (see D23).
    let shards = default_shards();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(shards.get())
        .build()
        .map_err(|error| format!("could not start the runtime: {error}"))?;
    runtime
        .block_on(run(file, io::stdout(), shards, DEFAULT_CHANNEL_CAPACITY))
        .map_err(|error| error.to_string())
}

fn init_logging() {
    let level = match std::env::var_os("RUST_LOG") {
        None => DEFAULT_LOG_LEVEL,
        Some(value) => match value.to_str() {
            Some(text) if text.trim().is_empty() => DEFAULT_LOG_LEVEL,
            Some(text) => parse_log_level(text).unwrap_or_else(|| {
                say(format_args!(
                    "ignoring RUST_LOG={text:?}: it must be one of off, error, warn, info, debug, trace"
                ));
                DEFAULT_LOG_LEVEL
            }),
            None => {
                say("ignoring RUST_LOG: it is not valid UTF-8");
                DEFAULT_LOG_LEVEL
            }
        },
    };
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .without_time()
        // By default tracing reports a failed log write with `eprintln!`, which
        // panics when stderr is closed.
        .log_internal_errors(false)
        .with_max_level(level)
        .init();
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
    use super::*;

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
}
