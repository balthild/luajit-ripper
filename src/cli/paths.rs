//! Where the output goes.
//!
//! The rules are deliberately strict, so that a typo in a path fails right away
//! instead of scattering files somewhere unexpected:
//!
//! * A single file input either goes to stdout or to a single file. An output
//!   that is a directory, or whose parent does not exist, is refused: folders
//!   are never created for a single file.
//! * A directory input needs an output directory. One is created when it is
//!   missing, but only when its parent already exists, so the tool never
//!   invents a chain of folders. Everything below the output directory is fair
//!   game and is created as needed.

use std::path::{Component, Path, PathBuf};
use std::{fs, io};

use walkdir::WalkDir;

use crate::cli::Error;

// MARK: job

/// Extension of the dumps this tool reads.
pub const DUMP_EXTENSION: &str = "ljbc";
/// Extension given to the decompiled output.
pub const SOURCE_EXTENSION: &str = "lua";

/// A resolved command line: one input and where its output goes.
#[derive(Debug)]
pub struct Job {
    /// The dump file or directory given on the command line.
    pub input: PathBuf,
    /// Where the source is written.
    pub sink: Sink,
}

/// Where the decompiled source is written.
#[derive(Debug)]
pub enum Sink {
    /// The source goes to stdout, which only a single file may do.
    Stdout,
    /// The source of the one input file goes to this path.
    File(PathBuf),
    /// Every dump below the input directory is written below a directory.
    Tree(Tree),
}

/// An output directory.
#[derive(Debug)]
pub struct Tree {
    /// Directory the output is written below.
    pub root: PathBuf,
    /// Whether output paths are taken from the module path in the dump header
    /// instead of from the location of the input file.
    pub module_structure: bool,
    /// Whether a dump whose source is already there and as old as the dump is
    /// left alone instead of being decompiled again.
    pub incremental: bool,
}

/// Where one dump is written.
#[derive(Debug)]
pub struct Target {
    /// Path to write the source to.
    pub path: PathBuf,
    /// Whether `path` comes from the chunk name rather than the input location.
    pub from_module: bool,
}

// MARK: resolving paths

/// Works out where a run reads from and writes to.
///
/// This creates the output directory when it needs one, and reports a path that
/// cannot be used without touching the filesystem.
pub fn resolve(
    input: &Path,
    output: Option<&Path>,
    module_structure: bool,
    incremental: bool,
) -> Result<Job, Error> {
    let metadata = fs::metadata(input).map_err(|source| io_error(input, source))?;

    if metadata.is_dir() {
        let Some(output) = output else {
            return Err(Error::Layout(format!(
                "--input {} is a directory, so --output is required",
                input.display()
            )));
        };
        let root = prepare_root(output)?;
        Ok(Job {
            input: input.to_path_buf(),
            sink: Sink::Tree(Tree {
                root,
                module_structure,
                incremental,
            }),
        })
    } else {
        if module_structure {
            return Err(Error::Layout(String::from(
                "--module-structure needs a directory as --input",
            )));
        }
        if incremental {
            // A single input is one output, written whether or not it was there
            // before: there is no tree to be incremental about.
            return Err(Error::Layout(String::from(
                "--incremental needs a directory as --input",
            )));
        }
        let Some(output) = output else {
            return Ok(Job {
                input: input.to_path_buf(),
                sink: Sink::Stdout,
            });
        };
        // A single input is one output. Accepting a directory here would mean
        // deciding a file name for it, and there is no name to decide on.
        if output.is_dir() {
            return Err(Error::Layout(format!(
                "--input {} is a single file, so --output {} must be a file too, \
                 or be left out to write to stdout",
                input.display(),
                output.display()
            )));
        }
        // The parent has to be there already: a file output is not a reason to
        // start making directories.
        let parent = parent_of(output);
        if !parent.is_dir() {
            return Err(Error::Layout(format!(
                "the directory {} of --output {} does not exist",
                parent.display(),
                output.display()
            )));
        }
        Ok(Job {
            input: input.to_path_buf(),
            sink: Sink::File(output.to_path_buf()),
        })
    }
}

/// Checks the output directory of a tree, creating it when it is missing.
fn prepare_root(output: &Path) -> Result<PathBuf, Error> {
    match fs::metadata(output) {
        Ok(metadata) if metadata.is_dir() => Ok(output.to_path_buf()),
        Ok(_) => Err(Error::Layout(format!(
            "--output {} is not a directory",
            output.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Exactly one level is created. `create_dir_all` would also make
            // the parents, which would turn a typo in a long path into a brand
            // new tree of directories.
            let parent = parent_of(output);
            if !parent.is_dir() {
                return Err(Error::Layout(format!(
                    "the directory {} of --output {} does not exist, and it is not created \
                     for you: make it first, or point --output one level deeper",
                    parent.display(),
                    output.display()
                )));
            }
            fs::create_dir(output).map_err(|source| io_error(output, source))?;
            Ok(output.to_path_buf())
        }
        Err(source) => Err(io_error(output, source)),
    }
}

/// Every dump below `input`, in path order.
///
/// Subdirectories are searched; symbolic links are not followed, so a link
/// pointing back up a tree cannot make the walk loop.
pub fn dumps(input: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut files = Vec::new();
    for entry in WalkDir::new(input).follow_links(false) {
        let entry = entry.map_err(|source| Error::Walk {
            path: input.to_path_buf(),
            source,
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == DUMP_EXTENSION)
        {
            files.push(entry.into_path());
        }
    }
    files.sort();
    Ok(files)
}

// MARK: targets

impl Tree {
    /// Where the dump `file` is written, given the chunk name it carries.
    ///
    /// `input` is the directory the walk started from, which is what the
    /// fallback mirrors.
    pub fn target(&self, input: &Path, file: &Path, chunk_name: Option<&str>) -> Target {
        if self.module_structure
            && let Some(relative) = chunk_name.and_then(module_relative_path)
        {
            return Target {
                path: self.root.join(relative),
                from_module: true,
            };
        }
        Target {
            path: self.root.join(mirrored(input, file)),
            from_module: false,
        }
    }

    /// Creates the directories `target` needs, below the output root.
    ///
    /// `mkdir -p` is used here: the output root itself is already in place, so
    /// everything created is a subdirectory of a directory this run was asked
    /// to fill in.
    pub fn prepare(&self, target: &Path) -> Result<(), Error> {
        let parent = parent_of(target);
        if parent != self.root {
            fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        }
        Ok(())
    }

    /// Where `file` would be written, when that is where it already is.
    ///
    /// `None` means there is work to do: either `--incremental` was not asked
    /// for, or the source is missing, out of date, or cannot be looked at. The
    /// target is worked out exactly as it is for a write, so the two cannot
    /// disagree about where the source belongs.
    pub fn skip(&self, input: &Path, file: &Path, chunk_name: Option<&str>) -> Option<Target> {
        if !self.incremental {
            return None;
        }
        let target = self.target(input, file, chunk_name);
        unchanged(&target.path, file).then_some(target)
    }
}

// MARK: stamps

/// Whether `source` was written at the very moment `dump` was made.
///
/// This is what `--incremental` asks, and it is not the same question as "is the
/// source at least as new as its dump": a source someone edited carries the
/// timestamp of the edit, and a dump compiled again carries the timestamp of the
/// compile, so both are out of date — but so is a source rebuilt an hour later
/// that happens to be newer. Only the same moment means the source is the one
/// this dump produced, which is why [`stamp`] puts that moment there.
///
/// Anything that cannot be looked at, and anything that is not a plain file,
/// counts as out of date, so that the run does the work and says whatever is
/// wrong with the dump or with where its source was meant to go, rather than
/// quietly skipping it.
pub fn unchanged(source: &Path, dump: &Path) -> bool {
    let Ok(source) = fs::metadata(source) else {
        return false;
    };
    if !source.is_file() {
        return false;
    }
    let Ok(dump) = fs::metadata(dump) else {
        return false;
    };
    source
        .modified()
        .ok()
        .zip(dump.modified().ok())
        .is_some_and(|(source, dump)| source == dump)
}

/// Gives a source the modification time of the dump it was written from.
///
/// Without this, [`unchanged`] could never be true: a source is written after
/// the dump it came from is read, so its own timestamp is always later than the
/// dump's, and an equality test would say "out of date" for a source that the
/// run has just produced. Copying the time over is what makes the timestamp mean
/// which dump the source belongs to rather than when it happened to be written,
/// and so what lets a second run tell a source that is still current from one
/// whose dump has been compiled again since. A source touched by hand gets a
/// newer time of its own, so it is out of date as well, which is what makes it
/// worth writing again.
pub fn stamp(source: &Path, dump: &Path) -> Result<(), Error> {
    let time = fs::metadata(dump)
        .and_then(|metadata| metadata.modified())
        .map_err(|source| io_error(dump, source))?;

    // The file is opened for writing because not every platform lets the times
    // of a handle that only reads be changed.
    fs::OpenOptions::new()
        .write(true)
        .open(source)
        .and_then(|file| file.set_modified(time))
        .map_err(|error| io_error(source, error))
}

// MARK: module paths

/// Turns the chunk name of a dump into a path relative to the output root.
///
/// Returns `None` when the name does not describe a place to write: a stripped
/// dump has no name at all, `=` marks a literal name rather than a file, and a
/// name that is empty, absolute, or climbs out of the output directory is
/// refused so that a crafted dump cannot write where it likes.
///
/// The leading `@` is kept as part of the first component, which is what the
/// dumps of a real application look like (`@modules/logic/...`).
pub fn module_relative_path(chunk_name: &str) -> Option<PathBuf> {
    if !chunk_name.starts_with('@') {
        return None;
    }

    // The name is read as a path so that it is split the way the machine that
    // made the dump splits paths: on Windows a `\` in the name is a separator
    // rather than part of a file name, which is what lets a `..` hidden behind
    // one be seen for what it is.
    let mut path = PathBuf::new();
    for component in Path::new(chunk_name).components() {
        // Only a plain name may decide where we write: a root, a prefix or a
        // parent is refused rather than followed.
        let Component::Normal(part) = component else {
            return None;
        };
        path.push(part);
    }
    if path.as_os_str().is_empty() {
        return None;
    }

    // `set_extension` leaves a name that already ends in `.lua` alone.
    path.set_extension(SOURCE_EXTENSION);
    Some(path)
}

/// The output location of `file` when the input tree is being mirrored.
fn mirrored(root: &Path, file: &Path) -> PathBuf {
    let mut path = PathBuf::from(file.strip_prefix(root).unwrap_or(file));
    path.set_extension(SOURCE_EXTENSION);
    path
}

/// The directory a path lives in, with an empty path standing for `.`.
fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn io_error(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

// MARK: tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_paths_keep_the_leading_at() {
        assert_eq!(
            module_relative_path("@modules/logic/rouge/map/Foo.lua"),
            Some(PathBuf::from("@modules/logic/rouge/map/Foo.lua"))
        );
    }

    #[test]
    fn module_paths_gain_the_lua_extension() {
        assert_eq!(
            module_relative_path("@modules/foo"),
            Some(PathBuf::from("@modules/foo.lua"))
        );
        assert_eq!(
            module_relative_path("@modules/foo.ljbc"),
            Some(PathBuf::from("@modules/foo.lua"))
        );
    }

    #[test]
    fn names_that_are_not_paths_are_refused() {
        // A stripped dump, and a name LuaJIT marks as a literal rather than a
        // file.
        assert_eq!(module_relative_path("=?"), None);
        assert_eq!(module_relative_path("=main"), None);
        assert_eq!(module_relative_path(""), None);
    }

    #[test]
    fn names_that_climb_out_are_refused() {
        assert_eq!(module_relative_path("@modules/../../outside.lua"), None);
        assert_eq!(module_relative_path("@modules/../outside.lua"), None);
    }

    #[test]
    fn redundant_separators_are_normalised_away() {
        // An empty step and a `.` step cannot leave the output root, so they
        // are folded into the plain names around them rather than refused.
        assert_eq!(
            module_relative_path("@modules//twin.lua"),
            Some(PathBuf::from("@modules/twin.lua"))
        );
        assert_eq!(
            module_relative_path("@modules/./here.lua"),
            Some(PathBuf::from("@modules/here.lua"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_backslash_is_a_separator_on_windows() {
        assert_eq!(
            module_relative_path("@modules\\logic\\Foo.lua"),
            Some(PathBuf::from("@modules/logic/Foo.lua"))
        );
        // A parent spelled with the Windows separator still climbs out.
        assert_eq!(module_relative_path("@modules\\..\\..\\outside.lua"), None);
    }

    #[test]
    fn odd_names_still_stay_inside_the_output_root() {
        // `@..` and `@` are names in their own right rather than a step up or
        // the root, so they are taken as the directories they look like. What
        // matters is that neither of them turns into a path that leaves the
        // output root, whatever a dump claims.
        let root = Path::new("/out");
        for name in ["@../outside.lua", "@/absolute.lua", "@modules/a.lua"] {
            let relative = module_relative_path(name).expect("a usable name");
            assert!(relative.is_relative(), "{relative:?}");
            assert!(
                relative
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
                "{relative:?}"
            );
            let target = root.join(&relative);
            assert!(target.starts_with(root), "{target:?}");
        }
    }

    #[test]
    fn mirrored_paths_below_the_root_keep_their_shape() {
        let root = Path::new("/dumps");
        assert_eq!(
            mirrored(root, Path::new("/dumps/a/b/c.ljbc")),
            PathBuf::from("a/b/c.lua")
        );
        // A file outside the root cannot be stripped, so it is used as it is.
        assert_eq!(
            mirrored(root, Path::new("elsewhere/c.ljbc")),
            PathBuf::from("elsewhere/c.lua")
        );
    }
}
