//! Tests for the command line tool.
//!
//! Every test runs the binary as a child process, so what is checked is what a
//! user sees: the exit code, the files that end up on disk, and what the tool
//! says about them. Dumps are made with a real `luajit`, as they are in the
//! rest of the suite, and a test that needs one is skipped when there is none.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A directory of its own for one test, removed when the test ends.
struct Temp {
    path: PathBuf,
}

impl Temp {
    fn new(name: &str) -> Temp {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "luajit-ripper-cli-{}-{unique}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("the temporary directory should be creatable");
        Temp { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }

    /// A path as a string, for the command line.
    fn text(&self, relative: &str) -> String {
        self.join(relative).to_string_lossy().into_owned()
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// What one run of the tool said.
struct Run {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Run {
    fn succeeded(&self) -> bool {
        self.status.success()
    }
}

/// Runs the tool with the arguments given.
fn run(arguments: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_luajit-ripper"))
        .args(arguments)
        .output()
        .expect("the tool should be runnable");
    Run {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The source every test compiles, which decompiles to something recognisable.
const SOURCE: &str = "local function add(a, b)\n\treturn a + b\nend\n\nreturn add\n";

/// Compiles `SOURCE` into the dump `dump`.
///
/// `relative` is both where the source is written below `work` and what LuaJIT
/// records as the chunk name, so a source at `modules/pkg/a.lua` leaves
/// `@modules/pkg/a.lua` in the dump, which is the shape a real application's
/// dumps have. The compiler runs with `work` as its directory, so the name it
/// records is the relative path rather than where the test happens to run.
fn compile(luajit: &str, work: &Path, relative: &str, dump: &Path, debug: bool) -> Vec<u8> {
    let source = work.join(relative);
    fs::create_dir_all(source.parent().expect("a source has a directory"))
        .expect("the source directory should be creatable");
    fs::write(&source, SOURCE).expect("the source should be writable");

    let mut command = Command::new(luajit);
    command.current_dir(work).arg("-b");
    if debug {
        command.arg("-g");
    }
    let output = command
        .args(["-d", "-t", "raw"])
        .arg(relative)
        .arg(dump)
        .output()
        .expect("luajit should be runnable");
    assert!(
        output.status.success(),
        "luajit failed to compile {relative}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read(dump).expect("the dump should be readable")
}

/// Skips a test when there is no LuaJIT to build dumps with.
macro_rules! luajit {
    () => {
        match support::luajit() {
            Some(luajit) => luajit,
            None => {
                eprintln!("luajit not found, skipping");
                return;
            }
        }
    };
}

/// Gives `path` the modification time of `like`.
///
/// `--incremental` asks for the same *moment*, not for "newer", so a test that
/// wants a skip has to put a source and its dump at the very same time rather
/// than hope they landed close enough together.
fn age_like(path: &Path, like: &Path) {
    let time = fs::metadata(like).unwrap().modified().unwrap();
    set_mtime(path, time);
}

/// Gives `path` a modification time `seconds` away from that of `like`.
fn age_apart(path: &Path, like: &Path, seconds: u64) {
    let time = fs::metadata(like).unwrap().modified().unwrap();
    set_mtime(path, time + std::time::Duration::from_secs(seconds));
}

/// Moves the modification time of `path` to `time`.
fn set_mtime(path: &Path, time: std::time::SystemTime) {
    // The file is opened for writing: changing the times of a handle that only
    // reads is not something every platform allows.
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("the output file should be there")
        .set_modified(time)
        .expect("the modification time should be settable");
}

#[test]
fn a_dump_without_an_output_goes_to_stdout() {
    let luajit = luajit!();
    let temp = Temp::new("stdout");
    let dump = temp.join("chunk.ljbc");
    compile(&luajit, temp.path(), "chunk.lua", &dump, true);

    let plain = run(&["--input", &dump.to_string_lossy()]);
    assert!(plain.succeeded(), "{}", plain.stderr);
    assert!(plain.stdout.contains("return add"), "{}", plain.stdout);
    assert!(plain.stderr.is_empty(), "{}", plain.stderr);

    // An output that names a file is the same source, in a file.
    let written = run(&[
        "--input",
        &dump.to_string_lossy(),
        "--output",
        &temp.text("chunk.out.lua"),
    ]);
    assert!(written.succeeded(), "{}", written.stderr);
    assert_eq!(
        fs::read_to_string(temp.join("chunk.out.lua")).unwrap(),
        plain.stdout
    );
}

#[test]
fn a_file_output_needs_a_directory_that_exists() {
    let luajit = luajit!();
    let temp = Temp::new("file-output");
    let dump = temp.join("chunk.ljbc");
    compile(&luajit, temp.path(), "chunk.lua", &dump, true);

    let refused = run(&[
        "--input",
        &dump.to_string_lossy(),
        "--output",
        &temp.text("missing/chunk.lua"),
    ]);
    assert!(!refused.succeeded());
    assert!(
        refused.stderr.contains("does not exist"),
        "{}",
        refused.stderr
    );
    // Nothing may be created for a single file, chain of folders included.
    assert!(!temp.join("missing").exists());

    // A directory in the place of the output file is refused too.
    fs::create_dir(temp.join("there")).unwrap();
    let refused = run(&[
        "--input",
        &dump.to_string_lossy(),
        "--output",
        &temp.text("there"),
    ]);
    assert!(!refused.succeeded());
    assert!(
        refused.stderr.contains("must be a file"),
        "{}",
        refused.stderr
    );
}

#[test]
fn a_directory_input_needs_an_output_directory() {
    let luajit = luajit!();
    let temp = Temp::new("dir-output");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    compile(
        &luajit,
        temp.path(),
        "chunk.lua",
        &dumps.join("chunk.ljbc"),
        true,
    );

    let refused = run(&["--input", &dumps.to_string_lossy()]);
    assert!(!refused.succeeded());
    assert!(
        refused.stderr.contains("--output is required"),
        "{}",
        refused.stderr
    );
}

#[test]
fn an_output_directory_is_created_one_level_at_a_time() {
    let luajit = luajit!();
    let temp = Temp::new("tree-root");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    compile(
        &luajit,
        temp.path(),
        "chunk.lua",
        &dumps.join("chunk.ljbc"),
        true,
    );

    // The parent is there, so one level is created for the run.
    let created = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &temp.text("fresh"),
    ]);
    assert!(created.succeeded(), "{}", created.stderr);
    assert!(temp.join("fresh").is_dir());
    assert!(temp.join("fresh/chunk.lua").is_file());

    // The parent is not there, so nothing is created: no `mkdir -p`.
    let refused = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &temp.text("missing/fresh"),
    ]);
    assert!(!refused.succeeded());
    assert!(
        refused.stderr.contains("does not exist"),
        "{}",
        refused.stderr
    );
    assert!(!temp.join("missing").exists());
}

#[test]
fn the_layout_of_the_input_is_mirrored() {
    let luajit = luajit!();
    let temp = Temp::new("mirror");
    let dumps = temp.join("dumps");
    fs::create_dir_all(dumps.join("sub/deeper")).unwrap();
    compile(
        &luajit,
        temp.path(),
        "chunk.lua",
        &dumps.join("top.ljbc"),
        true,
    );
    compile(
        &luajit,
        temp.path(),
        "chunk.lua",
        &dumps.join("sub/mid.ljbc"),
        true,
    );
    compile(
        &luajit,
        temp.path(),
        "chunk.lua",
        &dumps.join("sub/deeper/low.ljbc"),
        true,
    );
    // Anything that is not a dump is left alone.
    fs::write(dumps.join("notes.txt"), "not a dump").unwrap();

    let output = temp.text("out");
    let mirror = run(&["--input", &dumps.to_string_lossy(), "--output", &output]);
    assert!(mirror.succeeded(), "{}", mirror.stderr);
    assert!(temp.join("out/top.lua").is_file());
    assert!(temp.join("out/sub/mid.lua").is_file());
    assert!(temp.join("out/sub/deeper/low.lua").is_file());
    assert!(!temp.join("out/notes.txt").exists());
    assert!(
        mirror
            .stderr
            .contains("3 files processed: 3 successful, 0 partial, 0 failed"),
        "{}",
        mirror.stderr
    );
}

#[test]
fn module_structure_writes_below_the_name_in_the_dump() {
    let luajit = luajit!();
    let temp = Temp::new("modules");
    let work = temp.join("work");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    compile(
        &luajit,
        &work,
        "modules/pkg/one.lua",
        &dumps.join("aaaa.ljbc"),
        true,
    );
    compile(
        &luajit,
        &work,
        "modules/other/two.lua",
        &dumps.join("bbbb.ljbc"),
        true,
    );

    let output = temp.text("out");
    let run_ = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &output,
        "--module-structure",
    ]);
    assert!(run_.succeeded(), "{}", run_.stderr);
    // The hash of the dump says nothing about the module it holds; the name
    // inside it is what the path is built from, `@` and all.
    assert!(temp.join("out/@modules/pkg/one.lua").is_file());
    assert!(temp.join("out/@modules/other/two.lua").is_file());
    assert!(!temp.join("out/aaaa.lua").exists());
}

#[test]
fn a_dump_without_a_name_falls_back_to_its_input_path() {
    let luajit = luajit!();
    let temp = Temp::new("fallback");
    let dumps = temp.join("dumps");
    fs::create_dir_all(dumps.join("sub")).unwrap();
    // A stripped dump carries neither debug information nor a chunk name.
    compile(
        &luajit,
        temp.path(),
        "chunk.lua",
        &dumps.join("sub/plain.ljbc"),
        false,
    );

    let output = temp.text("out");
    let run_ = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &output,
        "--module-structure",
    ]);
    assert!(run_.succeeded(), "{}", run_.stderr);
    assert!(temp.join("out/sub/plain.lua").is_file());
    assert!(
        run_.stderr.contains("no usable module path"),
        "{}",
        run_.stderr
    );
}

#[test]
fn two_dumps_that_name_the_same_module_are_reported() {
    let luajit = luajit!();
    let temp = Temp::new("collision");
    let work = temp.join("work");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    compile(
        &luajit,
        &work,
        "modules/dup.lua",
        &dumps.join("one.ljbc"),
        true,
    );
    compile(
        &luajit,
        &work,
        "modules/dup.lua",
        &dumps.join("two.ljbc"),
        true,
    );

    let output = temp.text("out");
    let run_ = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &output,
        "--module-structure",
    ]);
    assert!(run_.succeeded(), "{}", run_.stderr);
    assert!(temp.join("out/@modules/dup.lua").is_file());
    assert!(
        run_.stderr.contains("written more than once"),
        "{}",
        run_.stderr
    );
}

#[test]
fn module_structure_needs_a_directory_to_work_on() {
    let luajit = luajit!();
    let temp = Temp::new("modules-single");
    let dump = temp.join("chunk.ljbc");
    compile(&luajit, temp.path(), "chunk.lua", &dump, true);

    let refused = run(&[
        "--input",
        &dump.to_string_lossy(),
        "--output",
        &temp.text("chunk.out.lua"),
        "--module-structure",
    ]);
    assert!(!refused.succeeded());
    assert!(
        refused.stderr.contains("needs a directory as --input"),
        "{}",
        refused.stderr
    );
}

#[test]
fn incremental_needs_a_directory_to_be_incremental_about() {
    let luajit = luajit!();
    let temp = Temp::new("incremental-single");
    let dump = temp.join("chunk.ljbc");
    compile(&luajit, temp.path(), "chunk.lua", &dump, true);

    let refused = run(&[
        "--input",
        &dump.to_string_lossy(),
        "--output",
        &temp.text("chunk.out.lua"),
        "--incremental",
    ]);
    assert!(!refused.succeeded());
    assert!(
        refused
            .stderr
            .contains("--incremental needs a directory as --input"),
        "{}",
        refused.stderr
    );
}

#[test]
fn an_incremental_run_leaves_a_source_of_the_same_age_alone() {
    let luajit = luajit!();
    let temp = Temp::new("incremental-same");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    let dump = dumps.join("chunk.ljbc");
    compile(&luajit, temp.path(), "chunk.lua", &dump, true);

    let output = temp.text("out");
    let first = run(&["--input", &dumps.to_string_lossy(), "--output", &output]);
    assert!(first.succeeded(), "{}", first.stderr);

    // What the source holds is what says whether it was written again: the
    // sentinel is not what a decompile would produce.
    let source = temp.join("out/chunk.lua");
    fs::write(&source, "-- left alone\n").unwrap();
    age_like(&source, &dump);

    let second = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &output,
        "--incremental",
    ]);
    assert!(second.succeeded(), "{}", second.stderr);
    assert_eq!(fs::read_to_string(&source).unwrap(), "-- left alone\n");
    assert!(
        second
            .stderr
            .contains("1 files processed: 0 successful, 0 partial, 0 failed, 1 skipped"),
        "{}",
        second.stderr
    );
    // Nothing to say about it while the run is under way, either.
    assert!(!second.stderr.contains("[1/1]"), "{}", second.stderr);
}

/// The source a run writes carries the time of the dump it came from.
///
/// This is the whole reason a second run has anything to go on: a source is
/// written after its dump is read, so left to itself it would always look newer
/// than the dump and never be skipped. The check is checked here on its own,
/// since a test that only looks at what the second run says would pass even if
/// the time came from somewhere else.
#[test]
fn an_incremental_run_dates_a_source_like_its_dump() {
    let luajit = luajit!();
    let temp = Temp::new("incremental-dates");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    let dump = dumps.join("chunk.ljbc");
    compile(&luajit, temp.path(), "chunk.lua", &dump, true);

    let output = temp.text("out");
    let run_ = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &output,
        "--incremental",
    ]);
    assert!(run_.succeeded(), "{}", run_.stderr);

    let source = temp.join("out/chunk.lua");
    assert_eq!(
        fs::metadata(&source).unwrap().modified().unwrap(),
        fs::metadata(&dump).unwrap().modified().unwrap(),
        "the source should carry the time of the dump it was written from"
    );
}

/// A second incremental run over its own output has nothing left to do.
#[test]
fn an_incremental_rerun_leaves_its_own_output_alone() {
    let luajit = luajit!();
    let temp = Temp::new("incremental-rerun");
    let work = temp.join("work");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    for (relative, name) in [
        ("modules/pkg/one.lua", "aaaa.ljbc"),
        ("modules/pkg/two.lua", "bbbb.ljbc"),
        ("modules/other/three.lua", "cccc.ljbc"),
    ] {
        compile(&luajit, &work, relative, &dumps.join(name), true);
    }

    let output = temp.text("out");
    let arguments = [
        "--input",
        &*dumps.to_string_lossy(),
        "--output",
        &output,
        "--module-structure",
        "--incremental",
    ];

    let first = run(&arguments);
    assert!(first.succeeded(), "{}", first.stderr);
    assert!(
        first
            .stderr
            .contains("3 files processed: 3 successful, 0 partial, 0 failed"),
        "{}",
        first.stderr
    );

    // Nothing has changed in between, so there is nothing to do: not a dump
    // rewritten, not a line of progress, only the account of the run.
    let second = run(&arguments);
    assert!(second.succeeded(), "{}", second.stderr);
    assert!(
        second
            .stderr
            .contains("3 files processed: 0 successful, 0 partial, 0 failed, 3 skipped"),
        "{}",
        second.stderr
    );

    // A dump compiled again is not the dump the source was written from: its
    // time moves on and the source stops matching it. The times are set apart by
    // hand rather than left to the clock, so that the test does not depend on
    // how fine the filesystem's idea of a moment is.
    let one = temp.join("out/@modules/pkg/one.lua");
    let two = temp.join("out/@modules/pkg/two.lua");
    let redone = dumps.join("aaaa.ljbc");
    compile(&luajit, &work, "modules/pkg/one.lua", &redone, true);
    age_apart(&redone, &one, 120);

    // A source someone edited carries the time of the edit, which is not the
    // dump's either.
    fs::write(&two, "-- edited\n").unwrap();
    age_apart(&two, &dumps.join("bbbb.ljbc"), 120);

    let third = run(&arguments);
    assert!(third.succeeded(), "{}", third.stderr);
    assert!(
        third
            .stderr
            .contains("3 files processed: 2 successful, 0 partial, 0 failed, 1 skipped"),
        "{}",
        third.stderr
    );
    assert!(
        fs::read_to_string(&one).unwrap().contains("return add"),
        "the source of a dump compiled again should have been written again"
    );
    assert!(
        fs::read_to_string(&two).unwrap().contains("return add"),
        "the edited source should have been written again"
    );
}

#[test]
fn an_incremental_run_writes_a_source_of_a_different_age() {
    let luajit = luajit!();
    let temp = Temp::new("incremental-stale");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    let dump = dumps.join("chunk.ljbc");
    compile(&luajit, temp.path(), "chunk.lua", &dump, true);

    let output = temp.text("out");
    let first = run(&["--input", &dumps.to_string_lossy(), "--output", &output]);
    assert!(first.succeeded(), "{}", first.stderr);

    let source = temp.join("out/chunk.lua");
    fs::write(&source, "-- stale\n").unwrap();
    // An output that is not the same moment as its dump is out of date, whether
    // it is older or newer: a source edited by hand is newer, and it still has
    // to go.
    age_apart(&source, &dump, 60);

    let second = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &output,
        "--incremental",
    ]);
    assert!(second.succeeded(), "{}", second.stderr);
    assert!(
        fs::read_to_string(&source).unwrap().contains("return add"),
        "{}",
        fs::read_to_string(&source).unwrap()
    );
    assert!(
        second
            .stderr
            .contains("1 files processed: 1 successful, 0 partial, 0 failed"),
        "{}",
        second.stderr
    );
    assert!(!second.stderr.contains("skipped"), "{}", second.stderr);
}

#[test]
fn an_incremental_run_finds_a_source_named_after_its_module() {
    let luajit = luajit!();
    let temp = Temp::new("incremental-modules");
    let work = temp.join("work");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    let dump = dumps.join("aaaa.ljbc");
    compile(&luajit, &work, "modules/pkg/one.lua", &dump, true);

    let output = temp.text("out");
    let first = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &output,
        "--module-structure",
    ]);
    assert!(first.succeeded(), "{}", first.stderr);

    // The output is not below a path the input tells the run about, so finding
    // it means reading the name out of the dump's own header.
    let source = temp.join("out/@modules/pkg/one.lua");
    assert!(source.is_file(), "{}", first.stderr);
    fs::write(&source, "-- left alone\n").unwrap();
    age_like(&source, &dump);

    let second = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &output,
        "--module-structure",
        "--incremental",
    ]);
    assert!(second.succeeded(), "{}", second.stderr);
    assert_eq!(fs::read_to_string(&source).unwrap(), "-- left alone\n");
    assert!(second.stderr.contains("1 skipped"), "{}", second.stderr);
}

#[test]
fn an_incremental_run_still_reports_a_dump_it_cannot_read() {
    let temp = Temp::new("incremental-broken");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    fs::write(dumps.join("broken.ljbc"), b"this is not a dump").unwrap();

    let output = temp.text("out");
    let run_ = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &output,
        "--incremental",
    ]);
    // A dump with no readable header has no name to look a source up by, so it
    // is not skipped: it fails, and the run says why.
    assert!(!run_.succeeded());
    assert!(
        run_.stderr
            .contains("1 files processed: 0 successful, 0 partial, 1 failed"),
        "{}",
        run_.stderr
    );
    assert!(run_.stderr.contains("bad magic"), "{}", run_.stderr);
}

#[test]
fn delete_needs_a_directory_to_delete_from() {
    let luajit = luajit!();
    let temp = Temp::new("delete-single");
    let dump = temp.join("chunk.ljbc");
    compile(&luajit, temp.path(), "chunk.lua", &dump, true);

    let refused = run(&[
        "--input",
        &dump.to_string_lossy(),
        "--output",
        &temp.text("chunk.out.lua"),
        "--delete",
    ]);
    assert!(!refused.succeeded());
    assert!(
        refused
            .stderr
            .contains("--delete needs a directory as --input"),
        "{}",
        refused.stderr
    );
}

#[test]
fn delete_refuses_an_output_that_holds_the_input() {
    let luajit = luajit!();
    let temp = Temp::new("delete-overlap");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    compile(
        &luajit,
        temp.path(),
        "chunk.lua",
        &dumps.join("chunk.ljbc"),
        true,
    );

    // The output is the input's parent, so walking it would reach the dump the
    // run is reading from.
    let above = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &temp.path().to_string_lossy(),
        "--delete",
    ]);
    assert!(!above.succeeded());
    assert!(
        above.stderr.contains("--delete would delete"),
        "{}",
        above.stderr
    );

    // And the output being the input itself is the same case, seen from closer.
    let same = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &dumps.to_string_lossy(),
        "--delete",
    ]);
    assert!(!same.succeeded());
    assert!(
        same.stderr.contains("--delete would delete"),
        "{}",
        same.stderr
    );
}

#[test]
fn delete_removes_an_output_no_dump_produced() {
    let luajit = luajit!();
    let temp = Temp::new("delete-stale");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    compile(
        &luajit,
        temp.path(),
        "chunk.lua",
        &dumps.join("chunk.ljbc"),
        true,
    );

    let output = temp.text("out");
    let first = run(&["--input", &dumps.to_string_lossy(), "--output", &output]);
    assert!(first.succeeded(), "{}", first.stderr);

    // Two leftovers of an earlier run: a source in a directory of its own, and
    // one next to the output, plus a file this tool would never have written.
    fs::create_dir_all(temp.join("out/gone")).unwrap();
    fs::write(temp.join("out/gone/stale.lua"), "-- stale\n").unwrap();
    fs::write(temp.join("out/deeper.lua"), "-- stale\n").unwrap();
    fs::write(temp.join("out/notes.txt"), "keep me\n").unwrap();

    let second = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &output,
        "--delete",
    ]);
    assert!(second.succeeded(), "{}", second.stderr);

    assert!(temp.join("out/chunk.lua").is_file());
    assert!(!temp.join("out/gone/stale.lua").exists());
    assert!(!temp.join("out/deeper.lua").exists());
    // The directory the stale source was the last thing in goes with it, and
    // the file this tool did not write is left where it is.
    assert!(!temp.join("out/gone").exists());
    assert_eq!(
        fs::read_to_string(temp.join("out/notes.txt")).unwrap(),
        "keep me\n"
    );
    assert!(second.stderr.contains("2 deleted"), "{}", second.stderr);
}

#[test]
fn delete_keeps_what_the_run_wrote_and_what_it_skipped() {
    let luajit = luajit!();
    let temp = Temp::new("delete-keeps");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    for name in ["one", "two"] {
        compile(
            &luajit,
            temp.path(),
            &format!("{name}.lua"),
            &dumps.join(format!("{name}.ljbc")),
            true,
        );
    }

    let output = temp.text("out");
    let first = run(&["--input", &dumps.to_string_lossy(), "--output", &output]);
    assert!(first.succeeded(), "{}", first.stderr);

    // One dump is compiled again and the other is not, so the run writes one
    // source and leaves the other alone; both are this run's output, and
    // deleting either would throw away work that is current.
    compile(
        &luajit,
        temp.path(),
        "one.lua",
        &dumps.join("one.ljbc"),
        true,
    );
    let one = temp.join("out/one.lua");
    let two = temp.join("out/two.lua");
    age_like(&two, &dumps.join("two.ljbc"));

    let second = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &output,
        "--incremental",
        "--delete",
    ]);
    assert!(second.succeeded(), "{}", second.stderr);
    assert_eq!(
        fs::metadata(&two).unwrap().modified().unwrap(),
        fs::metadata(dumps.join("two.ljbc"))
            .unwrap()
            .modified()
            .unwrap()
    );
    assert!(one.is_file());
    assert!(two.is_file());
    assert!(second.stderr.contains("1 skipped"), "{}", second.stderr);
    assert!(!second.stderr.contains("deleted"), "{}", second.stderr);
}

#[test]
fn delete_is_held_back_when_a_dump_fails() {
    let luajit = luajit!();
    let temp = Temp::new("delete-failed");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    compile(
        &luajit,
        temp.path(),
        "chunk.lua",
        &dumps.join("good.ljbc"),
        true,
    );

    let output = temp.text("out");
    let first = run(&["--input", &dumps.to_string_lossy(), "--output", &output]);
    assert!(first.succeeded(), "{}", first.stderr);

    // A dump that cannot be read makes the run one that does not know what it
    // produced, so the leftover is left alone rather than taken on a guess.
    fs::write(temp.join("out/stale.lua"), "-- stale\n").unwrap();
    fs::write(dumps.join("broken.ljbc"), b"this is not a dump").unwrap();

    let second = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &output,
        "--delete",
    ]);
    assert!(!second.succeeded());
    assert!(temp.join("out/stale.lua").is_file(), "{}", second.stderr);
    assert!(
        second.stderr.contains("nothing deleted"),
        "{}",
        second.stderr
    );
}

#[test]
fn delete_finds_a_stale_output_named_after_its_module() {
    let luajit = luajit!();
    let temp = Temp::new("delete-modules");
    let work = temp.join("work");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    compile(
        &luajit,
        &work,
        "modules/pkg/one.lua",
        &dumps.join("aaaa.ljbc"),
        true,
    );

    let output = temp.text("out");
    let first = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &output,
        "--module-structure",
    ]);
    assert!(first.succeeded(), "{}", first.stderr);

    // The output of this run is not below a path the input tells the run about,
    // so what belongs there is only known by reading the dump's header; the
    // leftovers are named after modules that no dump mentions.
    let kept = temp.join("out/@modules/pkg/one.lua");
    assert!(kept.is_file(), "{}", first.stderr);
    fs::create_dir_all(temp.join("out/@modules/other")).unwrap();
    fs::write(temp.join("out/@modules/other/gone.lua"), "-- stale\n").unwrap();

    let second = run(&[
        "--input",
        &dumps.to_string_lossy(),
        "--output",
        &output,
        "--module-structure",
        "--delete",
    ]);
    assert!(second.succeeded(), "{}", second.stderr);
    assert!(kept.is_file());
    assert!(!temp.join("out/@modules/other/gone.lua").exists());
    assert!(!temp.join("out/@modules/other").exists());
    assert!(second.stderr.contains("1 deleted"), "{}", second.stderr);
}

#[test]
fn a_dump_that_cannot_be_read_does_not_stop_the_others() {
    let luajit = luajit!();
    let temp = Temp::new("broken");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    compile(
        &luajit,
        temp.path(),
        "chunk.lua",
        &dumps.join("good.ljbc"),
        true,
    );
    fs::write(dumps.join("broken.ljbc"), b"this is not a dump").unwrap();

    let output = temp.text("out");
    let run_ = run(&["--input", &dumps.to_string_lossy(), "--output", &output]);
    assert!(!run_.succeeded());
    assert!(temp.join("out/good.lua").is_file());
    assert!(!temp.join("out/broken.lua").exists());
    assert!(
        run_.stderr
            .contains("2 files processed: 1 successful, 0 partial, 1 failed"),
        "{}",
        run_.stderr
    );
    assert!(run_.stderr.contains("bad magic"), "{}", run_.stderr);
}

#[test]
fn the_number_of_threads_does_not_change_the_output() {
    let luajit = luajit!();
    let temp = Temp::new("threads");
    let work = temp.join("work");
    let dumps = temp.join("dumps");
    fs::create_dir_all(&dumps).unwrap();
    for index in 0..8 {
        let relative = format!("modules/m{index}.lua");
        compile(
            &luajit,
            &work,
            &relative,
            &dumps.join(format!("dump{index}.ljbc")),
            true,
        );
    }

    for (threads, name) in [("1", "one"), ("8", "eight")] {
        let output = temp.text(name);
        let run_ = run(&[
            "--input",
            &dumps.to_string_lossy(),
            "--output",
            &output,
            "--module-structure",
            "--threads",
            threads,
        ]);
        assert!(run_.succeeded(), "{}", run_.stderr);
        assert!(
            run_.stderr
                .contains("8 files processed: 8 successful, 0 partial, 0 failed"),
            "{}",
            run_.stderr
        );
    }

    let one = temp.join("one");
    let eight = temp.join("eight");
    for index in 0..8 {
        let relative = format!("@modules/m{index}.lua");
        let first = fs::read(one.join(&relative)).expect("the single threaded output");
        let second = fs::read(eight.join(&relative)).expect("the parallel output");
        assert_eq!(first, second, "{relative} differs between runs");
    }
}

#[test]
fn every_dump_is_announced_as_it_comes_out() {
    let luajit = luajit!();
    let temp = Temp::new("progress");
    let dumps = temp.join("dumps");
    fs::create_dir_all(dumps.join("sub")).unwrap();
    compile(
        &luajit,
        temp.path(),
        "chunk.lua",
        &dumps.join("one.ljbc"),
        true,
    );
    compile(
        &luajit,
        temp.path(),
        "chunk.lua",
        &dumps.join("two.ljbc"),
        true,
    );
    compile(
        &luajit,
        temp.path(),
        "chunk.lua",
        &dumps.join("sub/three.ljbc"),
        true,
    );

    let output = temp.text("out");
    let run_ = run(&["--input", &dumps.to_string_lossy(), "--output", &output]);
    assert!(run_.succeeded(), "{}", run_.stderr);

    // stderr is a pipe here, not a terminal, so the lines are written one after
    // another instead of taking each other's place; nothing writes escapes a
    // reader of a log would have to look past.
    assert!(!run_.stderr.contains('\u{1b}'), "{}", run_.stderr);

    let lines: Vec<&str> = run_.stderr.lines().collect();
    // The progress comes first, one line per dump, named by the file its source
    // went to. Which dump is finished first is up to the pool, so the names are
    // compared as a set; the counter is not, so it is read off in order.
    let mut announced: Vec<&str> = Vec::new();
    for (index, line) in lines.iter().take(3).enumerate() {
        let (count, name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.split_once("] "))
            .unwrap_or_else(|| panic!("not a progress line: {line}"));
        assert_eq!(count, format!("{}/3", index + 1), "{line}");
        announced.push(name);
    }
    announced.sort_unstable();
    assert_eq!(announced, ["one.lua", "sub/three.lua", "two.lua"]);

    // The account of the run is the last thing said, so that it is what a
    // reader is left with whatever went by above it.
    assert_eq!(
        lines.last(),
        Some(&"3 files processed: 3 successful, 0 partial, 0 failed")
    );
}

#[test]
fn a_dump_that_fails_is_announced_by_its_input_path() {
    let luajit = luajit!();
    let temp = Temp::new("progress-broken");
    let dumps = temp.join("dumps");
    fs::create_dir_all(dumps.join("sub")).unwrap();
    compile(
        &luajit,
        temp.path(),
        "chunk.lua",
        &dumps.join("good.ljbc"),
        true,
    );
    fs::write(dumps.join("sub/broken.ljbc"), b"this is not a dump").unwrap();

    let output = temp.text("out");
    let run_ = run(&["--input", &dumps.to_string_lossy(), "--output", &output]);
    assert!(!run_.succeeded());

    // A dump that failed has no output path to be named by, so it is named by
    // where it was read from.
    let lines: Vec<&str> = run_.stderr.lines().collect();
    let mut announced: Vec<&str> = Vec::new();
    for line in lines.iter().take(2) {
        let (_, name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.split_once("] "))
            .unwrap_or_else(|| panic!("not a progress line: {line}"));
        announced.push(name);
    }
    announced.sort_unstable();
    assert_eq!(announced, ["good.lua", "sub/broken.ljbc"]);

    assert_eq!(
        lines.last(),
        Some(&"2 files processed: 1 successful, 0 partial, 1 failed")
    );
}

#[test]
fn the_options_reach_the_writer() {
    let luajit = luajit!();
    let temp = Temp::new("options");
    let dump = temp.join("chunk.ljbc");
    compile(&luajit, temp.path(), "chunk.lua", &dump, true);

    let tabs = run(&["--input", &dump.to_string_lossy()]);
    assert!(tabs.stdout.contains("\n\treturn a + b"), "{}", tabs.stdout);

    let spaces = run(&[
        "--input",
        &dump.to_string_lossy(),
        "--indent",
        "spaces",
        "--indent-width",
        "2",
    ]);
    assert!(
        spaces.stdout.contains("\n  return a + b"),
        "{}",
        spaces.stdout
    );
    assert!(!spaces.stdout.contains('\t'), "{}", spaces.stdout);

    // The width only means something for spaces.
    let refused = run(&["--input", &dump.to_string_lossy(), "--indent-width", "2"]);
    assert!(!refused.succeeded());
    assert!(
        refused.stderr.contains("--indent spaces"),
        "{}",
        refused.stderr
    );
}
