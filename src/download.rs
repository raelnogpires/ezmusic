use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded, unbounded};

use crate::{
    model::{DownloadEvent, DownloadRequest, TrackDraft},
    process::{configure_process_group, output_limited, terminate_process_group},
    source::MAX_COLLECTION_ITEMS,
    storage::ensure_free_space,
};

const MAX_QUEUED_DOWNLOADS: usize = 512;
const MAX_INPUT_BYTES: u64 = 1024 * 1024 * 1024;
const MIN_DOWNLOAD_FREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const CONVERSION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct DownloadService {
    request_tx: Option<Sender<DownloadRequest>>,
    event_rx: Receiver<DownloadEvent>,
    cancellations: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    shutdown: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}

impl DownloadService {
    pub fn start(
        yt_dlp: PathBuf,
        ffmpeg: PathBuf,
        library_dir: PathBuf,
        cache_dir: PathBuf,
        workers: usize,
        bitrate_kbps: u16,
    ) -> Result<Self> {
        fs::create_dir_all(&library_dir)?;
        fs::create_dir_all(&cache_dir)?;
        let (request_tx, request_rx) = bounded::<DownloadRequest>(MAX_QUEUED_DOWNLOADS);
        let (event_tx, event_rx) = unbounded::<DownloadEvent>();
        let cancellations = Arc::new(Mutex::new(HashMap::new()));
        let conversion_lock = Arc::new(Mutex::new(()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut worker_handles = Vec::new();

        for _ in 0..workers.clamp(1, 8) {
            let request_rx = request_rx.clone();
            let event_tx = event_tx.clone();
            let cancellations = Arc::clone(&cancellations);
            let conversion_lock = Arc::clone(&conversion_lock);
            let yt_dlp = yt_dlp.clone();
            let ffmpeg = ffmpeg.clone();
            let library_dir = library_dir.clone();
            let cache_dir = cache_dir.clone();
            let shutdown = Arc::clone(&shutdown);
            worker_handles.push(thread::spawn(move || {
                loop {
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    let request = match request_rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(request) => request,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    };
                    let token = cancellations
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&request.job_id)
                        .cloned()
                        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
                    if token.load(Ordering::Acquire) {
                        let _ = event_tx.send(DownloadEvent::Cancelled {
                            job_id: request.job_id.clone(),
                        });
                        cancellations
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&request.job_id);
                        continue;
                    }
                    let result = process_request(
                        &request,
                        &yt_dlp,
                        &ffmpeg,
                        &library_dir,
                        &cache_dir,
                        bitrate_kbps,
                        &conversion_lock,
                        &token,
                        &shutdown,
                        &event_tx,
                    );
                    if let Err(error) = result {
                        let event = if token.load(Ordering::Relaxed) {
                            DownloadEvent::Cancelled {
                                job_id: request.job_id.clone(),
                            }
                        } else {
                            DownloadEvent::Failed {
                                job_id: request.job_id.clone(),
                                error: format!("{error:#}"),
                            }
                        };
                        let _ = event_tx.send(event);
                    }
                    cancellations
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&request.job_id);
                }
            }));
        }
        Ok(Self {
            request_tx: Some(request_tx),
            event_rx,
            cancellations,
            shutdown,
            workers: worker_handles,
        })
    }

    pub fn enqueue(&self, request: DownloadRequest) -> Result<()> {
        let sender = self
            .request_tx
            .as_ref()
            .context("servico de downloads encerrado")?;
        let job_id = request.job_id.clone();
        self.cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(job_id.clone(), Arc::new(AtomicBool::new(false)));
        match sender.try_send(request) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.cancellations
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&job_id);
                bail!("fila cheia; limite de {MAX_QUEUED_DOWNLOADS} downloads pendentes")
            }
            Err(TrySendError::Disconnected(_)) => {
                self.cancellations
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&job_id);
                bail!("servico de downloads encerrado")
            }
        }
    }

    pub fn cancel(&self, job_id: &str) -> bool {
        let guard = self
            .cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(token) = guard.get(job_id) {
            token.store(true, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub fn try_events(&self) -> impl Iterator<Item = DownloadEvent> + '_ {
        self.event_rx.try_iter()
    }

    pub fn try_event(&self) -> Option<DownloadEvent> {
        self.event_rx.try_recv().ok()
    }
}

impl Drop for DownloadService {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        for token in self
            .cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            token.store(true, Ordering::Release);
        }
        self.request_tx.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn process_request(
    request: &DownloadRequest,
    yt_dlp: &Path,
    ffmpeg: &Path,
    library_dir: &Path,
    cache_dir: &Path,
    bitrate_kbps: u16,
    conversion_lock: &Mutex<()>,
    cancelled: &AtomicBool,
    shutdown: &AtomicBool,
    events: &Sender<DownloadEvent>,
) -> Result<()> {
    if request.items.is_empty() || request.items.len() > MAX_COLLECTION_ITEMS {
        bail!("lote de download invalido");
    }
    ensure_free_space(cache_dir, MIN_DOWNLOAD_FREE_BYTES)?;
    ensure_free_space(library_dir, MIN_DOWNLOAD_FREE_BYTES)?;
    let job_dir = cache_dir
        .join("downloads")
        .join(safe_component(&request.job_id));
    fs::create_dir_all(&job_dir)?;
    let mut sources: Vec<(String, PathBuf)> = Vec::new();
    for item in &request.items {
        let final_path = final_path(library_dir, item);
        if final_path.is_file() || sources.iter().any(|(url, _)| url == &item.url) {
            continue;
        }
        ensure_free_space(cache_dir, MIN_DOWNLOAD_FREE_BYTES)?;
        events.send(DownloadEvent::Downloading {
            job_id: request.job_id.clone(),
        })?;
        let source_dir = job_dir.join(format!("source-{:016x}", stable_hash(&item.url)));
        fs::create_dir_all(&source_dir)?;
        let input = download_source(yt_dlp, item, &source_dir, cancelled, shutdown)?;
        sources.push((item.url.clone(), input));
    }

    let _conversion_guard = conversion_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let total = request.items.len();
    let mut tracks = Vec::with_capacity(total);
    for (index, item) in request.items.iter().enumerate() {
        if is_cancelled(cancelled, shutdown) {
            bail!("download cancelado");
        }
        let path = final_path(library_dir, item);
        if !path.is_file() {
            ensure_free_space(library_dir, MIN_DOWNLOAD_FREE_BYTES)?;
            let input = sources
                .iter()
                .find(|(url, _)| url == &item.url)
                .map(|(_, path)| path)
                .context("fonte baixada nao encontrada")?;
            events.send(DownloadEvent::Converting {
                job_id: request.job_id.clone(),
                current: index + 1,
                total,
            })?;
            convert_item(
                ffmpeg,
                input,
                item,
                &path,
                total,
                bitrate_kbps,
                cancelled,
                shutdown,
            )?;
        }
        tracks.push(draft_for(item, path));
    }
    let _ = fs::remove_dir_all(&job_dir);
    events.send(DownloadEvent::Completed {
        job_id: request.job_id.clone(),
        tracks,
        album: request.album.clone(),
    })?;
    Ok(())
}

fn download_source(
    yt_dlp: &Path,
    item: &crate::model::SearchItem,
    source_dir: &Path,
    cancelled: &AtomicBool,
    shutdown: &AtomicBool,
) -> Result<PathBuf> {
    if let Some(input) = cached_input(source_dir) {
        return Ok(input);
    }
    let template = source_dir.join("input.%(ext)s");
    let mut download = Command::new(yt_dlp);
    download.args([
        "--ignore-config",
        "--no-playlist",
        "--continue",
        "--no-warnings",
        "--no-progress",
        "--socket-timeout",
        "10",
        "--retries",
        "3",
        "--fragment-retries",
        "3",
        "--concurrent-fragments",
        "1",
        "--limit-rate",
        "8M",
        "--max-filesize",
        "1G",
        "--match-filter",
        "!is_live",
        "-f",
        "bestaudio/best",
        "-o",
    ]);
    download.arg(&template).args(["--", &item.url]);
    run_cancellable(download, cancelled, shutdown, DOWNLOAD_TIMEOUT).context("download falhou")?;
    let input = cached_input(source_dir).context("yt-dlp terminou sem produzir um arquivo")?;
    if fs::metadata(&input)?.len() > MAX_INPUT_BYTES {
        bail!("arquivo de entrada excedeu o limite de 1 GiB");
    }
    Ok(input)
}

fn cached_input(source_dir: &Path) -> Option<PathBuf> {
    fs::read_dir(source_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with("input.") && !name.ends_with(".part"))
                    .unwrap_or(false)
        })
}

#[allow(clippy::too_many_arguments)]
fn convert_item(
    ffmpeg: &Path,
    input: &Path,
    item: &crate::model::SearchItem,
    final_path: &Path,
    total: usize,
    bitrate_kbps: u16,
    cancelled: &AtomicBool,
    shutdown: &AtomicBool,
) -> Result<()> {
    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("nome de arquivo de destino invalido")?;
    let part_path = final_path.with_file_name(format!(".{file_name}.part"));
    let mut conversion = Command::new(ffmpeg);
    conversion.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-nostats",
        "-y",
        "-i",
    ]);
    conversion.arg(input);
    if let Some(segment) = &item.segment {
        conversion.args([
            "-ss",
            &format!("{:.6}", segment.start_seconds),
            "-t",
            &format!("{:.6}", segment.end_seconds - segment.start_seconds),
        ]);
    }
    conversion.args(["-vn", "-map_metadata", "-1"]);
    if item.segment.is_none() && probe_is_opus(ffmpeg, input) {
        conversion.args(["-c:a", "copy"]);
    } else {
        conversion.args([
            "-c:a",
            "libopus",
            "-b:a",
            &format!("{bitrate_kbps}k"),
            "-vbr",
            "on",
            "-threads",
            "1",
            "-filter_threads",
            "1",
            "-filter_complex_threads",
            "1",
        ]);
    }
    conversion
        .args(["-metadata", &format!("title={}", item.title)])
        .args(["-metadata", &format!("artist={}", item.artist)]);
    if let Some(album) = &item.album {
        conversion.args(["-metadata", &format!("album={album}")]);
    }
    if let Some(segment) = &item.segment {
        conversion.args(["-metadata", &format!("track={}/{total}", segment.position)]);
    }
    conversion.args(["-f", "opus"]).arg(&part_path);
    if let Err(error) = run_cancellable(conversion, cancelled, shutdown, CONVERSION_TIMEOUT) {
        let _ = fs::remove_file(&part_path);
        return Err(error).context("conversao para Opus falhou");
    }
    fs::rename(&part_path, final_path).context("falha ao publicar faixa")?;
    Ok(())
}

fn final_path(library_dir: &Path, item: &crate::model::SearchItem) -> PathBuf {
    library_dir.join(format!(
        "{}-{}.opus",
        safe_component(&item.provider),
        safe_component(&item.source_id)
    ))
}

fn draft_for(item: &crate::model::SearchItem, path: PathBuf) -> TrackDraft {
    TrackDraft {
        provider: Some(item.provider.clone()),
        source_id: Some(item.source_id.clone()),
        title: item.title.clone(),
        artist: item.artist.clone(),
        album: item.album.clone(),
        path,
        duration_seconds: item.duration_seconds,
        imported: false,
    }
}

fn run_cancellable(
    mut command: Command,
    cancelled: &AtomicBool,
    shutdown: &AtomicBool,
    timeout: Duration,
) -> Result<()> {
    command.stdout(Stdio::null()).stderr(Stdio::null());
    configure_process_group(&mut command);
    let mut child = command.spawn().context("falha ao iniciar subprocesso")?;
    #[cfg(unix)]
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, child.id(), 10);
    }
    let deadline = Instant::now() + timeout;
    loop {
        if is_cancelled(cancelled, shutdown) {
            terminate_process_group(&mut child);
            bail!("operacao cancelada");
        }
        if Instant::now() >= deadline {
            terminate_process_group(&mut child);
            bail!("subprocesso excedeu o limite de {}s", timeout.as_secs());
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => bail!("subprocesso terminou com {status}"),
            Ok(None) => {}
            Err(error) => {
                terminate_process_group(&mut child);
                return Err(error).context("falha ao consultar subprocesso");
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn probe_is_opus(ffmpeg: &Path, input: &Path) -> bool {
    let mut command = Command::new(ffmpeg);
    command.args(["-hide_banner", "-nostdin", "-i"]).arg(input);
    output_limited(command, Duration::from_secs(15), 4 * 1024, 256 * 1024)
        .map(|output| String::from_utf8_lossy(&output.stderr).contains("Audio: opus"))
        .unwrap_or(false)
}

fn is_cancelled(cancelled: &AtomicBool, shutdown: &AtomicBool) -> bool {
    cancelled.load(Ordering::Acquire) || shutdown.load(Ordering::Acquire)
}

pub fn safe_component(value: &str) -> String {
    let mut filtered: String = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .collect();
    if filtered.is_empty() {
        "unknown".into()
    } else {
        if filtered.len() > 80 {
            filtered.truncate(64);
            filtered.push_str(&format!("-{:016x}", stable_hash(value)));
        }
        filtered
    }
}

fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_external_identifiers() {
        assert_eq!(safe_component("../../evil id"), "evilid");
        assert_eq!(safe_component(""), "unknown");
        assert_eq!(safe_component("abc_DEF-12"), "abc_DEF-12");
    }
}
