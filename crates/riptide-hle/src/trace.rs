//! One line in the session log for each console call worth knowing about.
//!
//! While a title is being brought up, what it asks for and in what order is the map of what
//! to build next. These lines go where [`set_trace`] says; until it is called they go
//! nowhere.

use std::sync::OnceLock;

use crate::stub::Log;

static TRACE: OnceLock<Log> = OnceLock::new();

/// Sends every trace line to `log` from now on. The first call wins.
pub fn set_trace(log: Log) {
    let _ = TRACE.set(log);
}

pub(crate) fn write(line: std::fmt::Arguments<'_>) {
    if let Some(log) = TRACE.get() {
        log(&line.to_string());
    }
}

/// Writes one trace line, formatted as `format!` does.
macro_rules! trace {
    ($($argument:tt)*) => {
        $crate::trace::write(format_args!($($argument)*))
    };
}

pub(crate) use trace;
