//! The command line tool.
//!
//! The tool is a thin driver around the library: it works out where every
//! output goes, decompiles the dumps on a thread pool, and reports what
//! happened. Nothing here is meant to be used from the library.

pub mod allocator;
pub mod paths;
pub mod run;

use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// A failure that stops the whole run.
#[derive(Debug, Error)]
pub enum Error {
    /// The command line does not describe a run that can be made.
    #[error("{0}")]
    Layout(String),

    /// A path could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// Path that was being used.
        path: PathBuf,
        /// What went wrong.
        source: io::Error,
    },

    /// The one dump of the run could not be decompiled.
    #[error("{path}: {reason}")]
    Failed {
        /// Dump that failed.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },

    /// The input directory could not be walked.
    #[error("cannot walk {path}: {source}")]
    Walk {
        /// Directory that was being walked.
        path: PathBuf,
        /// What went wrong.
        source: walkdir::Error,
    },

    /// The thread pool could not be created.
    #[error("cannot start the thread pool: {0}")]
    Pool(#[source] rayon::ThreadPoolBuildError),

    /// The source could not be written to stdout.
    #[error("cannot write to stdout: {0}")]
    Stdout(io::Error),
}

/// A failure for one dump.
///
/// A dump that cannot be handled does not stop the run: it is left out and
/// counted, so that one bad file in a tree of thousands does not hide the rest.
#[derive(Debug, Error)]
pub enum Failure {
    /// The dump could not be read.
    #[error("cannot read the dump: {0}")]
    Read(#[source] io::Error),

    /// The dump could not be decompiled.
    #[error("{0}")]
    Decompile(#[source] luajit_ripper::Error),

    /// The output path could not be prepared or written.
    #[error("{0}")]
    Output(#[from] Error),
}
