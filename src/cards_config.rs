//! Reading the card library's location out of localdeck's config file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Environment variable localdeck's own CLI reads, so both tools find the same
/// library without being told twice.
const CONFIG_ENV: &str = "LOCALDECK_CONFIG";

/// Only the table needed to open the library. localdeck's file also carries an
/// `[http]` section, which serde skips as an unknown field.
#[derive(Debug, Deserialize)]
pub struct CardsConfig {
    pub storage: localdeck_storage::config::Config,
}

impl CardsConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the card config at {}", path.display()))?;

        toml::from_str(&contents)
            .with_context(|| format!("cannot parse the card config at {}", path.display()))
    }
}

/// The config file to read, from the flag or else the environment.
pub fn resolve_path(flag: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = flag {
        return Ok(path.to_path_buf());
    }

    let from_env = std::env::var(CONFIG_ENV).with_context(|| {
        format!("no card config given: pass --cards-config or set {CONFIG_ENV}")
    })?;

    Ok(PathBuf::from(from_env))
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdeck_storage::{config::Database, location::Location};

    /// localdeck's real config carries sections this tool has no use for, and
    /// must still load.
    #[test]
    fn the_http_section_is_ignored() {
        let config: CardsConfig = toml::from_str(
            r#"
            [storage.database]
            type = "OnDisk"
            location = { type = "Usb", label = "MUSIC_DRIVE", path = "localdeck/db.sql" }

            [storage.library_source]
            roots = [{ type = "Usb", label = "MUSIC_DRIVE", path = "music" }]
            follow_symlinks = true

            [http]
            bind_addr = "0.0.0.0"
            port = 8080
            "#,
        )
        .expect("localdeck's own config layout must load");

        assert_eq!(
            config.storage.database,
            Database::OnDisk {
                location: Location::Usb {
                    label: "MUSIC_DRIVE".to_string(),
                    path: PathBuf::from("localdeck/db.sql"),
                }
            }
        );
    }
}
