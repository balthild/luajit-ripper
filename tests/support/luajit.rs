//! Talking to the LuaJIT binary.
//!
//! Compiling a source into a dump is how a test gets bytecode that is known to
//! match the LuaJIT that is actually installed, which is what the fixtures and
//! the round trips are built on.

use std::path::Path;
use std::process::Command;

use luajit_ripper::bytecode::Chunk;

use super::parse;

/// Returns the LuaJIT binary to use, or `None` when there is none.
///
/// `LUAJIT=<path>` picks one; without it, `luajit` on `PATH`. A test that gets
/// `None` is meant to report that it is skipping and return.
pub fn luajit() -> Option<String> {
    let candidate = std::env::var("LUAJIT").unwrap_or_else(|_| "luajit".to_string());
    let available = Command::new(&candidate)
        .arg("-v")
        .output()
        .is_ok_and(|output| output.status.success());
    available.then_some(candidate)
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

    // Tests run in parallel, so every dump needs a path of its own. The chunk
    // name is taken from the source path, so the output name does not influence
    // the dump itself.
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let output = work_dir().join(format!(
        "{name}{}-{unique}.ljbc",
        if debug { "-debug" } else { "-stripped" }
    ));

    compile_to(luajit, source, &output, debug).unwrap_or_else(|error| {
        panic!("cannot compile {}: {error}", source.display());
    });

    std::fs::read(&output).expect("the dump should be readable")
}

/// Compiles a source into a dump at a chosen path.
///
/// Unlike [`compile`] this writes where the caller says and hands the error
/// back, which is what a harness that keeps its own work directory needs.
pub fn compile_to(luajit: &str, source: &Path, output: &Path, debug: bool) -> Result<(), String> {
    let mut command = Command::new(luajit);
    command.arg("-b");
    if debug {
        command.arg("-g");
    }
    let status = command
        .args(["-d", "-t", "raw"])
        .arg(source)
        .arg(output)
        .output()
        .map_err(|error| format!("cannot run luajit: {error}"))?;

    if status.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&status.stderr).trim().to_string())
    }
}

/// Compiles a snippet of Lua source and returns the dump.
pub fn compile_source(luajit: &str, name: &str, source: &str, debug: bool) -> Vec<u8> {
    let path = work_dir().join(format!("{name}.lua"));
    std::fs::write(&path, source).expect("the snippet should be writable");
    compile(luajit, name, &path, debug)
}

/// Compiles a snippet and parses the resulting dump.
pub fn chunk_from_source(luajit: &str, name: &str, source: &str) -> &'static Chunk<'static> {
    let dump = compile_source(luajit, name, source, true);
    parse::parse_dump(&dump)
}

/// The directory the compiled dumps are written to.
fn work_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("luajit-ripper-fixtures-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the temp directory should be creatable");
    dir
}
