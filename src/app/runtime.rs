//! Hosts the [`Controller`] on threads next to the real audio engine.
//!
//! A small "core" thread blocks on engine events and applies them to the controller, so track
//! changes and sound endings are handled even when no UI is redrawing (e.g. a minimized window).
//! The same thread autosaves the library and settings when they change.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};

use crate::audio::{AudioBackend, AudioEngine, AudioError, EngineConfig, EngineEvent, EngineHandle, OutputInfo, OutputPreference};
use crate::state::persist::{LIBRARY_FILE, SETTINGS_FILE};
use crate::state::{Library, PersistError, Settings, Store};

use super::controller::{Controller, Dirty};

/// How long unsaved changes may wait before being written to disk.
const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(2);

pub type SharedController = Arc<Mutex<Controller<EngineHandle>>>;

#[derive(Clone, Debug, Default)]
pub struct CoreConfig {
    /// Where the library and settings are persisted; `None` keeps everything in memory.
    pub store: Option<Store>,
    /// Overrides the output device chosen in the settings.
    pub output: Option<OutputPreference>,
}

#[derive(Debug)]
pub enum CoreError {
    Persist(PersistError),
    Audio(AudioError),
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Persist(error) => write!(f, "cannot load saved data: {error}"),
            Self::Audio(error) => write!(f, "cannot start audio: {error}"),
        }
    }
}

impl std::error::Error for CoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Persist(error) => Some(error),
            Self::Audio(error) => Some(error),
        }
    }
}

impl From<PersistError> for CoreError {
    fn from(error: PersistError) -> Self {
        Self::Persist(error)
    }
}

impl From<AudioError> for CoreError {
    fn from(error: AudioError) -> Self {
        Self::Audio(error)
    }
}

/// The running application core: engine, controller and background thread.
pub struct Core {
    controller: SharedController,
    store: Option<Store>,
    output: OutputInfo,
    engine: Option<AudioEngine>,
    pump: Option<JoinHandle<()>>,
}

impl Core {
    pub fn start(config: CoreConfig) -> Result<Self, CoreError> {
        let (library, settings) = match &config.store {
            Some(store) => (
                load_or_recover(store, LIBRARY_FILE, Store::load_library)?,
                load_or_recover(store, SETTINGS_FILE, Store::load_settings)?,
            ),
            None => (Library::default(), Settings::default()),
        };
        let output = config
            .output
            .clone()
            .unwrap_or_else(|| settings.output_device.clone().map_or(OutputPreference::Auto, OutputPreference::Device));
        let (engine, events) = AudioEngine::start(EngineConfig { output })?;
        let output = engine.output_info().clone();
        let controller = Arc::new(Mutex::new(Controller::new(engine.handle(), library, settings)));
        let pump = {
            let controller = Arc::clone(&controller);
            let store = config.store.clone();
            thread::Builder::new()
                .name("kenkurs-core".into())
                .spawn(move || pump(&controller, &events, store.as_ref()))
                .map_err(AudioError::Io)?
        };
        Ok(Self { controller, store: config.store, output, engine: Some(engine), pump: Some(pump) })
    }

    /// Locks the controller. Keep the guard short-lived: the core thread needs it for events.
    pub fn lock(&self) -> MutexGuard<'_, Controller<EngineHandle>> {
        lock(&self.controller)
    }

    /// A shared reference to the controller, e.g. for the HTTP remote thread.
    pub fn controller(&self) -> SharedController {
        Arc::clone(&self.controller)
    }

    pub fn output_info(&self) -> &OutputInfo {
        &self.output
    }

    /// Writes pending changes now.
    pub fn save(&self) -> Result<(), PersistError> {
        match &self.store {
            Some(store) => save_dirty(&self.controller, store),
            None => Ok(()),
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        if let Err(error) = self.save() {
            log::error!("saving on exit failed: {error}");
        }
        // Stopping the engine closes the event channel, which ends the core thread.
        drop(self.engine.take());
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

fn lock<B: AudioBackend>(controller: &Mutex<Controller<B>>) -> MutexGuard<'_, Controller<B>> {
    controller.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Loads a file; an unparsable one is moved aside (never overwritten) and replaced by defaults.
fn load_or_recover<T: Default>(store: &Store, name: &str, load: fn(&Store) -> Result<T, PersistError>) -> Result<T, CoreError> {
    match load(store) {
        Ok(value) => Ok(value),
        Err(PersistError::Parse { path, source }) => {
            let backup = store.quarantine(name).map_err(|error| PersistError::Io { path: path.clone(), source: error })?;
            log::error!("{} is unreadable ({source}); kept as {} and starting fresh", path.display(), backup.display());
            Ok(T::default())
        }
        Err(error) => Err(error.into()),
    }
}

fn pump<B: AudioBackend>(controller: &Mutex<Controller<B>>, events: &Receiver<EngineEvent>, store: Option<&Store>) {
    loop {
        match events.recv_timeout(AUTOSAVE_INTERVAL) {
            Ok(event) => {
                let mut controller = lock(controller);
                controller.handle_engine_event(&event);
                while let Ok(event) = events.try_recv() {
                    controller.handle_engine_event(&event);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if let Some(store) = store {
            if let Err(error) = save_dirty(controller, store) {
                log::error!("autosave failed: {error}");
            }
        }
    }
}

/// Saves what changed: snapshot under the lock, write without holding it.
fn save_dirty<B: AudioBackend>(controller: &Mutex<Controller<B>>, store: &Store) -> Result<(), PersistError> {
    let (library, settings) = {
        let mut controller = lock(controller);
        let dirty = controller.take_dirty();
        (dirty.library.then(|| controller.library().clone()), dirty.settings.then(|| controller.settings().clone()))
    };
    let mut failed = Dirty::default();
    let mut result = Ok(());
    if let Some(library) = library {
        if let Err(error) = store.save_library(&library) {
            failed.library = true;
            result = Err(error);
        }
    }
    if let Some(settings) = settings {
        if let Err(error) = store.save_settings(&settings) {
            failed.settings = true;
            result = Err(error);
        }
    }
    if failed != Dirty::default() {
        lock(controller).mark_dirty(failed); // retried on the next autosave
    }
    result
}
