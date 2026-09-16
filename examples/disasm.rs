//! Prints the disassembly of a raw LuaJIT bytecode dump.
//!
//! ```text
//! cargo run --example disasm -- path/to/file.ljbc
//! ```

use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: disasm <dump.ljbc>");
        return ExitCode::FAILURE;
    };

    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("cannot read {path}: {error}");
            return ExitCode::FAILURE;
        }
    };

    let chunk = match luajit_ripper::bytecode::parse(&data) {
        Ok(chunk) => chunk,
        Err(error) => {
            eprintln!("{path}: {error}");
            return ExitCode::FAILURE;
        }
    };

    print!("{}", luajit_ripper::listing::dump(&chunk));

    ExitCode::SUCCESS
}
