//! Running the decompilation.
//!
//! Dumps are decompiled on a thread pool and written as they finish, but the
//! report is printed once every worker is done and in input order, so what a
//! run says does not depend on how many threads it used.
//!
//! A run over a directory also says where it is as it goes: every dump that
//! comes out is announced on stderr. The workers do not write anything
//! themselves — they send a message down a channel, and the thread that started
//! the run is the only one that ever writes, so the lines cannot be interleaved
//! and the terminal is not asked to render something half written.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use luajit_ripper::{Options, ast, bytecode, decompile_ast};
use rayon::prelude::*;

use crate::cli::allocator::with_allocator;
use crate::cli::paths::{self, Job, Sink, Tree};
use crate::cli::progress::Progress;
use crate::cli::{Error, Failure};

/// Decompiles everything `job` asks for.
pub fn run(job: &Job, options: &Options, threads: usize) -> Result<Summary, Error> {
    match &job.sink {
        Sink::Stdout => to_stdout(&job.input, options),
        Sink::File(path) => {
            let decompiled = decompile(&job.input, options).map_err(|failure| Error::Failed {
                path: job.input.clone(),
                reason: failure.to_string(),
            })?;
            write_to(path, &decompiled.source)?;
            Ok(Summary::one())
        }
        Sink::Tree(tree) => to_tree(&job.input, tree, options, threads),
    }
}

/// Decompiles one dump, writing the source to stdout.
fn to_stdout(input: &Path, options: &Options) -> Result<Summary, Error> {
    let decompiled = decompile(input, options).map_err(|failure| Error::Failed {
        path: input.to_path_buf(),
        reason: failure.to_string(),
    })?;

    let mut stdout = io::stdout().lock();
    stdout
        .write_all(decompiled.source.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(Error::Stdout)?;
    Ok(Summary::one())
}

/// A dump that was decompiled.
struct Decompiled {
    /// The source of the chunk.
    source: String,
    /// The name the chunk was compiled from, when it records one.
    name: Option<String>,
    /// Whether any part of the chunk had to be given up on.
    ///
    /// This is what tells a chunk that came out whole from one that only came
    /// out in part; see [`ast::nodes::has_recovery`].
    recovered: bool,
}

/// Reads a dump and decompiles it.
///
/// The allocator the passes work in belongs to the calling thread, and is
/// emptied on the way out: everything that has to survive the call is copied
/// into the returned values first. The walk over the chunk is the last thing
/// done inside it, since it is what has to look at the nodes before they go.
fn decompile(file: &Path, options: &Options) -> Result<Decompiled, Failure> {
    let data = fs::read(file).map_err(Failure::Read)?;
    with_allocator(|alloc| {
        let chunk = bytecode::parse(alloc, &data)?;
        // The chunk name is the module path of the dump, and it is what
        // `--module-structure` turns into the path below the output root.
        let name = chunk.header.name.map(str::to_owned);
        let root = ast::builder::build(alloc, &chunk)?;
        let source = decompile_ast(alloc, root, options)?;
        let recovered = ast::traverse::walk(root)
            .into_iter()
            .any(ast::nodes::has_recovery);
        Ok::<_, luajit_ripper::Error>(Decompiled {
            source,
            name,
            recovered,
        })
    })
    .map_err(Failure::Decompile)
}

/// The name of the chunk in `file`, read from the header alone.
///
/// `--module-structure` names the output after the module path in the dump, so
/// knowing whether a dump is already decompiled means knowing its name first —
/// and reading the whole dump to find it out would defeat the point of skipping
/// it. Only the header is parsed here, and only for that question.
///
/// A header that cannot be read gives no name: the dump is not skipped, and the
/// decompile that follows is what fails and says why.
fn chunk_name(file: &Path) -> Option<String> {
    let data = fs::read(file).ok()?;
    with_allocator(|alloc| {
        let mut reader = bytecode::Reader::new(&data);
        bytecode::header::read(alloc, &mut reader)
            .ok()?
            .name
            .map(str::to_owned)
    })
}

/// Walks the input directory and decompiles everything it holds.
fn to_tree(input: &Path, tree: &Tree, options: &Options, threads: usize) -> Result<Summary, Error> {
    let files = paths::dumps(input)?;
    let mut progress = Progress::stderr(files.len());

    // The workers report on a channel rather than writing themselves: the lines
    // would otherwise be interleaved, and a half written line on a terminal is
    // not something a second writer can take back.
    let (sender, receiver) = mpsc::channel::<(usize, Outcome)>();

    // The pool runs on a thread of its own so that building it, handing it the
    // work and closing the channel are one step: the bridge owns the sender, and
    // dropping it is what ends the loop below, which is why it is the bridge
    // rather than the main thread that has to hold it. Installing the pool from
    // the main thread would mean sending every outcome before the report can
    // start, which is exactly what the report does not need.
    let outcomes = std::thread::scope(|scope| {
        // `dumps` is borrowed rather than moved, so that the main thread keeps
        // the list it needs for the report.
        let dumps = &files;
        let bridge = scope.spawn(move || {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .map_err(Error::Pool)?;

            pool.install(|| {
                dumps.par_iter().enumerate().for_each(|(index, file)| {
                    let outcome = work(file, input, tree, options);
                    // A closed channel means the receiver is gone, which can only
                    // happen after a failure to write the progress, and the run
                    // is ending anyway.
                    let _ = sender.send((index, outcome));
                });
            });

            Ok::<_, Error>(())
        });

        // The outcomes are named here, as they arrive: this is the only thread
        // that writes, and it writes in the order the dumps were finished. The
        // report is built afterwards and in input order, so how many threads
        // ran the work still does not change what the run says.
        let mut collected: Vec<Option<Outcome>> = (0..files.len()).map(|_| None).collect();
        for (index, outcome) in receiver {
            // A dump that was left out says nothing: the whole point of leaving
            // it out is that there is nothing to report about it. Its turn is
            // still taken, so the names that do come by are numbered by how far
            // the run has got rather than by how many were written.
            match &outcome.result {
                Done::Skipped(_) => progress.skip(),
                _ => progress.step(&outcome.shown).map_err(Error::Progress)?,
            }
            collected[index] = Some(outcome);
        }

        // The channel is closed because the bridge dropped the sender, so there
        // is nothing left to wait for but the pool itself.
        let bridged = bridge.join().expect("the bridge thread does not panic");
        if bridged.is_err() {
            // The pool never ran, so there is no line to keep: it is cleared
            // here so that the failure that follows is not reported on top of
            // a name that never finished.
            progress.finish().ok();
        }
        bridged?;

        progress.finish().map_err(Error::Progress)?;
        Ok::<_, Error>(collected)
    })?;

    // A worker sends exactly one outcome per dump, so every slot is filled.
    let outcomes: Vec<Outcome> = outcomes.into_iter().flatten().collect();

    Ok(Summary::of(&files, &outcomes, tree.module_structure))
}

/// Decompiles one dump and writes it below the output root.
fn work(file: &Path, input: &Path, tree: &Tree, options: &Options) -> Outcome {
    // A dump whose output is named after its module path cannot be checked
    // without the name, and the name is in the header, so that one case reads the
    // header before deciding. A mirrored output is worked out from the input
    // path, which is already known, and reads nothing.
    let named = if tree.incremental && tree.module_structure {
        chunk_name(file)
    } else {
        None
    };

    let done: Result<Done, Failure> = (|| {
        if let Some(target) = tree.skip(input, file, named.as_deref()) {
            return Ok(Done::Skipped(target.path));
        }
        let decompiled = decompile(file, options)?;
        let target = tree.target(input, file, decompiled.name.as_deref());
        tree.prepare(&target.path)?;
        write_to(&target.path, &decompiled.source)?;
        Ok(Done::Written(Written {
            target: target.path,
            from_module: target.from_module,
            recovered: decompiled.recovered,
        }))
    })();

    // A dump that was written is named by its output path, one that failed by
    // its input path: there is no output path to name it by. A skipped dump is
    // named like a written one, since its source is where it would have gone.
    let result = match done {
        Ok(done) => done,
        Err(failure) => Done::Failed(failure.to_string()),
    };
    let shown = match &result {
        Done::Written(written) => below(&tree.root, &written.target),
        Done::Skipped(target) => below(&tree.root, target),
        Done::Failed(_) => below(input, file),
    };

    Outcome {
        input: file.to_path_buf(),
        shown,
        result,
    }
}

/// Writes the source, replacing whatever was there.
fn write_to(path: &Path, source: &str) -> Result<(), Error> {
    fs::write(path, source).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// What happened to one dump.
struct Outcome {
    /// Dump this is about.
    input: PathBuf,
    /// How the progress names this dump.
    ///
    /// A dump that was written is named by where its source went, below the
    /// output root, which is the name that tells two dumps apart. One that
    /// failed has no output path, so it is named by where it was read from,
    /// below the input root: a dump path is long, and what a reader wants to
    /// see go by is the part of it the run was pointed at.
    shown: String,
    /// What became of the dump.
    result: Done,
}

/// What became of one dump.
enum Done {
    /// It was decompiled and written.
    Written(Written),
    /// Its source was already there and as old as the dump, so it was left
    /// alone: nothing was read from it and nothing was written for it.
    Skipped(PathBuf),
    /// It was not written, with the reason why.
    Failed(String),
}

/// A dump that was written.
struct Written {
    /// Path the source was written to.
    target: PathBuf,
    /// Whether the chunk name, rather than the input location, chose it.
    from_module: bool,
    /// Whether the chunk had to be given up on in part.
    recovered: bool,
}

/// A path below `root`, or the path itself when it is not below it.
fn below(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// What a run did.
#[derive(Debug, Default)]
pub struct Summary {
    /// Dumps the input produced.
    total: usize,
    /// Dumps that were written.
    ///
    /// The report calls their number successful, less the ones that came out in
    /// part; this is the count of sources that reached the filesystem at all.
    written: usize,
    /// Dumps that were written but had to be given up on in part.
    ///
    /// Every one of these is also counted in `written`; what separates them is
    /// that something inside the chunk was not recovered, so the source is not a
    /// faithful rendering of the dump. Only `--mark-errors` lets a run finish
    /// this way: without it a chunk that cannot be recovered fails outright.
    partial: usize,
    /// Dumps that were left alone because their source was already there.
    ///
    /// These are counted nowhere else: no work was done for them, so they are
    /// neither successful nor failed, and the two together are not the total.
    skipped: usize,
    /// Output paths that were written more than once.
    collisions: usize,
    /// Failures, counted by reason.
    failures: BTreeMap<String, usize>,
    /// Dumps that failed, in input order.
    failed: Vec<(PathBuf, String)>,
    /// Dumps that fell back, in input order, paired with the path written.
    fell_back: Vec<(PathBuf, PathBuf)>,
    /// Paths written more than once, in the order they were seen.
    collided: Vec<PathBuf>,
}

impl Summary {
    /// The summary of a run that only had one dump to look at.
    fn one() -> Summary {
        Summary {
            total: 1,
            written: 1,
            ..Default::default()
        }
    }

    /// Whether every dump was written.
    pub fn ok(&self) -> bool {
        self.failed.is_empty()
    }

    /// Collects the outcomes of a tree run, in input order.
    fn of(files: &[PathBuf], outcomes: &[Outcome], module_structure: bool) -> Summary {
        let mut summary = Summary {
            total: files.len(),
            ..Default::default()
        };

        // The paths are checked in input order rather than as the workers
        // finish, so which of two colliding dumps is reported as the second one
        // does not depend on the number of threads.
        let mut seen: HashSet<&Path> = HashSet::new();
        for outcome in outcomes {
            match &outcome.result {
                Done::Written(written) => {
                    summary.written += 1;
                    if written.recovered {
                        summary.partial += 1;
                    }
                    // A path that did not come from the chunk name is one the
                    // run had to fall back on, which is only worth mentioning
                    // when a module path was asked for in the first place.
                    if !written.from_module && module_structure {
                        summary
                            .fell_back
                            .push((outcome.input.clone(), written.target.clone()));
                    }
                    if !seen.insert(&written.target) {
                        summary.collisions += 1;
                        summary.collided.push(written.target.clone());
                    }
                }
                // Nothing was written, so there is no path to collide on and no
                // fallback to mention: a skipped dump is only ever counted.
                Done::Skipped(_) => summary.skipped += 1,
                Done::Failed(reason) => {
                    *summary.failures.entry(reason.clone()).or_default() += 1;
                    summary.failed.push((outcome.input.clone(), reason.clone()));
                }
            }
        }

        summary
    }

    /// Writes the report to stderr.
    pub fn report(&self) {
        for (input, reason) in &self.failed {
            eprintln!("{}: {reason}", input.display());
        }
        for (input, target) in &self.fell_back {
            eprintln!(
                "{}: no usable module path in the dump, wrote {}",
                input.display(),
                target.display()
            );
        }
        for target in &self.collided {
            eprintln!("{}: written more than once", target.display());
        }

        if self.collisions > 0 {
            eprintln!(
                "{} output paths were written more than once",
                self.collisions
            );
        }

        let mut reasons: Vec<(&String, &usize)> = self.failures.iter().collect();
        reasons.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
        for (reason, count) in reasons.iter().take(20) {
            eprintln!("{count:7}  {reason}");
        }

        // The last line accounts for every dump, so that however much detail
        // the lines above carry, the run is summed up in one place. A dump that
        // came out in part counts as successful, because it was written: what
        // it does not promise is that the source is a faithful rendering. The
        // dumps that were skipped are said only when there are any, since a run
        // without `--incremental` has none to speak of and its last line is the
        // one it has always been.
        let skipped = if self.skipped > 0 {
            format!(", {} skipped", self.skipped)
        } else {
            String::new()
        };
        eprintln!(
            "{} files processed: {} successful, {} partial, {} failed{skipped}",
            self.total,
            self.written - self.partial,
            self.partial,
            self.failed.len(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(input: &str, target: Option<&str>) -> Outcome {
        Outcome {
            input: PathBuf::from(input),
            shown: target.unwrap_or(input).to_owned(),
            result: match target {
                Some(target) => Done::Written(Written {
                    target: PathBuf::from(target),
                    from_module: false,
                    recovered: false,
                }),
                None => Done::Failed(String::from("boom")),
            },
        }
    }

    fn skipped(input: &str, target: &str) -> Outcome {
        Outcome {
            input: PathBuf::from(input),
            shown: input.to_owned(),
            result: Done::Skipped(PathBuf::from(target)),
        }
    }

    #[test]
    fn colliding_output_paths_are_counted() {
        let files = [PathBuf::from("a.ljbc"), PathBuf::from("b.ljbc")];
        let outcomes = [
            outcome("a.ljbc", Some("x.lua")),
            outcome("b.ljbc", Some("x.lua")),
        ];
        let summary = Summary::of(&files, &outcomes, true);
        assert_eq!(summary.collisions, 1);
        assert_eq!(summary.written, 2);
        assert!(summary.ok());
    }

    #[test]
    fn a_failure_keeps_a_run_from_being_ok() {
        let files = [PathBuf::from("a.ljbc")];
        let outcomes = [outcome("a.ljbc", None)];
        let summary = Summary::of(&files, &outcomes, true);
        assert!(!summary.ok());
        assert_eq!(summary.written, 0);
        assert_eq!(summary.failures.get("boom"), Some(&1));
    }

    #[test]
    fn a_recovered_chunk_is_counted_apart_from_a_whole_one() {
        let files = [PathBuf::from("whole.ljbc"), PathBuf::from("part.ljbc")];
        let mut partial = outcome("part.ljbc", Some("part.lua"));
        if let Done::Written(written) = &mut partial.result {
            written.recovered = true;
        }
        let outcomes = [outcome("whole.ljbc", Some("whole.lua")), partial];

        let summary = Summary::of(&files, &outcomes, false);
        // A chunk that came out in part was still written, so it is counted in
        // both numbers; which of the two it lands in is what the run says about
        // the source it wrote.
        assert_eq!(summary.written, 2);
        assert_eq!(summary.partial, 1);
        assert_eq!(summary.written - summary.partial, 1);
        assert_eq!(summary.failed.len(), 0);
        assert!(summary.ok());
    }

    #[test]
    fn a_skipped_dump_is_counted_on_its_own() {
        let files = [PathBuf::from("done.ljbc"), PathBuf::from("left.ljbc")];
        let outcomes = [
            outcome("done.ljbc", Some("done.lua")),
            // The same output as one that was written, to show a skip cannot
            // collide with it: nothing was written to collide with.
            skipped("left.ljbc", "done.lua"),
        ];

        let summary = Summary::of(&files, &outcomes, false);
        assert_eq!(summary.total, 2);
        assert_eq!(summary.written, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.collisions, 0);
        assert_eq!(summary.failed.len(), 0);
        assert!(summary.ok());
    }
}
