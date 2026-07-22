pub mod config;
pub mod db;
pub mod download;
pub mod model;
pub mod player;
mod process;
pub mod source;
mod storage;
pub mod tools;
pub mod tui;

pub use config::{AppConfig, AppPaths};
pub use db::LibraryDb;
pub use model::{
    Album, AlbumDraft, DownloadEvent, DownloadRequest, MediaSegment, SearchItem, SearchKind, Track,
};
