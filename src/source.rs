use std::{path::PathBuf, process::Command, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use url::Url;

use crate::{
    model::{SearchItem, SearchKind},
    process::output_limited,
};

pub const MAX_COLLECTION_ITEMS: usize = 500;
const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 256 * 1024;
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(90);

pub trait SourceProvider: Send + Sync {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchItem>>;
    fn resolve(&self, url: &str) -> Result<Vec<SearchItem>>;
}

#[derive(Debug, Clone)]
pub struct YouTubeProvider {
    executable: PathBuf,
}

impl YouTubeProvider {
    pub fn new(executable: PathBuf) -> Self {
        Self { executable }
    }

    fn extract(&self, input: &str, limit: usize) -> Result<Value> {
        let mut command = Command::new(&self.executable);
        command.args([
            "--ignore-config",
            "--dump-single-json",
            "--flat-playlist",
            "--skip-download",
            "--no-warnings",
            "--socket-timeout",
            "10",
            "--retries",
            "2",
            "--extractor-retries",
            "2",
            "--playlist-end",
            &limit.clamp(1, MAX_COLLECTION_ITEMS).to_string(),
            "--",
            input,
        ]);
        let output = output_limited(command, EXTRACT_TIMEOUT, MAX_JSON_BYTES, MAX_ERROR_BYTES)
            .with_context(|| format!("falha ao executar {}", self.executable.display()))?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr);
            bail!("yt-dlp: {}", message.trim());
        }
        serde_json::from_slice(&output.stdout).context("yt-dlp retornou JSON invalido")
    }
}

impl SourceProvider for YouTubeProvider {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchItem>> {
        let query = query.trim();
        if query.is_empty() {
            bail!("digite algo para buscar");
        }
        if is_http_url(query) {
            return self.resolve(query);
        }
        let limit = limit.clamp(1, 50);
        let input = format!("ytsearch{limit}:{query}");
        normalize(self.extract(&input, limit)?)
    }

    fn resolve(&self, url: &str) -> Result<Vec<SearchItem>> {
        if !is_youtube_url(url) {
            bail!("use uma URL publica do YouTube ou YouTube Music");
        }
        normalize(self.extract(url, MAX_COLLECTION_ITEMS)?)
    }
}

pub fn is_http_url(value: &str) -> bool {
    Url::parse(value)
        .map(|url| matches!(url.scheme(), "http" | "https"))
        .unwrap_or(false)
}

pub fn is_youtube_url(value: &str) -> bool {
    Url::parse(value)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .map(|host| host == "youtu.be" || host == "youtube.com" || host.ends_with(".youtube.com"))
        .unwrap_or(false)
}

fn normalize(root: Value) -> Result<Vec<SearchItem>> {
    let mut items = Vec::new();
    if let Some(entries) = root.get("entries").and_then(Value::as_array) {
        for entry in entries.iter().filter(|entry| !entry.is_null()) {
            if let Some(item) = normalize_item(entry) {
                items.push(item);
            }
        }
    } else if let Some(item) = normalize_item(&root) {
        items.push(item);
    }
    if items.is_empty() {
        bail!("nenhum resultado encontrado");
    }
    items.truncate(MAX_COLLECTION_ITEMS);
    Ok(items)
}

fn normalize_item(value: &Value) -> Option<SearchItem> {
    let source_id = bounded(text(value, "id")?, 128);
    let title = bounded(text(value, "title").unwrap_or("Sem titulo"), 512);
    let artist = bounded(
        text(value, "artist")
            .or_else(|| text(value, "uploader"))
            .or_else(|| text(value, "channel"))
            .unwrap_or("Desconhecido"),
        256,
    );
    let album = text(value, "album").map(|album| bounded(album, 512));
    let entry_type = text(value, "_type").unwrap_or_default();
    let ie_key = text(value, "ie_key").unwrap_or_default();
    let has_entries = value.get("entries").and_then(Value::as_array).is_some();
    let kind = if album.is_some() && (has_entries || ie_key.contains("Tab")) {
        SearchKind::Album
    } else if has_entries || entry_type == "playlist" || ie_key.contains("Tab") {
        SearchKind::Playlist
    } else {
        SearchKind::Track
    };
    let url = text(value, "webpage_url")
        .or_else(|| text(value, "original_url"))
        .or_else(|| text(value, "url").filter(|url| is_http_url(url)))
        .map(|url| bounded(url, 4096))
        .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={source_id}"));
    let duration_seconds = value
        .get("duration")
        .and_then(Value::as_f64)
        .map(|duration| duration.max(0.0) as u64);
    Some(SearchItem {
        provider: "youtube".into(),
        source_id,
        kind,
        title,
        artist,
        album,
        duration_seconds,
        url,
    })
}

fn bounded(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_only_http_urls() {
        assert!(is_http_url("https://music.youtube.com/watch?v=abc"));
        assert!(!is_http_url("file:///etc/passwd"));
        assert!(!is_http_url("artist song"));
    }

    #[test]
    fn accepts_only_youtube_hosts() {
        assert!(is_youtube_url("https://music.youtube.com/watch?v=abc"));
        assert!(is_youtube_url("https://youtu.be/abc"));
        assert!(!is_youtube_url("https://example.com/watch?v=abc"));
        assert!(!is_youtube_url("file:///etc/passwd"));
    }

    #[test]
    fn normalizes_flat_search_results() {
        let payload = serde_json::json!({
            "entries": [{
                "id": "abc",
                "title": "Song",
                "artist": "Artist",
                "duration": 123.4,
                "url": "https://www.youtube.com/watch?v=abc"
            }]
        });
        let items = normalize(payload).unwrap();
        assert_eq!(items[0].title, "Song");
        assert_eq!(items[0].duration_seconds, Some(123));
        assert_eq!(items[0].kind, SearchKind::Track);
    }

    #[test]
    fn identifies_collections() {
        let payload = serde_json::json!({
            "id": "PL123",
            "title": "Album",
            "album": "Album",
            "entries": []
        });
        let item = normalize_item(&payload).unwrap();
        assert_eq!(item.kind, SearchKind::Album);
    }
}
