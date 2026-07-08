//! User configuration loading and third-party binary path resolution.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::hint::hint_error;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub binaries: Option<Binaries>,
    pub connect: Option<Connect>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binaries {
    pub chrome: Option<PathBuf>,
    pub ffmpeg: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connect {
    pub default: Option<String>,
    pub targets: Option<std::collections::BTreeMap<String, String>>,
}

pub fn load() -> Result<Config> {
    let path = config_path()?;
    load_from_path(path)
}

pub fn config_path() -> Result<PathBuf> {
    resolve_config_path(
        std::env::var_os("RDNY_CONFIG"),
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

pub fn resolve_config_path(
    rdny_config: Option<impl Into<PathBuf>>,
    xdg_config_home: Option<impl Into<PathBuf>>,
    home: Option<impl Into<PathBuf>>,
) -> Result<PathBuf> {
    if let Some(path) = rdny_config {
        Ok(path.into())
    } else if let Some(xdg) = xdg_config_home {
        Ok(xdg.into().join("rdny").join("config.toml"))
    } else {
        let home = home
            .map(Into::into)
            .context("HOME is not set; cannot resolve rdny config file")?;
        Ok(home.join(".config").join("rdny").join("config.toml"))
    }
}

pub fn load_from_path(path: PathBuf) -> Result<Config> {
    match fs::read_to_string(&path) {
        Ok(raw) => toml::from_str(&raw).map_err(|err| {
            hint_error(
                format!("could not parse config file {}: {err}", path.display()),
                "fix the TOML syntax and supported keys, then retry",
                None,
            )
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(err) => Err(hint_error(
            format!("could not read config file {}: {err}", path.display()),
            "fix file permissions or set RDNY_CONFIG to a readable config file",
            None,
        )),
    }
}

pub fn resolve_ffmpeg(env: Option<OsString>, config: &Config) -> PathBuf {
    env.map(PathBuf::from)
        .or_else(|| config.binaries.as_ref()?.ffmpeg.clone())
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_path_precedence() {
        assert_eq!(
            resolve_config_path(Some("/tmp/rdny.toml"), Some("/xdg"), Some("/home/me")).unwrap(),
            PathBuf::from("/tmp/rdny.toml")
        );
        assert_eq!(
            resolve_config_path(None::<&str>, Some("/xdg"), Some("/home/me")).unwrap(),
            PathBuf::from("/xdg/rdny/config.toml")
        );
        assert_eq!(
            resolve_config_path(None::<&str>, None::<&str>, Some("/home/me")).unwrap(),
            PathBuf::from("/home/me/.config/rdny/config.toml")
        );
    }

    #[test]
    fn parses_valid_full_config() {
        let raw = r#"
            [binaries]
            chrome = "/Applications/Helium.app/Contents/MacOS/Helium"
            ffmpeg = "/opt/homebrew/bin/ffmpeg"

            [connect]
            default = "helium"

            [connect.targets]
            helium = "127.0.0.1:9333"
        "#;
        let config: Config = toml::from_str(raw).unwrap();
        assert_eq!(
            config.binaries.as_ref().unwrap().chrome.as_ref().unwrap(),
            &PathBuf::from("/Applications/Helium.app/Contents/MacOS/Helium")
        );
        assert_eq!(
            config.connect.as_ref().unwrap().default.as_deref(),
            Some("helium")
        );
        assert_eq!(
            config
                .connect
                .as_ref()
                .unwrap()
                .targets
                .as_ref()
                .unwrap()
                .get("helium")
                .map(String::as_str),
            Some("127.0.0.1:9333")
        );
    }

    #[test]
    fn empty_file_and_missing_file_are_default() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("config.toml");
        fs::write(&empty, "").unwrap();
        assert_eq!(load_from_path(empty).unwrap(), Config::default());
        assert_eq!(
            load_from_path(dir.path().join("missing.toml")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn invalid_toml_mentions_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        fs::write(&path, "not = [toml").unwrap();
        let err = load_from_path(path.clone()).unwrap_err();
        assert!(format!("{err}").contains(&path.display().to_string()));
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = toml::from_str::<Config>("surprise = true").unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn ffmpeg_precedence() {
        let config = Config {
            binaries: Some(Binaries {
                chrome: None,
                ffmpeg: Some(PathBuf::from("/cfg/ffmpeg")),
            }),
            connect: None,
        };
        assert_eq!(
            resolve_ffmpeg(Some(OsString::from("/env/ffmpeg")), &config),
            PathBuf::from("/env/ffmpeg")
        );
        assert_eq!(resolve_ffmpeg(None, &config), PathBuf::from("/cfg/ffmpeg"));
        assert_eq!(
            resolve_ffmpeg(None, &Config::default()),
            PathBuf::from("ffmpeg")
        );
    }
}
