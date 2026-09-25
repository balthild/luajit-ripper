//! Extension queries on paths.

use std::ffi::OsStr;
use std::path::Path;

/// Extra queries on [`Path`].
pub trait PathExt {
    /// Whether the path ends in `extension`.
    ///
    /// This is [`Path::extension`] with the missing-extension case folded in,
    /// so that `path.extension().is_some_and(|ext| ext == "lua")` is written
    /// once here rather than at every call site. The comparison is against the
    /// extension on its own, so the leading `.` is not part of `extension`.
    ///
    /// As with [`Path::extension`], a name that is only an extension
    /// (`.gitignore`) has none: the file name as a whole is its stem there,
    /// not an extension.
    fn has_extension(&self, extension: impl AsRef<OsStr>) -> bool;
}

impl PathExt for Path {
    fn has_extension(&self, extension: impl AsRef<OsStr>) -> bool {
        self.extension()
            .is_some_and(|own| own == extension.as_ref())
    }
}

// MARK: tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_extension_is_found() {
        assert!(Path::new("dump.ljbc").has_extension("ljbc"));
        assert!(Path::new("/a/b/dump.ljbc").has_extension("ljbc"));
        // An `OsStr` argument is accepted just as a string literal is.
        assert!(Path::new("dump.ljbc").has_extension(OsStr::new("ljbc")));
    }

    #[test]
    fn a_different_extension_is_not() {
        assert!(!Path::new("dump.ljbc").has_extension("lua"));
    }

    #[test]
    fn a_missing_extension_is_not() {
        assert!(!Path::new("dump").has_extension("ljbc"));
        // A leading dot makes the whole name the stem, so there is no
        // extension to compare against.
        assert!(!Path::new(".gitignore").has_extension("gitignore"));
    }

    #[test]
    fn only_the_last_extension_counts() {
        assert!(Path::new("archive.tar.gz").has_extension("gz"));
        assert!(!Path::new("archive.tar.gz").has_extension("tar"));
    }
}
