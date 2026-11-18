//! Lines to standard output and standard error that never panic (spec
//! `crash-resilience.md`, "Standard output and error").
//!
//! `println!` and `eprintln!` panic when the write fails, and it fails when the other
//! end of a pipe went away: a parent that closed its pipe, a terminal that closed, a
//! `| head` that read enough. A desktop app or a command-line program must not end
//! for that, so the composition roots write through [`outln!`](crate::outln) and
//! [`errln!`](crate::errln), which drop the line instead. Clippy's
//! `disallowed-macros` in `rbs-app` and `rbs-cli` keeps the std macros out.

use std::fmt;
use std::io::{self, Write};

/// Writes `args` and a line end to standard output; a failed write is dropped.
pub fn out_line(args: fmt::Arguments<'_>) {
    write_line(&mut io::stdout().lock(), args);
}

/// Writes `args` and a line end to standard error; a failed write is dropped.
pub fn err_line(args: fmt::Arguments<'_>) {
    write_line(&mut io::stderr().lock(), args);
}

/// Writes `args` and a line end to `w`, dropping any error.
pub fn write_line(w: &mut impl Write, args: fmt::Arguments<'_>) {
    // A line that cannot be written has nowhere else to go.
    let _ = w.write_fmt(args).and_then(|()| w.write_all(b"\n"));
}

/// `println!` that drops the line when standard output is gone instead of panicking.
#[macro_export]
macro_rules! outln {
    () => {
        $crate::out::out_line(::std::format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::out::out_line(::std::format_args!($($arg)*))
    };
}

/// `eprintln!` that drops the line when standard error is gone instead of panicking.
#[macro_export]
macro_rules! errln {
    () => {
        $crate::out::err_line(::std::format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::out::err_line(::std::format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pipe whose reader went away: every write fails.
    struct Closed;

    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }

    #[test]
    fn a_failed_write_is_dropped() {
        write_line(&mut Closed, format_args!("rbs: {}", 1));
    }

    #[test]
    fn a_line_ends_with_a_line_end() {
        let mut buf = Vec::new();
        write_line(&mut buf, format_args!("a {}", "b"));
        write_line(&mut buf, format_args!(""));
        assert_eq!(buf, b"a b\n\n");
    }
}
