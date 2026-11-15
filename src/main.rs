#![forbid(unsafe_code)]

use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use payments_engine::{
    DEFAULT_CHANNEL_CAPACITY, DEFAULT_LOG_LEVEL, default_shards, parse_log_level, run,
};

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
        .block_on(run(
            file,
            io::stdout().lock(),
            shards,
            DEFAULT_CHANNEL_CAPACITY,
        ))
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
