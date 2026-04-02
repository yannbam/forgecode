use std::path::PathBuf;
use std::sync::LazyLock;

use config::ConfigBuilder;
use config::builder::DefaultState;

use crate::ForgeConfig;
use crate::legacy::LegacyConfig;

/// Loads all `.env` files found while walking up from the current working
/// directory to the root, with priority given to closer (lower) directories.
/// Executed at most once per process.
static LOAD_DOT_ENV: LazyLock<()> = LazyLock::new(|| {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut paths = vec![];
    let mut current = PathBuf::new();

    for component in cwd.components() {
        current.push(component);
        paths.push(current.clone());
    }

    paths.reverse();

    for path in paths {
        let env_file = path.join(".env");
        if env_file.is_file() {
            dotenvy::from_path(&env_file).ok();
        }
    }
});

/// Merges [`ForgeConfig`] from layered sources using a builder pattern.
#[derive(Default)]
pub struct ConfigReader {
    builder: ConfigBuilder<DefaultState>,
}

impl ConfigReader {
    /// Merges legacy Forge state into the preferred dot-directory without
    /// overwriting files that already exist there.
    fn merge_legacy_path(legacy: &PathBuf, preferred: &PathBuf) {
        if !legacy.exists() {
            return;
        }

        // Take the fast path when the preferred directory is absent and the
        // legacy directory can be moved wholesale.
        if !preferred.exists() {
            if std::fs::rename(legacy, preferred).is_ok() {
                return;
            }
        }

        // Merge recursively so pre-existing ~/.forge content does not strand
        // skills, agents, config, or credentials in the legacy directory.
        if std::fs::create_dir_all(preferred).is_err() {
            return;
        }

        Self::merge_directory_entries(legacy, preferred);
        Self::remove_dir_if_empty(legacy);
    }

    /// Recursively merges directory entries from `source` into `target`,
    /// preserving files that already exist in `target`.
    fn merge_directory_entries(source: &PathBuf, target: &PathBuf) {
        let Ok(entries) = std::fs::read_dir(source) else {
            return;
        };

        for entry in entries.flatten() {
            let source_path = entry.path();
            let target_path = target.join(entry.file_name());

            // Move brand-new paths directly to avoid unnecessary copying.
            if !target_path.exists() {
                let _ = std::fs::rename(&source_path, &target_path);
                continue;
            }

            // Merge nested directories so legacy skills/agents are not stranded
            // when ~/.forge already contains other content.
            if source_path.is_dir() && target_path.is_dir() {
                Self::merge_directory_entries(&source_path, &target_path);
                Self::remove_dir_if_empty(&source_path);
            }
        }
    }

    /// Removes `path` when it exists and no longer contains any entries.
    fn remove_dir_if_empty(path: &PathBuf) {
        let Ok(mut entries) = std::fs::read_dir(path) else {
            return;
        };

        if entries.next().is_none() {
            let _ = std::fs::remove_dir(path);
        }
    }

    /// Returns the canonical base directory for Forge state (`~/.forge`),
    /// migrating the legacy `~/forge` directory when possible.
    fn resolved_base_path() -> PathBuf {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let preferred = home.join(".forge");
        let legacy = home.join("forge");

        // Keep the dot-directory canonical, but merge legacy contents into it
        // when an upgrade leaves both locations present.
        if legacy.exists() {
            Self::merge_legacy_path(&legacy, &preferred);
        }

        // Fall back to the legacy directory only when migration could not move
        // it into place and the preferred directory still does not exist.
        if legacy.exists() && !preferred.exists() {
            return legacy;
        }

        preferred
    }

    /// Returns the path to the legacy JSON config file
    /// (`~/.forge/.config.json`).
    pub fn config_legacy_path() -> PathBuf {
        Self::base_path().join(".config.json")
    }

    /// Returns the path to the primary TOML config file
    /// (`~/.forge/.forge.toml`).
    pub fn config_path() -> PathBuf {
        Self::base_path().join(".forge.toml")
    }

    /// Returns the base directory for all Forge config files (`~/.forge`).
    pub fn base_path() -> PathBuf {
        Self::resolved_base_path()
    }

    /// Adds the provided TOML string as a config source without touching the
    /// filesystem.
    pub fn read_toml(mut self, contents: &str) -> Self {
        self.builder = self
            .builder
            .add_source(config::File::from_str(contents, config::FileFormat::Toml));

        self
    }

    /// Adds the embedded default config (`../.forge.toml`) as a source.
    pub fn read_defaults(self) -> Self {
        let defaults = include_str!("../.forge.toml");

        self.read_toml(defaults)
    }

    /// Adds `FORGE_`-prefixed environment variables as a config source.
    pub fn read_env(mut self) -> Self {
        self.builder = self.builder.add_source(
            config::Environment::with_prefix("FORGE")
                .prefix_separator("_")
                .separator("__")
                .try_parsing(true)
                .list_separator(",")
                .with_list_parse_key("retry.status_codes")
                .with_list_parse_key("http.root_cert_paths"),
        );

        self
    }

    /// Builds and deserializes all accumulated sources into a [`ForgeConfig`].
    ///
    /// Triggers `.env` file loading (at most once per process) by walking up
    /// the directory tree from the current working directory, with closer
    /// directories taking priority.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration cannot be built or deserialized.
    pub fn build(self) -> crate::Result<ForgeConfig> {
        *LOAD_DOT_ENV;
        let config = self.builder.build()?;
        Ok(config.try_deserialize::<ForgeConfig>()?)
    }

    /// Adds `~/.forge/.forge.toml` as a config source, silently skipping if
    /// absent.
    pub fn read_global(mut self) -> Self {
        let path = Self::config_path();
        self.builder = self
            .builder
            .add_source(config::File::from(path).required(false));
        self
    }

    /// Reads `~/.forge/.config.json` (legacy format) and adds it as a source,
    /// silently skipping errors.
    pub fn read_legacy(self) -> Self {
        let content = LegacyConfig::read(&Self::config_legacy_path());
        if let Ok(content) = content {
            self.read_toml(&content)
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};
    use std::time::{SystemTime, UNIX_EPOCH};

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::ModelConfig;

    /// Serializes tests that mutate environment variables to prevent races.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    /// Holds env vars set for a test's duration and removes them on drop, while
    /// holding [`ENV_MUTEX`].
    struct EnvGuard {
        previous: Vec<(&'static str, Option<String>)>,
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        /// Sets each `(key, value)` pair in the environment, returning a guard
        /// that cleans them up on drop.
        #[must_use]
        fn set(pairs: &[(&'static str, &str)]) -> Self {
            let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let previous = pairs
                .iter()
                .map(|(key, _)| (*key, std::env::var(key).ok()))
                .collect();
            for (key, value) in pairs {
                unsafe { std::env::set_var(key, value) };
            }
            Self { previous, _lock: lock }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, previous) in &self.previous {
                if let Some(value) = previous {
                    unsafe { std::env::set_var(key, value) };
                } else {
                    unsafe { std::env::remove_var(key) };
                }
            }
        }
    }

    /// Owns a unique temporary directory and removes it when the test ends.
    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        /// Creates a unique temporary directory rooted under the system temp
        /// directory.
        fn new(name: &str) -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "forge-config-{name}-{}-{suffix}",
                std::process::id()
            ));

            std::fs::create_dir_all(&path).unwrap();

            Self { path }
        }

        /// Returns the owned path for test setup and assertions.
        fn path(&self) -> &PathBuf {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn test_base_path_uses_dot_forge_directory() {
        let fixture = TestDir::new("dot-forge");
        let _guard = EnvGuard::set(&[("HOME", fixture.path().to_str().unwrap())]);

        let actual = ConfigReader::base_path();
        let expected = fixture.path().join(".forge");

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_base_path_migrates_legacy_forge_directory() {
        let fixture = TestDir::new("legacy-migration");
        let _guard = EnvGuard::set(&[("HOME", fixture.path().to_str().unwrap())]);
        let legacy = fixture.path().join("forge");
        let expected = fixture.path().join(".forge");

        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join(".forge.toml"), "tool_supported = true\n").unwrap();

        let actual = ConfigReader::base_path();

        assert_eq!(actual, expected);
        assert!(expected.exists());
        assert!(!legacy.exists());
    }

    #[test]
    fn test_base_path_merges_legacy_directory_into_existing_dot_forge() {
        let fixture = TestDir::new("existing-dot-forge");
        let _guard = EnvGuard::set(&[("HOME", fixture.path().to_str().unwrap())]);
        let legacy = fixture.path().join("forge");
        let preferred = fixture.path().join(".forge");

        std::fs::create_dir_all(preferred.join("skills")).unwrap();
        std::fs::write(preferred.join(".credentials.json"), "{\"new\":true}").unwrap();
        std::fs::create_dir_all(legacy.join("skills/custom-skill")).unwrap();
        std::fs::write(legacy.join(".forge.toml"), "tool_supported = true\n").unwrap();
        std::fs::write(
            legacy.join("skills/custom-skill/SKILL.md"),
            "# migrated skill\n",
        )
        .unwrap();

        let actual = ConfigReader::base_path();

        assert_eq!(actual, preferred);
        assert!(preferred.join(".forge.toml").exists());
        assert!(preferred.join(".credentials.json").exists());
        assert!(preferred.join("skills/custom-skill/SKILL.md").exists());
        assert!(!legacy.exists());
    }

    #[test]
    fn test_read_parses_without_error() {
        let actual = ConfigReader::default().read_defaults().build();
        assert!(actual.is_ok(), "read() failed: {:?}", actual.err());
    }

    #[test]
    fn test_legacy_layer_does_not_overwrite_defaults() {
        // Simulate what `read_legacy` does: serialize a ForgeConfig that only
        // carries session/commit/suggest (all other fields are None) and layer
        // it on top of the embedded defaults. The default values must survive.
        let legacy = ForgeConfig {
            session: Some(ModelConfig {
                provider_id: Some("anthropic".to_string()),
                model_id: Some("claude-3".to_string()),
            }),
            ..Default::default()
        };
        let legacy_toml = toml_edit::ser::to_string_pretty(&legacy).unwrap();

        let actual = ConfigReader::default()
            // Read legacy first and then defaults
            .read_toml(&legacy_toml)
            .read_defaults()
            .build()
            .unwrap();

        // Session should come from the legacy layer
        assert_eq!(
            actual.session,
            Some(ModelConfig {
                provider_id: Some("anthropic".to_string()),
                model_id: Some("claude-3".to_string()),
            })
        );

        // Default values from .forge.toml must be retained, not reset to zero
        assert_eq!(actual.max_parallel_file_reads, 64);
        assert_eq!(actual.max_read_lines, 2000);
        assert_eq!(actual.tool_timeout_secs, 300);
        assert_eq!(actual.max_search_lines, 1000);
        assert_eq!(actual.tool_supported, true);
    }

    #[test]
    fn test_read_session_from_env_vars() {
        let _guard = EnvGuard::set(&[
            ("FORGE_SESSION__PROVIDER_ID", "fake-provider"),
            ("FORGE_SESSION__MODEL_ID", "fake-model"),
        ]);

        let actual = ConfigReader::default()
            .read_defaults()
            .read_env()
            .build()
            .unwrap();

        let expected = Some(ModelConfig {
            provider_id: Some("fake-provider".to_string()),
            model_id: Some("fake-model".to_string()),
        });
        assert_eq!(actual.session, expected);
    }
}
