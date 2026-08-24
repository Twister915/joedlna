use std::collections::HashSet;
use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_friendly_name")]
    pub friendly_name: String,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub scanner: ScannerConfig,
    pub shares: Vec<ShareConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub http_port: u16,
    pub interface: Option<Ipv4Addr>,
    pub ssdp_max_age_seconds: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScannerConfig {
    pub rescan_interval_seconds: u64,
    pub settle_time_seconds: u64,
    pub watch_filesystem: bool,
    pub recursive: bool,
    pub follow_symlinks: bool,
    pub include_hidden: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareConfig {
    pub name: String,
    pub path: PathBuf,
    #[serde(default = "default_media_categories")]
    pub media: Vec<MediaCategory>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaCategory {
    Audio,
    Video,
    Image,
}

impl MediaCategory {
    pub const ALL: [Self; 3] = [Self::Audio, Self::Video, Self::Image];
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("configuration is invalid: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let source = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let config: Self = toml::from_str(&source).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.friendly_name.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "friendly_name must not be empty".into(),
            ));
        }
        if self.friendly_name.chars().count() > 64 {
            return Err(ConfigError::Invalid(
                "friendly_name must contain at most 64 characters".into(),
            ));
        }
        if self.shares.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one [[shares]] entry is required".into(),
            ));
        }
        if self.scanner.rescan_interval_seconds == 0 {
            return Err(ConfigError::Invalid(
                "scanner.rescan_interval_seconds must be greater than zero".into(),
            ));
        }
        if self.scanner.settle_time_seconds == 0 {
            return Err(ConfigError::Invalid(
                "scanner.settle_time_seconds must be greater than zero".into(),
            ));
        }
        if self.network.ssdp_max_age_seconds < 60 {
            return Err(ConfigError::Invalid(
                "network.ssdp_max_age_seconds must be at least 60".into(),
            ));
        }

        let mut names = HashSet::with_capacity(self.shares.len());
        let mut roots: Vec<(String, PathBuf)> = Vec::with_capacity(self.shares.len());
        for share in &self.shares {
            if share.name.trim().is_empty() {
                return Err(ConfigError::Invalid("share names must not be empty".into()));
            }
            if !names.insert(share.name.to_lowercase()) {
                return Err(ConfigError::Invalid(format!(
                    "share name {:?} is duplicated",
                    share.name
                )));
            }
            if share.media.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "share {:?} must enable at least one media category",
                    share.name
                )));
            }
            let unique_media: HashSet<_> = share.media.iter().copied().collect();
            if unique_media.len() != share.media.len() {
                return Err(ConfigError::Invalid(format!(
                    "share {:?} contains duplicate media categories",
                    share.name
                )));
            }
            let metadata = fs::metadata(&share.path).map_err(|error| {
                ConfigError::Invalid(format!(
                    "share {:?} cannot be read at {}: {error}",
                    share.name,
                    share.path.display()
                ))
            })?;
            if !metadata.is_dir() {
                return Err(ConfigError::Invalid(format!(
                    "share {:?} is not a directory: {}",
                    share.name,
                    share.path.display()
                )));
            }
            let root = share.path.canonicalize().map_err(|error| {
                ConfigError::Invalid(format!(
                    "share {:?} cannot be resolved at {}: {error}",
                    share.name,
                    share.path.display()
                ))
            })?;
            if let Some((other_name, other_root)) = roots.iter().find(|(_, other_root)| {
                root.starts_with(other_root) || other_root.starts_with(&root)
            }) {
                return Err(ConfigError::Invalid(format!(
                    "shares {:?} and {other_name:?} overlap at {} and {}; overlapping shares expose duplicate files",
                    share.name,
                    root.display(),
                    other_root.display()
                )));
            }
            roots.push((share.name.clone(), root));
        }
        Ok(())
    }

    pub fn rescan_interval(&self) -> Duration {
        Duration::from_secs(self.scanner.rescan_interval_seconds)
    }

    pub fn settle_time(&self) -> Duration {
        Duration::from_secs(self.scanner.settle_time_seconds)
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            http_port: 8_201,
            interface: None,
            ssdp_max_age_seconds: 1_800,
        }
    }
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self {
            rescan_interval_seconds: 300,
            settle_time_seconds: 30,
            watch_filesystem: true,
            recursive: true,
            follow_symlinks: false,
            include_hidden: false,
        }
    }
}

fn default_friendly_name() -> String {
    "JoeDLNA".into()
}

fn default_media_categories() -> Vec<MediaCategory> {
    MediaCategory::ALL.into()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn loads_minimal_config() {
        let temp = tempdir().unwrap();
        let media = temp.path().join("media");
        fs::create_dir(&media).unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            format!("[[shares]]\nname = \"Media\"\npath = {:?}\n", media),
        )
        .unwrap();

        let config = Config::load(&config_path).unwrap();

        assert_eq!(config.friendly_name, "JoeDLNA");
        assert_eq!(config.shares.len(), 1);
        assert!(config.scanner.watch_filesystem);
        assert!(config.scanner.recursive);
        assert_eq!(config.scanner.settle_time_seconds, 30);
        assert_eq!(config.scanner.rescan_interval_seconds, 300);
    }
}
