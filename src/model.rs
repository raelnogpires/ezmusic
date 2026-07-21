use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    Track,
    Album,
    Playlist,
    Unknown,
}

impl SearchKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Track => "faixa",
            Self::Album => "album",
            Self::Playlist => "playlist",
            Self::Unknown => "item",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchItem {
    pub provider: String,
    pub source_id: String,
    pub kind: SearchKind,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_seconds: Option<u64>,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub id: i64,
    pub provider: Option<String>,
    pub source_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub path: PathBuf,
    pub duration_seconds: Option<u64>,
    pub available: bool,
    pub imported: bool,
}

#[derive(Debug, Clone)]
pub struct TrackDraft {
    pub provider: Option<String>,
    pub source_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub path: PathBuf,
    pub duration_seconds: Option<u64>,
    pub imported: bool,
}

#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub job_id: String,
    pub item: SearchItem,
}

#[derive(Debug, Clone)]
pub enum DownloadEvent {
    Queued { job_id: String, title: String },
    Downloading { job_id: String },
    Converting { job_id: String },
    Completed { job_id: String, track: TrackDraft },
    Cancelled { job_id: String },
    Failed { job_id: String, error: String },
}

impl DownloadEvent {
    pub fn job_id(&self) -> &str {
        match self {
            Self::Queued { job_id, .. }
            | Self::Downloading { job_id }
            | Self::Converting { job_id }
            | Self::Completed { job_id, .. }
            | Self::Cancelled { job_id }
            | Self::Failed { job_id, .. } => job_id,
        }
    }
}
