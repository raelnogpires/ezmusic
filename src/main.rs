use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use ezmusic::{
    AppConfig, AppPaths, LibraryDb,
    player::AudioPlayer,
    tools::{ToolKind, ToolManager},
    tui,
};

#[derive(Debug, Parser)]
#[command(name = "ezmusic", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Abre a interface terminal.
    Tui,
    /// Verifica configuracao, banco, audio e ferramentas.
    Doctor,
    /// Importa uma pasta sem copiar seus arquivos.
    Import { path: PathBuf },
    /// Gerencia yt-dlp e FFmpeg.
    Tools {
        #[command(subcommand)]
        command: ToolsCommand,
    },
    /// Mede CPU, memoria e underruns durante reproducao local.
    Benchmark {
        track: PathBuf,
        #[arg(long, default_value_t = 10)]
        warmup_seconds: u64,
        #[arg(long, default_value_t = 60)]
        measure_seconds: u64,
    },
}

#[derive(Debug, Subcommand)]
enum ToolsCommand {
    /// Atualiza ferramentas agora, com verificacao e rollback.
    Update,
    /// Mostra versoes instaladas.
    Status,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("erro: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::discover()?;
    let config = AppConfig::load(&paths)?;
    let db = LibraryDb::open(&paths.database_file)?;
    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => tui::run(paths, config, db),
        Command::Doctor => doctor(&paths, &config, &db),
        Command::Import { path } => import(path, &paths, config, &db),
        Command::Tools { command } => tools(command, &paths),
        Command::Benchmark {
            track,
            warmup_seconds,
            measure_seconds,
        } => benchmark(track, config.audio_device, warmup_seconds, measure_seconds),
    }
}

fn doctor(paths: &AppPaths, config: &AppConfig, db: &LibraryDb) -> Result<()> {
    println!("EzMusic doctor");
    println!("  config: {}", paths.config_file.display());
    println!(
        "  banco: {} ({} faixas)",
        paths.database_file.display(),
        db.count_tracks()?
    );
    println!("  biblioteca: {}", config.library_path.display());
    println!(
        "  limites: {} download(s), 1 conversao, Opus {} kbps",
        config.max_parallel_downloads, config.opus_bitrate_kbps
    );
    std::fs::create_dir_all(&config.library_path)
        .with_context(|| format!("biblioteca sem escrita: {}", config.library_path.display()))?;

    let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = AudioPlayer::device_names().map_err(|error| format!("{error:#}"));
        let _ = audio_tx.send(result);
    });
    match audio_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(devices)) if devices.is_empty() => println!("  audio: nenhum dispositivo"),
        Ok(Ok(devices)) => {
            println!("  audio: {} dispositivo(s)", devices.len());
            for device in devices {
                println!("    - {device}");
            }
        }
        Ok(Err(error)) => println!("  audio: indisponivel ({error})"),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            println!("  audio: verificacao excedeu 2s (servidor de audio indisponivel)")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            println!("  audio: verificacao interrompida")
        }
    }

    let manager = ToolManager::new(paths.clone());
    for kind in [ToolKind::YtDlp, ToolKind::Ffmpeg] {
        let status = manager.status(kind);
        let name = match kind {
            ToolKind::YtDlp => "yt-dlp",
            ToolKind::Ffmpeg => "ffmpeg",
        };
        if status.installed {
            println!(
                "  {name}: {} ({})",
                status
                    .version
                    .unwrap_or_else(|| "versao desconhecida".into()),
                status.path.display()
            );
        } else {
            let reason = status
                .problem
                .as_deref()
                .map(|problem| format!("; {problem}"))
                .unwrap_or_default();
            println!("  {name}: indisponivel{reason} (sera baixado no primeiro uso)");
        }
    }
    Ok(())
}

fn import(path: PathBuf, paths: &AppPaths, mut config: AppConfig, db: &LibraryDb) -> Result<()> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("pasta inexistente: {}", path.display()))?;
    let count = db.scan_import_root(&canonical)?;
    if !config.import_roots.contains(&canonical) {
        config.import_roots.push(canonical.clone());
        config.save(paths)?;
    }
    println!("{count} arquivo(s) indexado(s) em {}", canonical.display());
    Ok(())
}

fn tools(command: ToolsCommand, paths: &AppPaths) -> Result<()> {
    let manager = ToolManager::new(paths.clone());
    match command {
        ToolsCommand::Update => {
            for status in manager.update_all()? {
                println!(
                    "{}: {}",
                    status.path.display(),
                    status.version.unwrap_or_else(|| "desconhecida".into())
                );
            }
        }
        ToolsCommand::Status => {
            for kind in [ToolKind::YtDlp, ToolKind::Ffmpeg] {
                let status = manager.status(kind);
                println!(
                    "{}: {}{}",
                    status.path.display(),
                    status.version.unwrap_or_else(|| "indisponivel".into()),
                    status
                        .problem
                        .map(|problem| format!(" ({problem})"))
                        .unwrap_or_default()
                );
            }
        }
    }
    Ok(())
}

fn benchmark(
    track: PathBuf,
    audio_device: Option<String>,
    warmup_seconds: u64,
    measure_seconds: u64,
) -> Result<()> {
    if measure_seconds == 0 {
        bail!("measure_seconds deve ser maior que zero");
    }
    if warmup_seconds > 300 {
        bail!("warmup_seconds nao pode exceder 300");
    }
    if measure_seconds > 3600 {
        bail!("measure_seconds nao pode exceder 3600");
    }
    let mut player = AudioPlayer::new(audio_device);
    player.play(&track)?;
    println!("Aquecendo por {warmup_seconds}s...");
    thread::sleep(Duration::from_secs(warmup_seconds));
    player.reset_underflows();
    let cpu_start = process_cpu_seconds();
    let started = Instant::now();
    let mut peak_rss_kib = resident_memory_kib().unwrap_or(0);
    while started.elapsed() < Duration::from_secs(measure_seconds) {
        peak_rss_kib = peak_rss_kib.max(resident_memory_kib().unwrap_or(0));
        if let Some(event) = player.try_events().next() {
            match event {
                ezmusic::player::PlayerEvent::Ended => {
                    bail!("a faixa terminou antes do fim da medicao")
                }
                ezmusic::player::PlayerEvent::Error(error) => {
                    bail!("erro de audio durante a medicao: {error}")
                }
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    let cpu_seconds = (process_cpu_seconds() - cpu_start).max(0.0);
    let cpu_percent = cpu_seconds / started.elapsed().as_secs_f64() * 100.0;
    let underflows = player.underflows();
    player.stop();

    println!("Resultado");
    println!("  RSS pico: {:.2} MiB", peak_rss_kib as f64 / 1024.0);
    println!("  CPU media: {cpu_percent:.3}% de um nucleo");
    println!("  underruns: {underflows}");
    let passed = peak_rss_kib <= 50 * 1024 && cpu_percent <= 1.0 && underflows == 0;
    println!("  meta: {}", if passed { "PASS" } else { "FAIL" });
    if !passed {
        bail!("benchmark excedeu o orcamento de performance");
    }
    Ok(())
}

fn process_cpu_seconds() -> f64 {
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return 0.0;
        }
        let user = usage.ru_utime.tv_sec as f64 + usage.ru_utime.tv_usec as f64 / 1_000_000.0;
        let system = usage.ru_stime.tv_sec as f64 + usage.ru_stime.tv_usec as f64 / 1_000_000.0;
        user + system
    }
}

#[cfg(target_os = "linux")]
fn resident_memory_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("VmRSS:")
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse().ok())
    })
}

#[cfg(target_os = "macos")]
fn resident_memory_kib() -> Option<u64> {
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            None
        } else {
            Some((usage.ru_maxrss as u64) / 1024)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn resident_memory_kib() -> Option<u64> {
    None
}
