use anyhow::Error;
use std::backtrace::BacktraceStatus;
use std::fmt::{self, Write};

/// Render an `anyhow::Error` into a string that keeps the full cause chain
/// and, when available, the captured backtrace so that upstream callers can
/// surface precise diagnostics.
pub fn format_anyhow(err: &Error) -> String {
    let mut output = String::new();

    // `{:#}` prints the error plus its `Caused by` chain in a readable form.
    let _ = writeln!(&mut output, "{err:#}");

    match err.backtrace().status() {
        BacktraceStatus::Captured => {
            let _ = writeln!(&mut output, "\nBacktrace:\n{}", err.backtrace());
        }
        BacktraceStatus::Disabled => {
            let _ = writeln!(
                &mut output,
                "\nBacktrace capture disabled. Set RUST_LIB_BACKTRACE=1 to enable."
            );
        }
        BacktraceStatus::Unsupported => {
            // Some targets do not support backtraces; omit additional output.
        }
        _ => {}
    }

    output.trim_end().to_owned()
}

#[track_caller]
pub fn anyhow_with_site(args: fmt::Arguments<'_>) -> Error {
    let location = std::panic::Location::caller();
    let mut message = String::new();
    let _ = write!(&mut message, "{} [{}:{}]", args, location.file(), location.line());
    anyhow::anyhow!(message)
}

#[macro_export]
macro_rules! anyhow_site {
    ($fmt:expr $(, $arg:expr)*) => {{
        $crate::error::anyhow_with_site(format_args!($fmt $(, $arg)*))
    }};
}

#[macro_export]
macro_rules! bail_site {
    ($fmt:expr $(, $arg:expr)*) => {{
        return Err($crate::anyhow_site!($fmt $(, $arg)*));
    }};
}
