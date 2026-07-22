use std::{path::PathBuf, process::Command, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use url::Url;

use crate::{
    model::{AlbumDraft, MediaSegment, SearchItem, SearchKind},
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

    fn extract(&self, input: &str, limit: usize, flat: bool) -> Result<Value> {
        let mut command = Command::new(&self.executable);
        command.args([
            "--ignore-config",
            "--dump-single-json",
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
        ]);
        if flat {
            command.arg("--flat-playlist");
        } else {
            command.arg("--no-flat-playlist");
        }
        command.args(["--", input]);
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
        normalize(
            self.extract(&format!("ytsearch{limit}:{query}"), limit, true)?,
            false,
        )
    }

    fn resolve(&self, url: &str) -> Result<Vec<SearchItem>> {
        if !is_youtube_url(url) {
            bail!("use uma URL publica do YouTube ou YouTube Music");
        }
        normalize(self.extract(url, MAX_COLLECTION_ITEMS, false)?, true)
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

fn normalize(root: Value, inspect_video: bool) -> Result<Vec<SearchItem>> {
    if inspect_video
        && root.get("entries").and_then(Value::as_array).is_none()
        && let Some(items) = normalize_chapters(&root)
    {
        return Ok(items);
    }

    let mut items = Vec::new();
    if let Some(entries) = root.get("entries").and_then(Value::as_array) {
        let album = collection_album(&root);
        for entry in entries.iter().filter(|entry| !entry.is_null()) {
            if let Some(mut item) = normalize_item(entry) {
                if let Some(album) = &album {
                    item.album = Some(album.title.clone());
                    item.album_identity = Some(album.clone());
                }
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
    } else if has_album_marker(&title) {
        SearchKind::Unknown
    } else {
        SearchKind::Track
    };
    let url = text(value, "webpage_url")
        .or_else(|| text(value, "original_url"))
        .or_else(|| text(value, "url").filter(|url| is_http_url(url)))
        .map(|url| bounded(url, 4096))
        .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={source_id}"));
    let duration_seconds = finite_number(value, "duration").map(|duration| duration as u64);
    Some(SearchItem {
        provider: "youtube".into(),
        source_id,
        kind,
        title,
        artist,
        album,
        duration_seconds,
        url,
        segment: None,
        album_identity: None,
    })
}

fn collection_album(root: &Value) -> Option<AlbumDraft> {
    let item = normalize_item(root)?;
    (item.kind == SearchKind::Album).then(|| AlbumDraft {
        provider: item.provider,
        source_id: item.source_id,
        title: item.album.unwrap_or(item.title),
        artist: item.artist,
    })
}

fn normalize_chapters(root: &Value) -> Option<Vec<SearchItem>> {
    let base = normalize_item(root)?;
    let duration = finite_number(root, "duration")?;
    if duration <= 0.0 {
        return None;
    }
    let chapters = match root.get("chapters").and_then(Value::as_array) {
        Some(chapters) if !chapters.is_empty() => structured_chapters(chapters, duration)?,
        _ => timestamp_chapters(text(root, "description")?, duration)?,
    };
    if chapters.len() < 2 || chapters.len() > MAX_COLLECTION_ITEMS {
        return None;
    }
    let album = AlbumDraft {
        provider: base.provider.clone(),
        source_id: base.source_id.clone(),
        title: text(root, "album")
            .map(|title| bounded(title, 512))
            .unwrap_or_else(|| normalize_album_title(&base.title)),
        artist: bounded(
            text(root, "album_artist")
                .or_else(|| text(root, "artist"))
                .or_else(|| text(root, "uploader"))
                .or_else(|| text(root, "channel"))
                .unwrap_or(&base.artist),
            256,
        ),
    };
    Some(
        chapters
            .into_iter()
            .enumerate()
            .map(|(index, (title, start, end))| SearchItem {
                provider: base.provider.clone(),
                source_id: chapter_source_id(&base.source_id, index, start),
                kind: SearchKind::Track,
                title: bounded(&title, 512),
                artist: album.artist.clone(),
                album: Some(album.title.clone()),
                duration_seconds: Some((end - start).round() as u64),
                url: base.url.clone(),
                segment: Some(MediaSegment {
                    start_seconds: start,
                    end_seconds: end,
                    position: (index + 1) as u32,
                }),
                album_identity: Some(album.clone()),
            })
            .collect(),
    )
}

fn structured_chapters(chapters: &[Value], duration: f64) -> Option<Vec<(String, f64, f64)>> {
    if chapters.len() < 2 || chapters.len() > MAX_COLLECTION_ITEMS {
        return None;
    }
    let mut starts = Vec::with_capacity(chapters.len());
    for chapter in chapters {
        let title = text(chapter, "title")?.trim();
        let start = finite_number(chapter, "start_time")?;
        if title.is_empty() || start < 0.0 || start >= duration {
            return None;
        }
        starts.push((title.to_string(), start, finite_number(chapter, "end_time")));
    }
    build_segments(starts, duration, true)
}

fn timestamp_chapters(description: &str, duration: f64) -> Option<Vec<(String, f64, f64)>> {
    let mut starts = Vec::new();
    for line in description.lines() {
        let line = line.trim();
        let Some((stamp, title)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let stamp = stamp.trim_matches(['[', ']', '(', ')']);
        if let Some(start) = parse_timestamp(stamp) {
            if title.trim().is_empty() {
                return None;
            }
            starts.push((title.trim().to_string(), start, None));
        } else if stamp.contains(':') && stamp.chars().any(|character| character.is_ascii_digit()) {
            return None;
        }
    }
    if starts.first().map(|(_, start, _)| *start) != Some(0.0) {
        return None;
    }
    build_segments(starts, duration, false)
}

fn build_segments(
    starts: Vec<(String, f64, Option<f64>)>,
    duration: f64,
    validate_ends: bool,
) -> Option<Vec<(String, f64, f64)>> {
    if starts.len() < 2 || starts.len() > MAX_COLLECTION_ITEMS {
        return None;
    }
    let mut output = Vec::with_capacity(starts.len());
    for (index, (title, start, declared_end)) in starts.iter().enumerate() {
        if !start.is_finite()
            || *start < 0.0
            || *start >= duration
            || index > 0 && *start <= starts[index - 1].1
        {
            return None;
        }
        let next = starts
            .get(index + 1)
            .map(|entry| entry.1)
            .unwrap_or(duration);
        let end = declared_end.unwrap_or(next);
        if !end.is_finite() || end <= *start || end > duration || validate_ends && end > next {
            return None;
        }
        output.push((title.clone(), *start, end));
    }
    Some(output)
}

fn parse_timestamp(value: &str) -> Option<f64> {
    let parts: Vec<_> = value.split(':').collect();
    let numbers: Vec<u64> = parts
        .iter()
        .map(|part| part.parse::<u64>().ok())
        .collect::<Option<_>>()?;
    match numbers.as_slice() {
        [minutes, seconds] if *seconds < 60 => Some((minutes * 60 + seconds) as f64),
        [hours, minutes, seconds] if *minutes < 60 && *seconds < 60 => {
            Some((hours * 3600 + minutes * 60 + seconds) as f64)
        }
        _ => None,
    }
}

fn chapter_source_id(video_id: &str, index: usize, start: f64) -> String {
    format!(
        "{}-{:016x}-chapter-{:03}-{:010}",
        bounded(video_id, 72),
        stable_hash(video_id),
        index + 1,
        (start * 1000.0).round() as u64
    )
}

fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

fn has_album_marker(title: &str) -> bool {
    let lower = title.to_lowercase();
    [
        "album completo",
        "full album",
        "soundtrack",
        "compilation",
        "mixtape",
        "album",
        "ost",
        "lp",
        "ep",
    ]
    .iter()
    .any(|marker| {
        lower.match_indices(marker).any(|(start, found)| {
            let end = start + found.len();
            lower[..start]
                .chars()
                .next_back()
                .is_none_or(|character| !character.is_alphanumeric())
                && lower[end..]
                    .chars()
                    .next()
                    .is_none_or(|character| !character.is_alphanumeric())
        })
    })
}

fn normalize_album_title(title: &str) -> String {
    let mut result = title.trim().to_string();
    for wrapper in [
        "[full album]",
        "(full album)",
        "[album completo]",
        "(album completo)",
    ] {
        if let Some(index) = result.to_lowercase().find(wrapper) {
            result.replace_range(index..index + wrapper.len(), "");
            result = result.trim_matches([' ', '-', '–', '—']).to_string();
        }
    }
    bounded(if result.is_empty() { title } else { &result }, 512)
}

fn finite_number(value: &Value, key: &str) -> Option<f64> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
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

    fn chapter_video(chapters: Value) -> Value {
        serde_json::json!({
            "id": "video",
            "title": "Artist - Record [Full Album]",
            "uploader": "Channel",
            "duration": 180.0,
            "webpage_url": "https://www.youtube.com/watch?v=video",
            "chapters": chapters
        })
    }

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
    }

    #[test]
    fn normalizes_structured_chapters_with_stable_ids_and_boundaries() {
        let payload = chapter_video(serde_json::json!([
            {"title":"One","start_time":0.0,"end_time":60.0},
            {"title":"Two","start_time":60.0,"end_time":180.0}
        ]));
        let first = normalize(payload.clone(), true).unwrap();
        let retry = normalize(payload, true).unwrap();
        assert_eq!(first, retry);
        assert!(first[0].source_id.ends_with("-chapter-001-0000000000"));
        assert_eq!(first[1].segment.as_ref().unwrap().end_seconds, 180.0);
        assert_eq!(first[0].album.as_deref(), Some("Artist - Record"));
    }

    #[test]
    fn parses_description_timestamp_index_and_calculates_ends() {
        let mut payload = chapter_video(serde_json::json!([]));
        payload.as_object_mut().unwrap().remove("chapters");
        payload["description"] = Value::String("0:00 One\n01:15 Two\n1:30:00 Three".into());
        payload["duration"] = serde_json::json!(6000.0);
        let items = normalize(payload, true).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].segment.as_ref().unwrap().end_seconds, 75.0);
        assert_eq!(items[1].segment.as_ref().unwrap().end_seconds, 5400.0);
    }

    #[test]
    fn rejects_bad_timestamp_indexes_and_title_only_album_signal() {
        for description in [
            "0:00 One\n0:00 Two",
            "0:30 One\n0:10 Two",
            "0:00 One\n9:00 Two",
            "0:00 Only",
        ] {
            let mut payload = chapter_video(serde_json::json!([]));
            payload.as_object_mut().unwrap().remove("chapters");
            payload["description"] = Value::String(description.into());
            payload["duration"] = serde_json::json!(120.0);
            let items = normalize(payload, true).unwrap();
            assert_eq!(items.len(), 1);
            assert!(items[0].album_identity.is_none());
        }
    }

    #[test]
    fn title_markers_are_boundary_aware_hints() {
        let mut hinted = normalize_item(&serde_json::json!({"id":"a","title":"My LP"})).unwrap();
        assert_eq!(hinted.kind, SearchKind::Unknown);
        hinted = normalize_item(&serde_json::json!({"id":"b","title":"Help Me"})).unwrap();
        assert_eq!(hinted.kind, SearchKind::Track);
    }

    #[test]
    fn provider_album_keeps_identity_and_order() {
        let payload = serde_json::json!({
            "id":"OLAK5uy_album", "title":"Record", "album":"Record", "artist":"Artist",
            "entries":[
                {"id":"one","title":"One"},
                {"id":"two","title":"Two"}
            ]
        });
        let items = normalize(payload, true).unwrap();
        assert_eq!(items[0].source_id, "one");
        assert_eq!(items[1].source_id, "two");
        assert_eq!(
            items[0].album_identity.as_ref().unwrap().source_id,
            "OLAK5uy_album"
        );
    }
}
