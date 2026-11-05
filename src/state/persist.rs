//! Loading and saving the library and settings as JSON.
//!
//! Each file is wrapped in a small envelope (`{"version": 1, "data": ...}`) to allow migrations.
//! Writes are atomic (temporary file, `fsync`, rename) so a crash never leaves a truncated file,
//! and the settings file is created owner-only because it holds the Discord bot token.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::library::Library;
use super::settings::Settings;

pub const LIBRARY_FILE: &str = "library.json";
pub const SETTINGS_FILE: &str = "settings.json";
const FORMAT_VERSION: u32 = 1;

#[derive(Debug)]
pub enum PersistError {
    Io { path: PathBuf, source: io::Error },
    Parse { path: PathBuf, source: serde_json::Error },
}

impl fmt::Display for PersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Parse { path, source } => write!(f, "{} is not valid: {source}", path.display()),
        }
    }
}

impl std::error::Error for PersistError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
        }
    }
}

#[derive(Serialize)]
struct EnvelopeOut<'a, T> {
    version: u32,
    data: &'a T,
}

#[derive(Deserialize)]
struct EnvelopeIn<T> {
    version: u32,
    data: T,
}

/// A directory holding the persisted files.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The default store under the user's config directory, if one can be determined.
    pub fn open_default() -> Option<Self> {
        crate::util::paths::config_dir().map(Self::new)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Loads the library; a missing file yields an empty library.
    pub fn load_library(&self) -> Result<Library, PersistError> {
        self.load(LIBRARY_FILE)
    }

    pub fn save_library(&self, library: &Library) -> Result<(), PersistError> {
        self.save(LIBRARY_FILE, library, false)
    }

    /// Loads the settings; a missing file yields the defaults.
    pub fn load_settings(&self) -> Result<Settings, PersistError> {
        self.load(SETTINGS_FILE)
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<(), PersistError> {
        self.save(SETTINGS_FILE, settings, true)
    }

    /// Moves an unreadable file aside (`<name>.corrupt-<unix time>`) so that it is preserved
    /// instead of being overwritten by the next save. Returns the backup path.
    pub fn quarantine(&self, name: &str) -> io::Result<PathBuf> {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs());
        let from = self.dir.join(name);
        let to = self.dir.join(format!("{name}.corrupt-{stamp}"));
        fs::rename(&from, &to)?;
        Ok(to)
    }

    fn load<T: DeserializeOwned + Default>(&self, name: &str) -> Result<T, PersistError> {
        let path = self.dir.join(name);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(T::default()),
            Err(source) => return Err(PersistError::Io { path, source }),
        };
        match serde_json::from_slice::<EnvelopeIn<T>>(&bytes) {
            Ok(envelope) => {
                if envelope.version > FORMAT_VERSION {
                    log::warn!(
                        "{} has format version {}, newer than this build understands ({FORMAT_VERSION})",
                        path.display(),
                        envelope.version
                    );
                }
                Ok(envelope.data)
            }
            Err(source) => Err(PersistError::Parse { path, source }),
        }
    }

    fn save<T: Serialize>(&self, name: &str, data: &T, private: bool) -> Result<(), PersistError> {
        let path = self.dir.join(name);
        let io_error = |source| PersistError::Io { path: path.clone(), source };
        fs::create_dir_all(&self.dir).map_err(io_error)?;
        let mut bytes = serde_json::to_vec_pretty(&EnvelopeOut { version: FORMAT_VERSION, data })
            .map_err(|source| PersistError::Parse { path: path.clone(), source })?;
        bytes.push(b'\n');
        write_atomic(&path, &bytes, private).map_err(io_error)
    }
}

fn write_atomic(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    // A leftover from a crash could carry wider permissions; start fresh.
    let _ = fs::remove_file(&temporary);

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;

    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::library::Track;

    fn temp_store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("kenkurs-test-{name}-{}", crate::util::id::new_id()));
        Store::new(dir)
    }

    #[test]
    fn missing_files_load_defaults() {
        let store = temp_store("missing");
        assert_eq!(store.load_library().unwrap(), Library::default());
        assert_eq!(store.load_settings().unwrap(), Settings::default());
    }

    #[test]
    fn round_trip_and_permissions() {
        let store = temp_store("roundtrip");
        let mut library = Library::default();
        library.add_playlist("Dungeon", "").tracks.push(Track::new("Drips", "/drips.ogg"));
        let settings = Settings { discord_token: "secret".into(), remote_port: 4000, ..Settings::default() };

        store.save_library(&library).unwrap();
        store.save_settings(&settings).unwrap();
        assert_eq!(store.load_library().unwrap(), library);
        assert_eq!(store.load_settings().unwrap(), settings);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.dir().join(SETTINGS_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(!store.dir().join("settings.json.tmp").exists());
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn corrupt_files_report_parse_errors_and_can_be_quarantined() {
        let store = temp_store("corrupt");
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.dir().join(LIBRARY_FILE), b"{ not json").unwrap();
        assert!(matches!(store.load_library(), Err(PersistError::Parse { .. })));
        let backup = store.quarantine(LIBRARY_FILE).unwrap();
        assert!(backup.exists());
        assert_eq!(store.load_library().unwrap(), Library::default());
        fs::remove_dir_all(store.dir()).unwrap();
    }
}
