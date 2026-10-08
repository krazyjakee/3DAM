//! Desktop updater IPC contract. Updates belong to the installed shell, not LibraryService.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    Installing,
    Ready,
    Error,
}

impl UpdatePhase {
    pub fn busy(self) -> bool {
        matches!(self, Self::Checking | Self::Downloading | Self::Installing)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DesktopRelease {
    pub version: String,
    pub notes: Option<String>,
    pub date: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DesktopUpdateStatus {
    pub current_version: String,
    pub supported: bool,
    pub unavailable_reason: Option<String>,
    pub automatic_checks: bool,
    pub phase: UpdatePhase,
    pub release: Option<DesktopRelease>,
    pub checked_at: Option<i64>,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub error: Option<String>,
}
