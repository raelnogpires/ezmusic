use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{config::AppPaths, process::output_limited, storage::ensure_free_space};

const USER_AGENT: &str = "ezmusic/0.1";
const MAX_RELEASE_JSON_BYTES: usize = 4 * 1024 * 1024;
const MIN_YTDLP_VERSION: &str = "2026.01.01";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    YtDlp,
    Ffmpeg,
}

impl ToolKind {
    fn binary_name(self) -> &'static str {
        match self {
            Self::YtDlp => "yt-dlp",
            Self::Ffmpeg => "ffmpeg",
        }
    }

    fn repository(self) -> &'static str {
        match self {
            Self::YtDlp => "yt-dlp/yt-dlp",
            Self::Ffmpeg => "eugeneware/ffmpeg-static",
        }
    }

    fn asset_name(self) -> Result<&'static str> {
        match (self, std::env::consts::OS, std::env::consts::ARCH) {
            (Self::YtDlp, "linux", "x86_64") => Ok("yt-dlp_linux"),
            (Self::YtDlp, "macos", "aarch64") => Ok("yt-dlp_macos"),
            (Self::Ffmpeg, "linux", "x86_64") => Ok("ffmpeg-linux-x64"),
            (Self::Ffmpeg, "macos", "aarch64") => Ok("ffmpeg-darwin-arm64"),
            _ => bail!(
                "plataforma ainda nao suportada: {} {}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        }
    }

    fn max_asset_bytes(self) -> u64 {
        match self {
            Self::YtDlp => 64 * 1024 * 1024,
            Self::Ffmpeg => 128 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolStatus {
    pub kind: ToolKind,
    pub path: PathBuf,
    pub installed: bool,
    pub version: Option<String>,
    pub problem: Option<String>,
}

#[derive(Clone)]
pub struct ToolManager {
    paths: AppPaths,
    operation_lock: Arc<Mutex<()>>,
}

impl ToolManager {
    pub fn new(paths: AppPaths) -> Self {
        Self {
            paths,
            operation_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn path(&self, kind: ToolKind) -> PathBuf {
        self.paths.tools_dir.join(kind.binary_name())
    }

    pub fn status(&self, kind: ToolKind) -> ToolStatus {
        if let Some(path) = self.usable_tool(kind) {
            let version = smoke_version(kind, &path).ok();
            return ToolStatus {
                kind,
                installed: true,
                path,
                version,
                problem: None,
            };
        }
        let managed = self.path(kind);
        let path = system_candidate(kind).unwrap_or_else(|| managed.clone());
        let problem = if path.is_file() {
            smoke_version(kind, &path)
                .err()
                .map(|error| format!("{error:#}"))
        } else {
            None
        };
        ToolStatus {
            kind,
            installed: false,
            path,
            version: None,
            problem,
        }
    }

    pub fn ensure(&self, kind: ToolKind) -> Result<PathBuf> {
        if let Some(path) = self.usable_tool(kind) {
            return Ok(path);
        }
        let _guard = self
            .operation_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(path) = self.usable_tool(kind) {
            return Ok(path);
        }
        self.install_latest_unlocked(kind)
    }

    pub fn update_all(&self) -> Result<Vec<ToolStatus>> {
        let _guard = self
            .operation_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut failures = Vec::new();
        for kind in [ToolKind::YtDlp, ToolKind::Ffmpeg] {
            if let Err(error) = self.install_latest_unlocked(kind) {
                failures.push(format!("{}: {error:#}", kind.binary_name()));
            }
        }
        if !failures.is_empty() {
            bail!("falha ao atualizar ferramentas: {}", failures.join("; "));
        }
        Ok([ToolKind::YtDlp, ToolKind::Ffmpeg]
            .into_iter()
            .map(|kind| self.status(kind))
            .collect())
    }

    fn install_latest_unlocked(&self, kind: ToolKind) -> Result<PathBuf> {
        self.paths.ensure()?;
        let release = latest_release(kind.repository())?;
        let target = self.path(kind);
        if smoke_version(kind, &target).is_ok()
            && fs::read_to_string(self.release_marker(kind))
                .map(|tag| tag.trim() == release.tag_name)
                .unwrap_or(false)
        {
            return Ok(target);
        }
        let expected_name = kind.asset_name()?;
        let asset = release
            .assets
            .into_iter()
            .find(|asset| asset.name == expected_name)
            .with_context(|| {
                format!(
                    "asset {expected_name} nao encontrado em {}",
                    release.tag_name
                )
            })?;
        if asset.size == 0 || asset.size > kind.max_asset_bytes() {
            bail!(
                "asset {} tem tamanho inseguro: {} bytes (limite {})",
                asset.name,
                asset.size,
                kind.max_asset_bytes()
            );
        }
        let digest = asset
            .digest
            .as_deref()
            .and_then(|value| value.strip_prefix("sha256:"))
            .context("GitHub nao publicou SHA-256 para o asset")?;

        let temporary = self
            .paths
            .tools_dir
            .join(format!("{}.new", kind.binary_name()));
        let previous = self
            .paths
            .tools_dir
            .join(format!("{}.previous", kind.binary_name()));
        ensure_free_space(
            &self.paths.tools_dir,
            kind.max_asset_bytes().saturating_mul(2),
        )?;
        download_verified(
            &asset.browser_download_url,
            digest,
            &temporary,
            asset.size,
            kind.max_asset_bytes(),
        )?;
        make_executable(&temporary)?;
        smoke_version(kind, &temporary).context("o binario baixado falhou no smoke test")?;

        if previous.exists() {
            fs::remove_file(&previous)
                .with_context(|| format!("falha ao remover {}", previous.display()))?;
        }
        if target.exists() {
            fs::rename(&target, &previous).context("falha ao guardar versao anterior")?;
        }
        if let Err(error) = fs::rename(&temporary, &target) {
            if previous.exists() {
                let _ = fs::rename(&previous, &target);
            }
            return Err(error).context("falha ao ativar nova ferramenta");
        }
        if let Err(error) = smoke_version(kind, &target) {
            let _ = fs::remove_file(&target);
            if previous.exists() {
                let _ = fs::rename(&previous, &target);
            }
            return Err(error).context("nova ferramenta falhou; rollback aplicado");
        }
        self.write_release_marker(kind, &release.tag_name)?;
        Ok(target)
    }

    fn usable_tool(&self, kind: ToolKind) -> Option<PathBuf> {
        let managed = self.path(kind);
        if smoke_version(kind, &managed).is_ok() {
            return Some(managed);
        }
        system_tool(kind)
    }

    fn release_marker(&self, kind: ToolKind) -> PathBuf {
        self.paths
            .tools_dir
            .join(format!("{}.release", kind.binary_name()))
    }

    fn write_release_marker(&self, kind: ToolKind, tag: &str) -> Result<()> {
        let marker = self.release_marker(kind);
        let temporary = marker.with_extension("release.new");
        fs::write(&temporary, tag)?;
        fs::rename(temporary, marker)?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
    size: u64,
}

fn latest_release(repository: &str) -> Result<GitHubRelease> {
    let url = format!("https://api.github.com/repos/{repository}/releases/latest");
    let response = http_agent()
        .get(&url)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .call()
        .with_context(|| format!("falha ao consultar {url}"))?;
    let bytes = read_http_limited(response.into_reader(), MAX_RELEASE_JSON_BYTES)
        .context("resposta da API do GitHub excedeu o limite")?;
    serde_json::from_slice(&bytes).context("resposta invalida da API do GitHub")
}

fn download_verified(
    url: &str,
    expected_digest: &str,
    destination: &Path,
    expected_size: u64,
    max_size: u64,
) -> Result<()> {
    let response = http_agent()
        .get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .with_context(|| format!("falha ao baixar {url}"))?;
    if let Some(length) = response.header("Content-Length")
        && length
            .parse::<u64>()
            .ok()
            .is_some_and(|length| length > max_size)
    {
        bail!("servidor anunciou um download maior que o limite de seguranca");
    }
    let mut reader = response.into_reader();
    let result = (|| {
        let mut output = fs::File::create(destination)
            .with_context(|| format!("falha ao criar {}", destination.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        let mut total = 0_u64;
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total = total.saturating_add(count as u64);
            if total > max_size {
                bail!("download excedeu o limite de {max_size} bytes");
            }
            output.write_all(&buffer[..count])?;
            hasher.update(&buffer[..count]);
        }
        output.sync_all()?;
        if total != expected_size {
            bail!("tamanho incorreto: esperado {expected_size}, recebido {total}");
        }
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected_digest) {
            bail!("SHA-256 incorreto: esperado {expected_digest}, recebido {actual}");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

fn smoke_version(kind: ToolKind, path: &Path) -> Result<String> {
    if !path.is_file() {
        bail!("{} nao instalado", path.display());
    }
    let argument = match kind {
        ToolKind::YtDlp => "--version",
        ToolKind::Ffmpeg => "-version",
    };
    let mut command = Command::new(path);
    command.arg(argument);
    let output = output_limited(command, Duration::from_secs(5), 64 * 1024, 64 * 1024)
        .with_context(|| format!("falha ao executar {}", path.display()))?;
    if !output.status.success() {
        bail!("{} retornou erro", path.display());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = stdout
        .lines()
        .next()
        .unwrap_or("versao desconhecida")
        .to_string();
    match kind {
        ToolKind::YtDlp if !supported_ytdlp_version(&version) => {
            bail!("yt-dlp {version} e anterior ao minimo {MIN_YTDLP_VERSION}")
        }
        ToolKind::Ffmpeg => {
            let mut encoders = Command::new(path);
            encoders.args(["-hide_banner", "-encoders"]);
            let output = output_limited(encoders, Duration::from_secs(5), 1024 * 1024, 64 * 1024)?;
            if !output.status.success()
                || !String::from_utf8_lossy(&output.stdout).contains("libopus")
            {
                bail!("FFmpeg sem encoder libopus");
            }
        }
        ToolKind::YtDlp => {}
    }
    Ok(version)
}

fn supported_ytdlp_version(version: &str) -> bool {
    let numeric = version.strip_prefix("stable@").unwrap_or(version);
    numeric
        .get(..10)
        .is_some_and(|date| date >= MIN_YTDLP_VERSION)
}

fn system_tool(kind: ToolKind) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(kind.binary_name()))
        .find(|candidate| smoke_version(kind, candidate).is_ok())
}

fn system_candidate(kind: ToolKind) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(kind.binary_name()))
        .find(|candidate| candidate.is_file())
}

fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(30))
        .build()
}

fn read_http_limited(reader: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    reader
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut output)?;
    if output.len() > limit {
        bail!("resposta excedeu {limit} bytes");
    }
    Ok(output)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_supported_assets() {
        let result = ToolKind::YtDlp.asset_name();
        if matches!(
            (std::env::consts::OS, std::env::consts::ARCH),
            ("linux", "x86_64") | ("macos", "aarch64")
        ) {
            assert!(result.is_ok());
        }
    }

    #[test]
    fn rejects_obsolete_ytdlp_versions() {
        assert!(!supported_ytdlp_version("2024.04.09"));
        assert!(supported_ytdlp_version("2026.07.04"));
        assert!(supported_ytdlp_version("stable@2026.07.04"));
        assert!(!supported_ytdlp_version("unknown"));
    }

    #[test]
    fn limits_http_bodies() {
        let error = read_http_limited(&b"12345"[..], 4).unwrap_err();
        assert!(error.to_string().contains("excedeu"));
    }
}
