//! A test's scratch directory, removed with everything in it when the
//! test drops it.

use std::ops::Deref;
use std::path::Path;

/// A fresh directory under the system temp dir. It reads as a `Path`,
/// so `dir.join(…)` and `&dir` work as on a `PathBuf`. Keep it alive
/// for as long as the test uses the directory: bind it to a named
/// variable, such as `_dir`, not to `_`.
pub(crate) struct TestDir(tempfile::TempDir);

impl TestDir {
    /// A new directory whose name starts with `prefix`.
    pub(crate) fn new(prefix: &str) -> Self {
        Self(
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir()
                .expect("create a test dir"),
        )
    }
}

impl Deref for TestDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.0.path()
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        self.0.path()
    }
}
