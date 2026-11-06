//! Writing to stdout without panicking when the reader has gone.
//!
//! `println!` panics when stdout is a pipe whose reader exited, as in
//! `wsblast ... | head -n 5`. wsblast ignores that case instead: the line is
//! dropped, the run carries on, and its exit code still reports the SLO
//! result. Other write errors are reported on stderr.

use std::fmt;
use std::io::{ErrorKind, Write};

/// Writes one line to stdout; see the module documentation.
pub(crate) fn print_line(args: fmt::Arguments<'_>) {
    let mut stdout = std::io::stdout().lock();
    if let Err(err) = writeln!(stdout, "{args}").and_then(|()| stdout.flush())
        && err.kind() != ErrorKind::BrokenPipe
    {
        eprintln!("wsblast: cannot write to stdout: {err}");
    }
}

/// `println!` that tolerates a closed stdout, through [`print_line`].
macro_rules! outln {
    ($($arg:tt)*) => {
        $crate::output::print_line(format_args!($($arg)*))
    };
}
pub(crate) use outln;

#[cfg(test)]
mod tests {
    #[test]
    fn printing_a_line_does_not_panic() {
        super::outln!("{} {}", "wsblast", 1);
    }
}
