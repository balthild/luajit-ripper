//! Decompiles a raw LuaJIT bytecode dump and writes the Lua source to stdout.
//!
//! ```text
//! cargo run --example decompile_file -- path/to/file.ljbc [options]
//! ```
//!
//! The options are `--spaces`, `--slots`, `--syntactic-sugar`, `--bit-library`
//! and `--mark-errors`; they may come before or after the path.

use std::process::ExitCode;

use luajit_ripper::{Options, decompile};

fn main() -> ExitCode {
    let mut path: Option<String> = None;
    let mut options = Options::default();

    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--spaces" => options.indent = luajit_ripper::Indent::Spaces(4),
            "--slots" => options.show_slot_ids = true,
            "--syntactic-sugar" => options.function_definition_sugar = true,
            "--bit-library" => options.bitop_style = luajit_ripper::BitOpStyle::BitLibrary,
            "--mark-errors" => options.on_function_error = luajit_ripper::OnFunctionError::Mark,
            other if other.starts_with("--") => {
                eprintln!("unknown option {other}");
                return ExitCode::FAILURE;
            }
            other => {
                if path.is_some() {
                    eprintln!("more than one dump was given");
                    return ExitCode::FAILURE;
                }
                path = Some(other.to_string());
            }
        }
    }

    let Some(path) = path else {
        eprintln!(
            "usage: decompile_file <dump.ljbc> [--spaces] [--slots] \
             [--syntactic-sugar] [--bit-library] [--mark-errors]"
        );
        return ExitCode::FAILURE;
    };

    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("cannot read {path}: {error}");
            return ExitCode::FAILURE;
        }
    };

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
