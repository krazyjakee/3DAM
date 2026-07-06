//! `dam-sources` — the `Source` trait and its implementations (tech-spec 07).
//!
//! Phase 1 ships the **local filesystem** file source: it walks a directory tree and yields file
//! entries the engine hashes and catalogues. SFTP/SMB (file sources) and the federated peer source
//! land later behind the same trait.

use dam_api::LibError;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// One file discovered by a file source.
#[derive(Clone, Debug)]
pub struct FileEntry {
    /// Path relative to the source root (stored as `asset.path`).
    pub rel_path: String,
    /// Absolute path on this host (used to read bytes; never persisted).
    pub abs_path: PathBuf,
    pub size: u64,
    pub modified_ms: Option<i64>,
}

/// A file source yields raw file entries the engine processes locally (tech-spec 07).
pub trait FileSource: Send + Sync {
    /// Walk the source, invoking `sink` per discovered file. Fail-soft: a per-entry error is
    /// reported to `sink` as `Err` and iteration continues.
    fn walk(
        &self,
        sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
    ) -> Result<(), LibError>;
}

/// A local directory tree.
pub struct LocalFsSource {
    root: PathBuf,
}

impl LocalFsSource {
    pub fn new(root: impl Into<PathBuf>) -> LocalFsSource {
        LocalFsSource { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl FileSource for LocalFsSource {
    fn walk(
        &self,
        sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
    ) -> Result<(), LibError> {
        if !self.root.exists() {
            return Err(LibError::SourceUnavailable(format!(
                "path does not exist: {}",
                self.root.display()
            )));
        }
        for entry in walkdir::WalkDir::new(&self.root).follow_links(false) {
            let cont = match entry {
                Ok(de) if de.file_type().is_file() => {
                    let abs = de.path().to_path_buf();
                    let rel = abs
                        .strip_prefix(&self.root)
                        .unwrap_or(&abs)
                        .to_string_lossy()
                        .into_owned();
                    let (size, modified_ms) = match de.metadata() {
                        Ok(m) => (
                            m.len(),
                            m.modified()
                                .ok()
                                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                                .map(|d| d.as_millis() as i64),
                        ),
                        Err(_) => (0, None),
                    };
                    sink(Ok(FileEntry {
                        rel_path: rel,
                        abs_path: abs,
                        size,
                        modified_ms,
                    }))
                }
                Ok(_) => true, // directories/symlinks: skip, keep going
                Err(e) => sink(Err(LibError::Internal(e.to_string()))),
            };
            if !cont {
                break; // sink asked to stop (cancellation)
            }
        }
        Ok(())
    }
}
