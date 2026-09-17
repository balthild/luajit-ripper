//! The live progress line of a run over a directory.
//!
//! A run that has thousands of dumps to get through says nothing for minutes if
//! it only reports at the end, so one line is written per dump as it is done.
//! The lines are written by the calling thread alone, which is what keeps them
//! from being interleaved: the workers only send messages.
//!
//! When the sink is a terminal, each new line takes the place of the one before
//! it: the cursor is moved to the start of the previous line and everything it
//! passed over is erased. What is left on screen is the single line of the dump
//! that is being worked on, rather than a line per dump scrolling by. The full
//! account is in the report that follows.
//!
//! A sink that is not a terminal — a pipe, a file — gets the same lines without
//! the escapes, one per dump, which is what a reader of a log wants.

use std::io::{self, IsTerminal, Write};

/// Moves the cursor to the start of the previous line and clears the way.
///
/// * `ESC [ 1 A` — up one line;
/// * `ESC [ 1 G` — to the first column of that line;
/// * `ESC [ 0 J` — erase from the cursor to the end of the screen.
const REWRITE: &str = "\x1b[1A\x1b[1G\x1b[0J";

/// Writes one line per dump, in place when the sink is a terminal.
pub struct Progress<W: Write> {
    /// Where the lines go.
    out: W,
    /// How many dumps the run has in total.
    total: usize,
    /// How many of them are done.
    done: usize,
    /// Whether a line that is still on screen may be rewritten.
    in_place: bool,
    /// Whether the last thing written was such a line.
    live: bool,
}

impl Progress<io::Stderr> {
    /// A progress line on stderr, for `total` dumps.
    ///
    /// Rewriting the line is only attempted when stderr is a terminal: escapes
    /// written into a pipe or a file would be read as text. `TERM=dumb` is a
    /// terminal that cannot be moved around either, so it is left alone as well.
    pub fn stderr(total: usize) -> Progress<io::Stderr> {
        let in_place = io::stderr().is_terminal()
            && std::env::var_os("TERM").is_none_or(|term| term != "dumb");
        Progress::new(io::stderr(), in_place, total)
    }
}

impl<W: Write> Progress<W> {
    /// Writes the progress of `total` dumps to `out`.
    ///
    /// `in_place` asks for each line to take the place of the one before it; a
    /// sink that cannot show that, such as a test's buffer, is given `false`.
    pub fn new(out: W, in_place: bool, total: usize) -> Progress<W> {
        Progress {
            out,
            total,
            done: 0,
            in_place,
            live: false,
        }
    }

    /// Reports `name` as the dump that has just been finished.
    pub fn step(&mut self, name: &str) -> io::Result<()> {
        self.done += 1;
        self.begin_line()?;
        writeln!(self.out, "[{}/{}] {name}", self.done, self.total)?;
        self.live = true;
        self.out.flush()
    }

    /// Ends the progress, clearing the line that is on screen.
    ///
    /// Whatever is written next starts on a clean line, so a report does not
    /// share a line with the progress of the last dump.
    pub fn finish(&mut self) -> io::Result<()> {
        if self.live && self.in_place {
            self.out.write_all(REWRITE.as_bytes())?;
            self.live = false;
        }
        self.out.flush()
    }

    /// Makes room for the next line.
    ///
    /// The line of the previous dump is erased rather than scrolled past. The
    /// very first line, and every line of a sink that cannot be moved around, is
    /// written where the cursor already is.
    fn begin_line(&mut self) -> io::Result<()> {
        if self.live && self.in_place {
            self.out.write_all(REWRITE.as_bytes())?;
        }
        self.live = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collects what a progress writes, with the escapes visible.
    fn lines(chunks: Vec<&str>, in_place: bool) -> String {
        let mut progress = Progress::new(Vec::new(), in_place, chunks.len());
        for chunk in chunks {
            progress
                .step(chunk)
                .expect("writing to a buffer cannot fail");
        }
        progress.finish().expect("writing to a buffer cannot fail");
        String::from_utf8(progress.out).expect("the buffer holds the escapes and names")
    }

    #[test]
    fn every_dump_gets_a_line() {
        assert_eq!(
            lines(vec!["a.lua", "b.lua", "c.lua"], false),
            "[1/3] a.lua\n[2/3] b.lua\n[3/3] c.lua\n"
        );
    }

    #[test]
    fn a_live_line_is_rewritten_where_it_stands() {
        // The first line is written as it is; every line after it walks back up
        // over the one before. `finish` leaves the line empty and the cursor at
        // its start, so the report that follows is not appended to it.
        assert_eq!(
            lines(vec!["a.lua", "b.lua"], true),
            format!("[1/2] a.lua\n{REWRITE}[2/2] b.lua\n{REWRITE}")
        );
    }

    #[test]
    fn nothing_is_written_for_an_empty_run() {
        assert_eq!(lines(Vec::new(), true), "");
    }
}
