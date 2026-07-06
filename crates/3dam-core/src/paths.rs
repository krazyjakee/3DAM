//! Default on-disk locations (tech-spec 02 §9), resolved via the platform data dir.

use std::path::PathBuf;

/// The default data directory holding `library.db` (and later `server.db`). Overridable by the
/// caller; this is only the platform default.
pub fn default_data_dir() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("", "", "3dam") {
        dirs.data_dir().to_path_buf()
    } else {
        PathBuf::from(".").join("3dam-data")
    }
}
