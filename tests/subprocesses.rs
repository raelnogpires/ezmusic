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
    model::{DownloadEvent, DownloadRequest, SearchItem, SearchKind},
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
            item: SearchItem {
                provider: "youtube".into(),
                source_id: "one".into(),
                kind: SearchKind::Track,
                title: "One".into(),
                artist: "Artist".into(),
                album: None,
                duration_seconds: Some(60),
                url: "https://www.youtube.com/watch?v=one".into(),
            },
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut completed = false;
    while Instant::now() < deadline {
        for event in service.try_events() {
            match event {
                DownloadEvent::Completed { track, .. } => {
                    assert!(track.path.is_file());
                    assert_eq!(track.path.extension().unwrap(), "opus");
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
            item: SearchItem {
                provider: "youtube".into(),
                source_id: "slow".into(),
                kind: SearchKind::Track,
                title: "Slow".into(),
                artist: "Artist".into(),
                album: None,
                duration_seconds: None,
                url: "https://www.youtube.com/watch?v=slow".into(),
            },
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
