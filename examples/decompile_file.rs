//! Decompiles a raw LuaJIT bytecode dump and writes the Lua source to stdout.
//!
//! ```text
//! cargo run --example decompile_file -- path/to/file.ljbc
//! ```

use std::process::ExitCode;

use luajit_ripper::{Options, decompile};

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let Some(path) = arguments.next() else {
        eprintln!("usage: decompile_file <dump.ljbc>");
        return ExitCode::FAILURE;
    };

    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("cannot read {path}: {error}");
            return ExitCode::FAILURE;
        }
    };

    let mut options = Options::default();
    for argument in arguments {
        match argument.as_str() {
            "--spaces" => options.indent = luajit_ripper::Indent::Spaces(4),
            "--slots" => options.show_slot_ids = true,
            "--syntactic-sugar" => options.function_definition_sugar = true,
            "--bit-library" => options.bitop_style = luajit_ripper::BitOpStyle::BitLibrary,
            "--mark-errors" => options.on_function_error = luajit_ripper::OnFunctionError::Mark,
            other => {
                eprintln!("unknown option {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    match decompile(&data, &options) {
        Ok(source) => {
            print!("{source}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{path}: {error}");
            ExitCode::FAILURE
        }
    }
}
