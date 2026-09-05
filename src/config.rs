use std::{collections::HashSet, fs, path::Path, path::PathBuf};

use serde::Deserialize;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    #[default]
    Tabbed,
    Linear,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub services: Vec<Service>,
    #[serde(default)]
    pub layout: Layout,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    pub cmd: String,
    #[serde(default)]
    pub cwd: PathBuf,
    #[serde(default = "enabled")]
    pub autostart: bool,
    #[serde(default)]
    pub hide_stderr: bool,
}

const fn enabled() -> bool {
    true
}

impl Config {
    /// Read and validate every service before any processes are started.
    ///
    /// # Errors
    /// Returns a contextual error for unreadable or invalid configuration.
    pub fn load(path: &Path) -> Result<Self, String> {
        let absolute = fs::canonicalize(path)
            .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        let yaml = fs::read_to_string(&absolute)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let base = absolute
            .parent()
            .ok_or("configuration has no parent directory")?;
        Self::parse(&yaml, base).map_err(|error| format!("{}: {error}", path.display()))
    }

    fn parse(yaml: &str, base: &Path) -> Result<Self, String> {
        let mut config: Self = serde_saphyr::from_str(yaml).map_err(|error| error.to_string())?;
        if config.services.is_empty() {
            return Err("services must contain at least one service".into());
        }
        let mut names = HashSet::new();
        for service in &mut config.services {
            if service.name.trim().is_empty() || service.name.chars().any(char::is_control) {
                return Err(
                    "service names must be nonempty and contain no control characters".into(),
                );
            }
            if !names.insert(service.name.clone()) {
                return Err(format!("duplicate service name: {}", service.name));
            }
            if service.cmd.trim().is_empty() || service.cmd.contains('\0') {
                return Err(format!(
                    "{}: cmd must be nonempty and contain no NUL bytes",
                    service.name
                ));
            }
            let directory = base.join(&service.cwd);
            service.cwd = fs::canonicalize(&directory).map_err(|error| {
                format!(
                    "{}: invalid cwd {}: {error}",
                    service.name,
                    directory.display()
                )
            })?;
            if !service.cwd.is_dir() {
                return Err(format!("{}: cwd is not a directory", service.name));
            }
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_relative_paths() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("app")).unwrap();
        let config = Config::parse("services:\n  - name: a\n    cmd: echo a\n  - name: b\n    cmd: echo b\n    cwd: app\n    autostart: false\n", directory.path()).unwrap();
        assert_eq!(config.layout, Layout::Tabbed);
        assert!(config.services[0].autostart);
        assert!(!config.services[0].hide_stderr);
        assert_eq!(
            config.services[0].cwd,
            fs::canonicalize(directory.path()).unwrap()
        );
        assert!(!config.services[1].autostart);
        assert_eq!(
            config.services[1].cwd,
            fs::canonicalize(directory.path().join("app")).unwrap()
        );
    }

    #[test]
    fn stderr_visibility_is_configured_per_service() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::parse(
            "services: [{name: hidden, cmd: echo, hide_stderr: true}, {name: visible, cmd: echo, hide_stderr: false}]",
            directory.path(),
        ).unwrap();
        assert!(config.services[0].hide_stderr);
        assert!(!config.services[1].hide_stderr);
    }

    #[test]
    fn invalid_configurations_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        for yaml in [
            "services: []",
            "services: [{name: '', cmd: echo}]",
            "services: [{name: a, cmd: ' '}]",
            "services: [{name: a, cmd: echo}, {name: a, cmd: echo}]",
            "services: [{name: a, cmd: echo, cwd: missing}]",
            "services: [{name: a, cmd: echo}]\nlayout: invalid",
            "services: [{name: a, cmd: echo, autostrat: false}]",
            "services: [{name: a, cmd: echo, hide_stderr: invalid}]",
        ] {
            assert!(Config::parse(yaml, directory.path()).is_err(), "{yaml}");
        }
    }

    #[test]
    fn loading_resolves_against_config_directory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("accordo.yaml");
        fs::write(&path, "services: [{name: a, cmd: pwd}]\nlayout: linear").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.layout, Layout::Linear);
        assert_eq!(
            config.services[0].cwd,
            fs::canonicalize(directory.path()).unwrap()
        );
    }
}
