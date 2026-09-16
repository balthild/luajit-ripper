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
