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

/// Where remote fetches materialise their bytes (issue #87).
///
/// Under the data dir rather than `std::env::temp_dir()`, because the latter is a tmpfs on most
/// Linux hosts — RAM backed by swap — and a remote video can be gigabytes. The data dir is real
/// disk the user chose, and it is already where every other derivative lives.
///
/// Its contents are pure scratch: [`dam_sources::clean_scratch`] empties it at engine open, and
/// nothing in it is ever authoritative.
pub fn scratch_dir(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("scratch")
}
