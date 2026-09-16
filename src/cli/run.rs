//! Running the decompilation.
//!
//! Dumps are decompiled on a thread pool and written as they finish, but the
//! report is printed once every worker is done and in input order, so what a
//! run says does not depend on how many threads it used.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use luajit_ripper::{Options, ast, bytecode, decompile_ast};
use rayon::prelude::*;

use crate::cli::allocator::with_allocator;
use crate::cli::paths::{self, Job, Sink, Tree};
use crate::cli::{Error, Failure};

/// Decompiles everything `job` asks for.
pub fn run(job: &Job, options: &Options, threads: usize) -> Result<Summary, Error> {
    match &job.sink {
        Sink::Stdout => to_stdout(&job.input, options),
        Sink::File(path) => {
            let (source, _) = decompile(&job.input, options).map_err(|failure| Error::Failed {
                path: job.input.clone(),
                reason: failure.to_string(),
            })?;
            write_to(path, &source)?;
            Ok(Summary::one())
        }
        Sink::Tree(tree) => to_tree(&job.input, tree, options, threads),
    }
}

/// Decompiles one dump, writing the source to stdout.
fn to_stdout(input: &Path, options: &Options) -> Result<Summary, Error> {
    let (source, _) = decompile(input, options).map_err(|failure| Error::Failed {
        path: input.to_path_buf(),
        reason: failure.to_string(),
    })?;

    let mut stdout = io::stdout().lock();
    stdout
        .write_all(source.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(Error::Stdout)?;
    Ok(Summary::one())
}

/// Reads a dump and decompiles it, giving back the source and the chunk name.
///
/// The allocator the passes work in belongs to the calling thread, and is
/// emptied on the way out: everything that has to survive the call is copied
/// into the returned values first.
fn decompile(file: &Path, options: &Options) -> Result<(String, Option<String>), Failure> {
    let data = fs::read(file).map_err(Failure::Read)?;
    with_allocator(|alloc| {
        let chunk = bytecode::parse(alloc, &data)?;
        // The chunk name is the module path of the dump, and it is what
        // `--module-structure` turns into the path below the output root.
        let name = chunk.header.name.map(str::to_owned);
        let root = ast::builder::build(alloc, &chunk)?;
        let source = decompile_ast(alloc, root, options)?;
        Ok::<_, luajit_ripper::Error>((source, name))
    })
    .map_err(Failure::Decompile)
}

/// Walks the input directory and decompiles everything it holds.
fn to_tree(input: &Path, tree: &Tree, options: &Options, threads: usize) -> Result<Summary, Error> {
    let files = paths::dumps(input)?;

    // `num_threads(0)` asks rayon to pick a number itself, which is what the
    // default of `--threads` means.
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(Error::Pool)?;

    let outcomes: Vec<Outcome> = pool.install(|| {
        files
            .par_iter()
            .map(|file| work(file, input, tree, options))
            .collect()
    });

    Ok(Summary::of(&files, &outcomes, tree.module_structure))
}

/// Decompiles one dump and writes it below the output root.
fn work(file: &Path, input: &Path, tree: &Tree, options: &Options) -> Outcome {
    let written = (|| {
        let (source, name) = decompile(file, options)?;
        let target = tree.target(input, file, name.as_deref());
        tree.prepare(&target.path)?;
        write_to(&target.path, &source)?;
        Ok::<_, Failure>(Written {
            target: target.path,
            from_module: target.from_module,
        })
    })();

    Outcome {
        input: file.to_path_buf(),
        result: written.map_err(|failure| failure.to_string()),
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
    /// Where the source went, or why it did not.
    result: Result<Written, String>,
}

/// A dump that was written.
struct Written {
    /// Path the source was written to.
    target: PathBuf,
    /// Whether the chunk name, rather than the input location, chose it.
    from_module: bool,
}

/// What a run did.
#[derive(Debug, Default)]
pub struct Summary {
    /// Dumps the input produced.
    total: usize,
    /// Dumps that were written.
    written: usize,
    /// Output paths taken from the chunk name.
    module_paths: usize,
    /// Output paths that had to fall back to the input location.
    fallbacks: usize,
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
                Ok(written) => {
                    summary.written += 1;
                    if written.from_module {
                        summary.module_paths += 1;
                    } else if module_structure {
                        summary.fallbacks += 1;
                        summary
                            .fell_back
                            .push((outcome.input.clone(), written.target.clone()));
                    }
                    if !seen.insert(&written.target) {
                        summary.collisions += 1;
                        summary.collided.push(written.target.clone());
                    }
                }
                Err(reason) => {
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

        eprintln!("decompiled {} of {} files", self.written, self.total);
        if self.module_paths > 0 {
            eprintln!(
                "{} output paths came from the chunk name",
                self.module_paths
            );
        }
        if self.fallbacks > 0 {
            eprintln!("{} fell back to the input location", self.fallbacks);
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(input: &str, target: Option<&str>) -> Outcome {
        Outcome {
            input: PathBuf::from(input),
            result: match target {
                Some(target) => Ok(Written {
                    target: PathBuf::from(target),
                    from_module: false,
                }),
                None => Err(String::from("boom")),
            },
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
}
