#![feature(thread_local)]

mod cli;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use luajit_ripper::{BitOpStyle, Indent, OnFunctionError, Options};

use crate::cli::paths::{self, Sink};
use crate::cli::run;

/// Decompiles LuaJIT bytecode dumps into Lua source.
#[derive(Debug, Parser)]
#[command(name = "luajit-ripper", version, about, long_about = None)]
struct Cli {
    /// The path to the dump or a directory of dumps.
    #[arg(short, long, value_name = "PATH")]
    input: PathBuf,

    /// The path to the output. If left out, a single dump goes to stdout.
    /// Required when the input is a directory.
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// For directory input, use the chunk name (such as `@modules/a/b/c.lua`)
    /// as the path in the output directory. A dump whose chunk name is missing
    /// or invalid will keep the relative path of the input file.
    #[arg(long)]
    module_structure: bool,

    /// Decompile only dumps whose outputs have different modification times.
    #[arg(long)]
    incremental: bool,

    /// For directory input, remove the lua files inside the output directory
    /// that this run did not produce.
    #[arg(long)]
    delete: bool,

    /// Allow N dumps to be decompiled in parallel; `0` picks a number based on
    /// CPU cores.
    #[arg(short = 'j', long, value_name = "N", default_value_t = 0)]
    threads: usize,

    /// Set indentation style for the decompiled source.
    #[arg(long, value_enum, default_value_t = IndentStyle::Tabs)]
    indent: IndentStyle,

    /// If indenting with spaces, sets the number of spaces per level.
    #[arg(long, value_name = "N")]
    indent_width: Option<u8>,

    /// Let unnamed registers carry the ids of the definitions they may refer to.
    #[arg(long)]
    slots: bool,

    /// Write `t.f = function(self) end` as `function t:f() end`.
    #[arg(long)]
    syntactic_sugar: bool,

    /// Write bit operations as `bit.band(a, b)` instead of `a & b`.
    #[arg(long)]
    bit_library: bool,

    /// Write the regions that cannot be structured into code with a comment
    /// pointing them out, instead of failing the entire chunk.
    #[arg(long)]
    mark_errors: bool,
}

/// What the source is indented with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum IndentStyle {
    Tabs,
    Spaces,
}

impl Cli {
    /// The decompiler options the command line asks for.
    fn options(&self) -> Result<Options, String> {
        let indent = match (self.indent, self.indent_width) {
            (IndentStyle::Tabs, None) => Indent::Tabs,
            (IndentStyle::Tabs, Some(_)) => {
                return Err(String::from("--indent-width needs --indent spaces"));
            }
            (IndentStyle::Spaces, width) => Indent::Spaces(width.unwrap_or(4)),
        };

        Ok(Options {
            indent,
            bitop_style: if self.bit_library {
                BitOpStyle::BitLibrary
            } else {
                BitOpStyle::Operator
            },
            on_function_error: if self.mark_errors {
                OnFunctionError::Mark
            } else {
                OnFunctionError::Fail
            },
            show_slot_ids: self.slots,
            function_definition_sugar: self.syntactic_sugar,
        })
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let options = match cli.options() {
        Ok(options) => options,
        Err(message) => return fail(&message),
    };

    let layout = paths::Layout {
        module_structure: cli.module_structure,
        incremental: cli.incremental,
        delete: cli.delete,
    };

    let job = match paths::resolve(&cli.input, cli.output.as_deref(), &layout) {
        Ok(job) => job,
        Err(error) => return fail(&error.to_string()),
    };

    let summary = match run::run(&job, &options, cli.threads) {
        Ok(summary) => summary,
        Err(error) => return fail(&error.to_string()),
    };

    // A single dump says nothing worth saying: either it worked, or the error
    // above has already been printed.
    if matches!(job.sink, Sink::Tree(_)) {
        summary.report();
    }

    if summary.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Reports a failure and gives back the exit code for it.
fn fail(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::FAILURE
}
