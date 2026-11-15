//! Helpers shared by the unit tests of several modules.

use std::cell::RefCell;
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::sync::Once;

use crate::pipeline::{ReadError, read_transactions};

/// A destination that always fails, like a full disk.
pub(crate) struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("disk full"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("disk full"))
    }
}

/// A shard count or channel size from a plain number.
pub(crate) fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("a non-zero number")
}

/// A real read error, obtained from input that has no header.
pub(crate) fn a_read_error() -> ReadError {
    let Err(error) = read_transactions("deposit,1,1,1.0\n".as_bytes()) else {
        panic!("input without a header should fail");
    };
    error
}

thread_local! {
    /// What the current thread has logged since `capture_logs` last cleared it.
    static LOGGED: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Sends log output to the buffer of the thread that produced it.
struct ThreadWriter;

impl Write for ThreadWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        LOGGED.with(|logged| logged.borrow_mut().extend_from_slice(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Runs `run` and returns its result together with what the calling thread
/// logged at warn level or above while it ran.
///
/// One subscriber is installed for the whole test process and each thread
/// gets its own buffer. A subscriber scoped to a single thread is not
/// enough: tracing remembers that nobody is listening at a log call site
/// the first time some other test thread reaches it, and a scoped
/// subscriber then sometimes misses events, which made these tests flaky.
/// What other threads log is not captured.
pub(crate) fn capture_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| ThreadWriter)
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other global subscriber in the tests");
    });
    LOGGED.with(|logged| logged.borrow_mut().clear());
    let result = run();
    let bytes = LOGGED.with(|logged| std::mem::take(&mut *logged.borrow_mut()));
    (result, String::from_utf8(bytes).expect("logs are UTF-8"))
}
