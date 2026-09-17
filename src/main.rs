#![feature(thread_local)]

//! The `luajit-ripper` command line tool.
//!
//! ```text
//! luajit-ripper --input <dump.ljbc> [--output <file.lua>]
//! luajit-ripper --input <dir of dumps> --output <dir> [--module-structure]
//!                                            [--incremental]
//! ```
//!
//! A single dump without an output goes to stdout; a directory of dumps is
//! decompiled on a thread pool into an output directory, either mirroring the
//! layout of the input or following the module path recorded in each dump. With
//! `--incremental`, a dump whose source is already there and as old as the dump
//! itself is left alone, so a rerun only does the work that is out of date.
//!
//! The tool is only built when the `cli` feature is on, which is what pulls in
//! clap, rayon and walkdir. It also needs a nightly compiler, because the
//! allocator each worker keeps is a `#[thread_local]` static.

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
    /// A dump to decompile, or a directory holding dumps.
    #[arg(short, long, value_name = "PATH")]
    input: PathBuf,

    /// Where to write the source.
    ///
    /// Left out, a single dump is written to stdout. A directory of dumps needs
    /// an output directory, which is created when its parent already exists.
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// Name every output after the module path recorded in its dump.
    ///
    /// A dump holds the name of the file it was compiled from, such as
    /// `@modules/logic/rouge/map/Foo.lua`. The name is used as a path below the
    /// output directory, with the leading `@` kept, which turns a flat
    /// collection of hashed dumps back into the tree it was built from. A dump
    /// without a usable name keeps the path of its input file. Directory input
    /// only, since it is what makes two dumps tell themselves apart.
    #[arg(long)]
    module_structure: bool,

    /// Leave a dump alone when its source is already there and just as old.
    ///
    /// A dump is skipped when the file it would be written to exists and carries
    /// the same modification time as the dump itself — the same moment, not one
    /// at least as new, so a dump compiled again is decompiled again, and so is a
    /// source that was edited after the fact. A skipped dump is not even read, so
    /// it also says nothing while the run is under way; the report counts it.
    /// Directory input only, since it is a rerun over a directory that it saves.
    #[arg(long)]
    incremental: bool,

    /// How many dumps to decompile at once. Zero picks a number automatically.
    #[arg(short = 'j', long, value_name = "N", default_value_t = 0)]
    threads: usize,

    /// What to indent the source with.
    #[arg(long, value_enum, default_value_t = IndentStyle::Tabs)]
    indent: IndentStyle,

    /// How many spaces make a level. Needs --indent spaces.
    #[arg(long, value_name = "N")]
    indent_width: Option<u8>,

    /// Let unnamed registers carry the ids of the definitions they may refer to.
    #[arg(long)]
    slots: bool,

    /// Write `t.f = function () end` as `function t.f() end`.
    #[arg(long)]
    syntactic_sugar: bool,

    /// Write bit operations as `bit.band(a, b)` instead of `a & b`.
    #[arg(long)]
    bit_library: bool,

    /// Write the regions that cannot be structured into code, with a comment
    /// pointing them out, instead of failing the chunk they are in.
    #[arg(long)]
    mark_errors: bool,
}

/// What the source is indented with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum IndentStyle {
    /// One tab per level, which is what the original compiler is fed.
    Tabs,
    /// A chosen number of spaces.
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

    let job = match paths::resolve(
        &cli.input,
        cli.output.as_deref(),
        cli.module_structure,
        cli.incremental,
    ) {
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
