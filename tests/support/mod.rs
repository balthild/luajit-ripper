//! Helpers shared by the integration tests.
//!
//! This is a plain module, not a test target of its own: cargo only picks up
//! the `.rs` files directly inside `tests/`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use luajit_ripper::bytecode::Chunk;

/// Returns the LuaJIT binary to use, or `None` when there is none.
pub fn luajit() -> Option<String> {
    let candidate = std::env::var("LUAJIT").unwrap_or_else(|_| "luajit".to_string());
    let available = Command::new(&candidate)
        .arg("-v")
        .output()
        .is_ok_and(|output| output.status.success());
    available.then_some(candidate)
}

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

pub fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(fixtures_dir())
        .expect("fixtures directory should exist")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().is_some_and(|ext| ext == "lua"))
                .then(|| path.file_stem().unwrap().to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    names
}

/// Compiles a Lua source with `luajit -b -t raw` and returns the dump.
///
/// LuaJIT strips debug information by default; `debug` adds `-g`. The dump is
/// always written with `-d` (deterministic), because the order of the hash part
/// of a template table depends on LuaJIT's randomised string hash and would
/// otherwise differ between runs.
pub fn compile(luajit: &str, name: &str, source: &Path, debug: bool) -> Vec<u8> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    let dir = std::env::temp_dir().join(format!("luajit-ripper-fixtures-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp directory should be creatable");
    // Tests run in parallel, so every dump needs a path of its own. The chunk
    // name is taken from the source path, so the output name does not influence
    // the dump itself.
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let output = dir.join(format!(
        "{name}{}-{unique}.ljbc",
        if debug { "-debug" } else { "-stripped" }
    ));

    let mut command = Command::new(luajit);
    command.arg("-b");
    if debug {
        command.arg("-g");
    }
    let status = command
        .args(["-d", "-t", "raw"])
        .arg(source)
        .arg(&output)
        .output()
        .expect("luajit should be runnable");
    assert!(
        status.status.success(),
        "luajit failed to compile {}: {}",
        source.display(),
        String::from_utf8_lossy(&status.stderr)
    );

    std::fs::read(&output).expect("dump should be readable")
}

/// Compiles a snippet of Lua source and returns the dump.
pub fn compile_source(luajit: &str, name: &str, source: &str, debug: bool) -> Vec<u8> {
    let dir = std::env::temp_dir().join(format!("luajit-ripper-fixtures-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp directory should be creatable");
    let path = dir.join(format!("{name}.lua"));
    std::fs::write(&path, source).expect("snippet should be writable");
    compile(luajit, name, &path, debug)
}

/// Compiles a snippet and parses the resulting dump.
pub fn chunk_from_source(luajit: &str, name: &str, source: &str) -> &'static Chunk<'static> {
    let dump = compile_source(luajit, name, source, true);
    parse_dump(&dump)
}

/// An allocator that lives for the rest of the program.
///
/// Tests keep the whole AST alive while they inspect it, so leaking the arena
/// keeps the call sites the same shape they had before the ast was moved off
/// `Rc`.
pub fn arena() -> &'static oxc_allocator::Allocator {
    Box::leak(Box::new(oxc_allocator::Allocator::default()))
}

/// Parses a dump into a chunk that lives for the rest of the program.
pub fn parse_dump(dump: &[u8]) -> &'static Chunk<'static> {
    try_parse_dump(dump).unwrap_or_else(|error| panic!("cannot parse the dump: {error}"))
}

/// Parses a dump, keeping the error instead of panicking.
pub fn try_parse_dump(dump: &[u8]) -> Result<&'static Chunk<'static>, luajit_ripper::Error> {
    let chunk = luajit_ripper::bytecode::parse(arena(), dump)?;
    Ok(Box::leak(Box::new(chunk)))
}

/// Builds the AST of a chunk and runs the passes that come before unwarping.
///
/// This is the order the decompiler uses: the graph is repaired, the local
/// variable names are recovered, and the temporary registers are inlined.
pub fn prepare(chunk: &'static Chunk<'static>) -> Result<NodeRef<'static>, luajit_ripper::Error> {
    let alloc = arena();
    let root = luajit_ripper::ast::builder::build(alloc, chunk)?;
    luajit_ripper::ast::mutator::pre_pass(alloc, root);
    luajit_ripper::ast::locals::mark_locals(root, false);
    luajit_ripper::ast::slotworks::eliminate_temporary(
        alloc,
        root,
        luajit_ripper::ast::slotworks::Options {
            identify_slots: true,
            ..Default::default()
        },
    )?;
    Ok(root)
}

/// A shared, mutable AST node.
pub use luajit_ripper::ast::nodes::NodeRef;

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// How many dumps a run over the corpus looks at by default.
///
/// The corpus is a few tens of thousands of files and the `_ignored_*`
/// harnesses take minutes over all of them, which is too slow to sit through
/// after every change. They check this many dumps spread over the whole corpus
/// instead, and the full set is only looked at when it is asked for:
///
/// * `LJR_FULL=1` checks every dump.
/// * `LJR_SAMPLE=<n>` checks `n` of them, and `LJR_SAMPLE=0` checks all.
///
/// `LJR_FULL` wins when both are set.
const CORPUS_SAMPLE: usize = 128;

/// As much of the corpus as this run should look at.
#[derive(Debug)]
pub struct Corpus {
    /// Directory the dumps live in.
    pub dir: PathBuf,
    /// The dumps to check: a sample, unless the whole set was asked for.
    pub files: Vec<PathBuf>,
    /// How many dumps the corpus holds in total.
    pub total: usize,
}

impl Corpus {
    /// Loads the corpus, or reports that there is none and gives back `None`.
    ///
    /// A test that gets `None` is meant to return, which is what happens on a
    /// checkout without the corpus.
    pub fn load() -> Option<Corpus> {
        let Some(dir) = corpus_dir() else {
            eprintln!("corpus not present, skipping");
            return None;
        };

        let all = dumps_in(&dir);
        let total = all.len();
        if total == 0 {
            eprintln!("corpus directory {} is empty, skipping", dir.display());
            return None;
        }

        let files = sample(&all, sample_size());
        if files.len() == total {
            eprintln!("corpus: all {total} dumps");
        } else {
            eprintln!(
                "corpus: {} of {total} dumps sampled \
                 (LJR_SAMPLE=<n> changes that, LJR_FULL=1 checks the whole set)",
                files.len()
            );
        }

        Some(Corpus { dir, files, total })
    }
}

/// How many dumps to look at, where `0` means all of them.
fn sample_size() -> usize {
    if std::env::var("LJR_FULL").is_ok_and(|value| is_truthy(&value)) {
        return 0;
    }
    match std::env::var("LJR_SAMPLE") {
        Ok(value) => value.parse().unwrap_or(CORPUS_SAMPLE),
        Err(_) => CORPUS_SAMPLE,
    }
}

/// Whether an environment variable was set to something that means yes.
fn is_truthy(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

/// The directory the corpus lives in, if it is there.
pub fn corpus_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("LJR_DATASET_DIR") {
        let dir = PathBuf::from(dir);
        return dir.is_dir().then_some(dir);
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("_ignored/ljbc");
    dir.is_dir().then_some(dir)
}

/// Every dump below `dir`, in path order.
fn dumps_in(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("corpus directory should be readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "ljbc"))
        .collect();
    files.sort();
    files
}

/// Picks `size` dumps spread over the whole list.
///
/// The names are content hashes, so any few of them are as arbitrary as any
/// other few. Spreading the sample over the list is what keeps a partial run
/// from depending on where in the sorted order a dump happens to land.
fn sample(files: &[PathBuf], size: usize) -> Vec<PathBuf> {
    if size == 0 || size >= files.len() {
        return files.to_vec();
    }
    (0..size)
        .map(|index| files[index * files.len() / size].clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(count: usize) -> Vec<PathBuf> {
        (0..count)
            .map(|index| PathBuf::from(format!("{index}.ljbc")))
            .collect()
    }

    #[test]
    fn a_sample_is_spread_over_the_whole_corpus() {
        let files = numbered(100);
        let picked = sample(&files, 4);
        let expected: Vec<PathBuf> = [0, 25, 50, 75]
            .into_iter()
            .map(|index| PathBuf::from(format!("{index}.ljbc")))
            .collect();
        assert_eq!(picked, expected);
        // The start, the middle and the end of the list are all represented.
        assert_eq!(picked.first(), files.first());
        assert_eq!(picked.last(), files.get(75));
    }

    #[test]
    fn a_sample_of_everything_is_everything() {
        let files = numbered(10);
        assert_eq!(sample(&files, 10), files);
        assert_eq!(sample(&files, 0), files);
        assert_eq!(sample(&files, 99), files);
    }

    #[test]
    fn samples_do_not_repeat_a_dump() {
        let files = numbered(1000);
        let picked = sample(&files, 333);
        let mut unique = picked.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), picked.len());
    }
}
