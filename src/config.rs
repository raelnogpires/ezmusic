use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use directories::{ProjectDirs, UserDirs};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub config_file: PathBuf,
    pub database_file: PathBuf,
    pub tools_dir: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let project = ProjectDirs::from("dev", "ezmusic", "ezmusic")
            .context("nao foi possivel descobrir os diretorios do usuario")?;
        let config_dir = project.config_dir().to_path_buf();
        let data_dir = project.data_dir().to_path_buf();
        let cache_dir = project.cache_dir().to_path_buf();
        Ok(Self {
            config_file: config_dir.join("config.toml"),
            database_file: data_dir.join("library.sqlite3"),
            tools_dir: data_dir.join("tools"),
            config_dir,
            data_dir,
            cache_dir,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        for path in [
            &self.config_dir,
            &self.data_dir,
            &self.cache_dir,
            &self.tools_dir,
        ] {
            fs::create_dir_all(path)
                .with_context(|| format!("falha ao criar {}", path.display()))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub version: u32,
    pub library_path: PathBuf,
    pub import_roots: Vec<PathBuf>,
    pub audio_device: Option<String>,
    pub max_parallel_downloads: usize,
    pub opus_bitrate_kbps: u16,
    pub accepted_download_notice: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        let library_path = UserDirs::new()
            .and_then(|user| user.audio_dir().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("Music"))
            .join("EzMusic");
        Self {
            version: 1,
            library_path,
            import_roots: Vec::new(),
            audio_device: None,
            max_parallel_downloads: 1,
            opus_bitrate_kbps: 160,
            accepted_download_notice: false,
        }
    }
}

impl AppConfig {
    pub fn load(paths: &AppPaths) -> Result<Self> {
        paths.ensure()?;
        if !paths.config_file.exists() {
            let config = Self::default();
            config.save(paths)?;
            return Ok(config);
        }
        let contents = fs::read_to_string(&paths.config_file)
            .with_context(|| format!("falha ao ler {}", paths.config_file.display()))?;
        let config: Self = toml::from_str(&contents).context("config.toml invalido")?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, paths: &AppPaths) -> Result<()> {
        self.validate()?;
        paths.ensure()?;
        let encoded = toml::to_string_pretty(self).context("falha ao serializar configuracao")?;
        let temporary = paths.config_file.with_extension("toml.new");
        fs::write(&temporary, encoded)
            .with_context(|| format!("falha ao escrever {}", temporary.display()))?;
        fs::rename(&temporary, &paths.config_file)
            .with_context(|| format!("falha ao publicar {}", paths.config_file.display()))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.max_parallel_downloads == 0 || self.max_parallel_downloads > 4 {
            bail!("max_parallel_downloads deve estar entre 1 e 4");
        }
        if !(64..=320).contains(&self.opus_bitrate_kbps) {
            bail!("opus_bitrate_kbps deve estar entre 64 e 320");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_one_download_worker() {
        assert_eq!(AppConfig::default().max_parallel_downloads, 1);
    }

    #[test]
    fn rejects_unsafe_worker_counts() {
        let config = AppConfig {
            max_parallel_downloads: 0,
            ..AppConfig::default()
        };
        assert!(config.validate().is_err());
        let config = AppConfig {
            max_parallel_downloads: 5,
            ..AppConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn accepts_removed_update_field_from_old_configs() {
        let config: AppConfig = toml::from_str("tools_update_interval_hours = 24").unwrap();
        assert_eq!(config.max_parallel_downloads, 1);
    }
}
