//! The process's stderr lines. `eprintln!` panics when the write fails, and
//! a release build aborts on panic; [`log!`](crate::log!) drops the line, or
//! what is left of it after a short write.

use std::io::Write as _;

/// One line to stderr in a single write. A failed write is ignored.
pub fn write_line(args: std::fmt::Arguments<'_>) {
    let mut line = args.to_string();
    line.push('\n');
    let _ = std::io::stderr().write_all(line.as_bytes());
}

/// `eprintln!` that never panics.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::log::write_line(format_args!($($arg)*))
    };
}
