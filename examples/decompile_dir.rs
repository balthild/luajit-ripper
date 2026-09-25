//! Decompiles every dump in a directory.
//!
//! ```text
//! cargo run --release --example decompile_dir -- <input dir> <output dir>
//! ```
//!
//! Files that cannot be decompiled are reported on stderr and skipped; with
//! `--mark-errors` the functions that fail are left with a call to `error`
//! instead, so the output is still complete.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

use luajit_ripper::path::PathExt;
use luajit_ripper::{OnFunctionError, Options, decompile};

fn main() -> ExitCode {
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    let mut mark_errors = false;
    let mut sugar = false;
    arguments.retain(|argument| {
        match argument.as_str() {
            "--mark-errors" => mark_errors = true,
            "--syntactic-sugar" => sugar = true,
            _ => return true,
        }
        false
    });

    if arguments.len() != 2 {
        eprintln!(
            "usage: decompile_dir [--mark-errors] [--syntactic-sugar] <input dir> <output dir>"
        );
        return ExitCode::FAILURE;
    }

    let input = PathBuf::from(&arguments[0]);
    let output = PathBuf::from(&arguments[1]);
    let options = Options {
        on_function_error: if mark_errors {
            OnFunctionError::Mark
        } else {
            OnFunctionError::Fail
        },
        function_definition_sugar: sugar,
        ..Default::default()
    };

    let mut files: Vec<PathBuf> = match std::fs::read_dir(&input) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.has_extension("ljbc"))
            .collect(),
        Err(error) => {
            eprintln!("cannot read {}: {error}", input.display());
            return ExitCode::FAILURE;
        }
    };
    files.sort();

    let mut failures: HashMap<String, usize> = HashMap::new();
    let mut done = 0usize;

    for file in &files {
        let data = match std::fs::read(file) {
            Ok(data) => data,
            Err(error) => {
                eprintln!("{}: {error}", file.display());
                continue;
            }
        };

        match decompile(&data, &options) {
            Ok(source) => {
                let name = file.file_stem().unwrap_or_default();
                let mut path = output.join(name);
                path.set_extension("lua");
                if let Err(error) = std::fs::write(&path, source) {
                    eprintln!("{}: {error}", path.display());
                }
                done += 1;
            }
            Err(error) => {
                let key = format!("{error}");
                *failures.entry(key).or_default() += 1;
            }
        }
    }

    eprintln!("decompiled {done} of {} files", files.len());
    let mut entries: Vec<_> = failures.into_iter().collect();
    entries.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for (reason, count) in entries.iter().take(20) {
        eprintln!("{count:7}  {reason}");
    }

    ExitCode::SUCCESS
}
