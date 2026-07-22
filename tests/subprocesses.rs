#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use ezmusic::{
    download::DownloadService,
    model::{AlbumDraft, DownloadEvent, DownloadRequest, MediaSegment, SearchItem, SearchKind},
    source::{SourceProvider, YouTubeProvider},
};
use tempfile::tempdir;

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn provider_parses_fake_ytdlp_output() {
    let directory = tempdir().unwrap();
    let ytdlp = directory.path().join("yt-dlp");
    executable(
        &ytdlp,
        r#"#!/bin/sh
last=""
for argument do
  last="$argument"
done
if [ "$last" != "ytsearch20:artist one" ]; then
  printf 'unexpected search input: %s\n' "$last" >&2
  exit 64
fi
printf '%s\n' '{"entries":[{"id":"one","title":"One","artist":"Artist","duration":60,"webpage_url":"https://www.youtube.com/watch?v=one"}]}'
"#,
    );
    let results = YouTubeProvider::new(ytdlp)
        .search("artist one", 20)
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "One");
}

#[test]
fn provider_resolves_every_track_in_a_collection() {
    let directory = tempdir().unwrap();
    let ytdlp = directory.path().join("yt-dlp");
    executable(
        &ytdlp,
        r#"#!/bin/sh
last=""
for argument do
  last="$argument"
done
if [ "$last" != "https://music.youtube.com/playlist?list=album" ]; then
  printf 'unexpected collection URL: %s\n' "$last" >&2
  exit 64
fi
printf '%s\n' '{"entries":[{"id":"one","title":"One","artist":"Artist","webpage_url":"https://www.youtube.com/watch?v=one"},{"id":"two","title":"Two","artist":"Artist","webpage_url":"https://www.youtube.com/watch?v=two"}]}'
"#,
    );

    let results = YouTubeProvider::new(ytdlp)
        .resolve("https://music.youtube.com/playlist?list=album")
        .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].source_id, "one");
    assert_eq!(results[1].source_id, "two");
}

#[test]
fn download_pipeline_publishes_only_completed_opus() {
    let directory = tempdir().unwrap();
    let ytdlp = directory.path().join("yt-dlp");
    let ffmpeg = directory.path().join("ffmpeg");
    executable(
        &ytdlp,
        r#"#!/usr/bin/env bash
set -eu
out=""
while (($#)); do
  if [[ "$1" == "-o" ]]; then
    shift
    out="$1"
  fi
  shift
done
out="${out/'%(ext)s'/webm}"
printf 'fake audio' > "$out"
"#,
    );
    executable(
        &ffmpeg,
        r#"#!/usr/bin/env bash
set -eu
if [[ "${1:-}" == "-version" ]]; then
  echo "ffmpeg fake"
  exit 0
fi
convert=false
last=""
for arg in "$@"; do
  [[ "$arg" == "-y" ]] && convert=true
  last="$arg"
done
if ! $convert; then
  echo "Audio: aac" >&2
  exit 1
fi
printf 'OggS fake opus' > "$last"
"#,
    );
    let library = directory.path().join("library");
    let cache = directory.path().join("cache");
    let service = DownloadService::start(ytdlp, ffmpeg, library.clone(), cache, 1, 160).unwrap();
    service
        .enqueue(DownloadRequest {
            job_id: "youtube-one".into(),
            items: vec![SearchItem {
                provider: "youtube".into(),
                source_id: "one".into(),
                kind: SearchKind::Track,
                title: "One".into(),
                artist: "Artist".into(),
                album: None,
                duration_seconds: Some(60),
                url: "https://www.youtube.com/watch?v=one".into(),
                segment: None,
                album_identity: None,
            }],
            album: None,
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut completed = false;
    while Instant::now() < deadline {
        for event in service.try_events() {
            match event {
                DownloadEvent::Completed { tracks, .. } => {
                    assert!(tracks[0].path.is_file());
                    assert_eq!(tracks[0].path.extension().unwrap(), "opus");
                    completed = true;
                }
                DownloadEvent::Failed { error, .. } => panic!("{error}"),
                _ => {}
            }
        }
        if completed {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(completed, "download did not complete before deadline");
    assert!(!library.join(".youtube-one.opus.part").exists());
}

#[test]
fn dropping_download_service_stops_running_children() {
    let directory = tempdir().unwrap();
    let ytdlp = directory.path().join("yt-dlp");
    let ffmpeg = directory.path().join("ffmpeg");
    executable(&ytdlp, "#!/bin/sh\nsleep 60\n");
    executable(&ffmpeg, "#!/bin/sh\necho 'ffmpeg fake'\n");
    let service = DownloadService::start(
        ytdlp,
        ffmpeg,
        directory.path().join("library"),
        directory.path().join("cache"),
        1,
        160,
    )
    .unwrap();
    service
        .enqueue(DownloadRequest {
            job_id: "youtube-slow".into(),
            items: vec![SearchItem {
                provider: "youtube".into(),
                source_id: "slow".into(),
                kind: SearchKind::Track,
                title: "Slow".into(),
                artist: "Artist".into(),
                album: None,
                duration_seconds: None,
                url: "https://www.youtube.com/watch?v=slow".into(),
                segment: None,
                album_identity: None,
            }],
            album: None,
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut started_download = false;
    while Instant::now() < deadline {
        if service
            .try_events()
            .any(|event| matches!(event, DownloadEvent::Downloading { .. }))
        {
            started_download = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(started_download, "worker did not start before deadline");
    let started = Instant::now();
    drop(service);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn chapter_album_downloads_source_once_and_publishes_every_track() {
    let directory = tempdir().unwrap();
    let ytdlp = directory.path().join("yt-dlp");
    let ffmpeg = directory.path().join("ffmpeg");
    let download_log = directory.path().join("downloads.log");
    let conversion_log = directory.path().join("conversions.log");
    executable(
        &ytdlp,
        &format!(
            r#"#!/usr/bin/env bash
set -eu
printf 'download\n' >> '{}'
out=""
while (($#)); do
  if [[ "$1" == "-o" ]]; then shift; out="$1"; fi
  shift
done
out="${{out/'%(ext)s'/webm}}"
printf 'one source' > "$out"
"#,
            download_log.display()
        ),
    );
    executable(
        &ffmpeg,
        &format!(
            r#"#!/usr/bin/env bash
set -eu
printf '%s\n' "$*" >> '{}'
last=""
for arg in "$@"; do last="$arg"; done
printf 'OggS chapter' > "$last"
"#,
            conversion_log.display()
        ),
    );
    let album = AlbumDraft {
        provider: "youtube".into(),
        source_id: "video".into(),
        title: "Record".into(),
        artist: "Artist".into(),
    };
    let items = [("One", 0.0, 60.0), ("Two", 60.0, 120.0)]
        .into_iter()
        .enumerate()
        .map(|(index, (title, start, end))| SearchItem {
            provider: "youtube".into(),
            source_id: format!("video-chapter-{index}"),
            kind: SearchKind::Track,
            title: title.into(),
            artist: "Artist".into(),
            album: Some("Record".into()),
            duration_seconds: Some(60),
            url: "https://www.youtube.com/watch?v=video".into(),
            segment: Some(MediaSegment {
                start_seconds: start,
                end_seconds: end,
                position: (index + 1) as u32,
            }),
            album_identity: Some(album.clone()),
        })
        .collect();
    let library = directory.path().join("library");
    let service = DownloadService::start(
        ytdlp,
        ffmpeg,
        library.clone(),
        directory.path().join("cache"),
        1,
        160,
    )
    .unwrap();
    service
        .enqueue(DownloadRequest {
            job_id: "youtube-album-video".into(),
            items,
            album: Some(album),
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let tracks = loop {
        if let Some(event) = service.try_event() {
            match event {
                DownloadEvent::Completed { tracks, .. } => break tracks,
                DownloadEvent::Failed { error, .. } => panic!("{error}"),
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "album download timed out");
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(tracks.len(), 2);
    assert!(tracks.iter().all(|track| track.path.is_file()));
    assert_eq!(fs::read_to_string(download_log).unwrap().lines().count(), 1);
    let conversions = fs::read_to_string(conversion_log).unwrap();
    assert_eq!(conversions.lines().count(), 2);
    assert!(conversions.contains("-ss 60.000000 -t 60.000000"));
    assert!(conversions.contains("track=2/2"));
    assert!(fs::read_dir(library).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".part")
    }));
}
