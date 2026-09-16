//! Prints the disassembly of a bytecode dump, in LuaJIT's `-bl` format.
//!
//! The output is byte for byte what `luajit -bl` writes, which is what makes it
//! useful for checking a dump against the compiler that produced it.
//!
//! Usage: `cargo run --example listing -- <file.ljbc>`

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: listing <file.ljbc>");
        return ExitCode::FAILURE;
    };

    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("cannot read {}: {error}", path.to_string_lossy());
            return ExitCode::FAILURE;
        }
    };

    let alloc = oxc_allocator::Allocator::default();
    match luajit_ripper::bytecode::parse(&alloc, &data) {
        Ok(chunk) => {
            print!("{}", luajit_ripper::listing::dump(&chunk));
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{}: {error}", path.to_string_lossy());
            ExitCode::FAILURE
        }
    }
}
