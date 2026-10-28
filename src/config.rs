//! User settings, stored as TOML in the config directory
//! (`~/.config/toshokan/config.toml` on Linux).
//!
//! The app only ever rewrites the values it changes at runtime, so comments and anything
//! else the user put in the file are kept.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use toml_edit::{DocumentMut, Value};

use crate::api::Quality;
use crate::download::OutputFormat;

const TEMPLATE: &str = r#"# toshokan settings. Changes made in the app's Settings (F5) are saved here, keeping
# your comments; edits made here while it runs take effect on restart.

# Show erotica and pornographic titles in search (F2 toggles this in the app).
include_adult = false

# Preselected in the download dialog: "data" (original) or "data-saver" (compressed).
quality = "data"

# Preselected in the download dialog: "cbz" or "folder".
format = "cbz"

# Translation opened first on a manga's page, as a MangaDex language code
# such as "en", "es-la", "pt-br" or "fr".
language = "en"

# Where chapters are saved. ~ means your home folder.
# The TOSHOKAN_DIR environment variable overrides this.
download_dir = "~/Manga"
"#;

/// The template's first two lines, which explain how the file is kept.
const TEMPLATE_HEADER: &str =
    "# toshokan settings. Changes made in the app's Settings (F5) are saved here, keeping
# your comments; edits made here while it runs take effect on restart.
";

/// The first lines of files written by versions that only saved some settings.
const OLD_HEADER: &str =
    "# toshokan settings. The app updates include_adult, quality and format as you change
# them; edit the rest here and restart.
";

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub include_adult: bool,
    pub quality: Quality,
    pub format: OutputFormat,
    pub language: String,
    /// As written in the file; see [`Settings::download_root`].
    pub download_dir: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            include_adult: false,
            quality: Quality::Data,
            format: OutputFormat::Cbz,
            language: "en".to_owned(),
            download_dir: "~/Manga".to_owned(),
        }
    }
}

impl Settings {
    /// `$TOSHOKAN_DIR` if set, otherwise `download_dir` with `~` expanded.
    pub fn download_root(&self) -> PathBuf {
        match std::env::var_os("TOSHOKAN_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => expand_home(&self.download_dir),
        }
    }

    /// Whether `$TOSHOKAN_DIR` decides the download folder instead of `download_dir`.
    pub fn download_dir_overridden() -> bool {
        std::env::var_os("TOSHOKAN_DIR").is_some()
    }
}

#[derive(Debug)]
pub struct ConfigError {
    path: PathBuf,
    message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message.trim_end())
    }
}

impl std::error::Error for ConfigError {}

pub struct Config {
    path: PathBuf,
    pub settings: Settings,
}

impl Config {
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("toshokan")
            .join("config.toml")
    }

    /// Reads the settings at `path`. A missing file gives the defaults and is created with
    /// comments explaining each setting, so there's something to edit.
    pub fn load(path: PathBuf) -> Result<Config, ConfigError> {
        let settings = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|err| ConfigError {
                path: path.clone(),
                message: err.to_string(),
            })?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // Not being able to write the template isn't worth refusing to start over;
                // saving later will report the problem.
                let _ = write_atomically(&path, TEMPLATE);
                Settings::default()
            }
            Err(err) => {
                return Err(ConfigError {
                    path,
                    message: err.to_string(),
                });
            }
        };
        Ok(Config { path, settings })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes the settings back to the file, keeping comments and anything else in it.
    pub fn save(&self) -> io::Result<()> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => match text.strip_prefix(OLD_HEADER) {
                Some(rest) => format!("{}{rest}", TEMPLATE_HEADER),
                None => text,
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => TEMPLATE.to_owned(),
            Err(err) => return Err(err),
        };
        let mut doc: DocumentMut = text
            .parse()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        set(
            &mut doc,
            "include_adult",
            self.settings.include_adult.into(),
        );
        set(&mut doc, "quality", self.settings.quality.as_str().into());
        set(&mut doc, "format", self.settings.format.as_str().into());
        set(&mut doc, "language", self.settings.language.as_str().into());
        set(
            &mut doc,
            "download_dir",
            self.settings.download_dir.as_str().into(),
        );
        write_atomically(&self.path, &doc.to_string())
    }
}

/// Replaces `key`'s value, keeping any comment after it on the same line.
fn set(doc: &mut DocumentMut, key: &str, mut value: Value) {
    match doc.get_mut(key).and_then(|item| item.as_value_mut()) {
        Some(existing) => {
            *value.decor_mut() = existing.decor().clone();
            *existing = value;
        }
        None => doc[key] = toml_edit::Item::Value(value),
    }
}

/// A file in the app's data folder (`~/.local/share/toshokan` on Linux).
pub fn data_path(file: &str) -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("toshokan")
        .join(file)
}

/// Reads the JSON at `path`; a missing file gives the default. An unreadable one is moved
/// aside to `<file>.bak` and replaced with the default, and the returned warning says so.
pub(crate) fn load_json<T: DeserializeOwned + Default>(
    path: &Path,
    what: &str,
) -> (T, Option<String>) {
    let parsed = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|err| err.to_string()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(err) => Err(err.to_string()),
    };
    match parsed {
        Ok(value) => (value, None),
        Err(err) => {
            let backup = path.with_extension("json.bak");
            let _ = fs::rename(path, &backup);
            let warning = format!(
                "{what} couldn't be loaded ({err}); the old file was moved to {}",
                backup.display()
            );
            (T::default(), Some(warning))
        }
    }
}

/// Writes to a temporary file first so a crash never leaves a half-written config.
pub(crate) fn write_atomically(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension("toml.tmp");
    fs::write(&partial, contents)?;
    fs::rename(&partial, path)
}

/// `path` with a leading `~` replaced by the home folder.
pub(crate) fn expand_home(path: &str) -> PathBuf {
    let rest = match path.strip_prefix('~') {
        Some("") => "",
        Some(rest) if rest.starts_with('/') => &rest[1..],
        _ => return PathBuf::from(path),
    };
    match dirs::home_dir() {
        Some(home) => home.join(rest),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("toshokan").join("config.toml")
    }

    #[test]
    fn creates_a_template_that_parses_to_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(config_path(&dir)).unwrap();
        assert_eq!(config.settings, Settings::default());

        let template: Settings =
            toml::from_str(&fs::read_to_string(config.path()).unwrap()).unwrap();
        assert_eq!(template, Settings::default());
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);
        write_atomically(&path, "language = \"es-la\"\n").unwrap();
        let settings = Config::load(path).unwrap().settings;
        assert_eq!(settings.language, "es-la");
        assert_eq!(settings.format, OutputFormat::Cbz);
    }

    #[test]
    fn saving_keeps_comments_and_hand_edited_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);
        let original = "# mine\ninclude_adult = false # toggled with F2\nlanguage = \"fr\"\n";
        write_atomically(&path, original).unwrap();

        let mut config = Config::load(path.clone()).unwrap();
        config.settings.include_adult = true;
        config.settings.quality = Quality::DataSaver;
        config.save().unwrap();

        let saved = fs::read_to_string(&path).unwrap();
        assert!(
            saved
                .starts_with("# mine\ninclude_adult = true # toggled with F2\nlanguage = \"fr\"\n")
        );
        assert!(saved.contains("quality = \"data-saver\""));
        assert_eq!(Config::load(path).unwrap().settings, config.settings);
    }

    #[test]
    fn saves_the_language_and_folder_and_updates_an_old_header() {
        assert!(TEMPLATE.starts_with(TEMPLATE_HEADER));
        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);
        let old = TEMPLATE.replacen(TEMPLATE_HEADER, OLD_HEADER, 1);
        write_atomically(&path, &old).unwrap();

        let mut config = Config::load(path.clone()).unwrap();
        config.settings.language = "es-la".to_owned();
        config.settings.download_dir = "/mnt/books/manga".to_owned();
        config.save().unwrap();

        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.starts_with(TEMPLATE_HEADER), "{saved}");
        assert!(
            saved.contains(
                "# such as \"en\", \"es-la\", \"pt-br\" or \"fr\".\nlanguage = \"es-la\""
            )
        );
        assert!(saved.contains("download_dir = \"/mnt/books/manga\""));
        assert_eq!(Config::load(path).unwrap().settings, config.settings);
    }

    #[test]
    fn rejects_bad_values_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);

        write_atomically(&path, "quality = \"best\"\n").unwrap();
        let err = Config::load(path.clone()).err().unwrap().to_string();
        assert!(err.contains("quality"), "{err}");

        write_atomically(&path, "include_adlut = true\n").unwrap();
        let err = Config::load(path).err().unwrap().to_string();
        assert!(err.contains("include_adlut"), "{err}");
    }

    #[test]
    fn expands_the_home_folder() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_home("~/Manga"), home.join("Manga"));
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("/srv/manga"), PathBuf::from("/srv/manga"));
        assert_eq!(expand_home("~bob/manga"), PathBuf::from("~bob/manga"));
    }
}
