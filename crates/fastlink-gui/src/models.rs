//! Session-local registration of explicit GGUF paths.
//!
//! Validation records file metadata only. It does not copy the file, read
//! weight bytes, scan a directory, or persist anything. Filesystem checks are
//! safe to run on a background worker: on Unix the open is non-blocking and
//! the size comes from `fstat` of that handle.

use std::fs::File;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

/// Metadata recorded for one user-supplied `.gguf` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredGguf {
    /// Path the user submitted. Later aliases do not replace it.
    pub path: PathBuf,
    /// Canonical path when the OS can resolve one. Compared without a new
    /// filesystem call on the UI thread.
    pub identity: PathBuf,
    pub file_name: String,
    pub size_bytes: u64,
    /// Always false: this shell never loads weights.
    pub loaded: bool,
}

/// Why an explicit path was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPathError {
    Empty,
    NotGguf,
    Missing,
    NotAFile,
    NotReadable,
    /// The OS reported a failure that is not "missing" or "permission denied".
    Unavailable,
}

impl ModelPathError {
    #[must_use]
    pub fn summary(self) -> &'static str {
        match self {
            Self::Empty => "Enter one .gguf path. An empty path is not registered.",
            Self::NotGguf => "The path must end in .gguf. Other files are not registered.",
            Self::Missing => "That path does not exist. Nothing was registered.",
            Self::NotAFile => "That path is not a file. Directories are not scanned or registered.",
            Self::NotReadable => "That .gguf file is not readable. Nothing was registered.",
            Self::Unavailable => "The path could not be checked. Nothing was registered.",
        }
    }
}

/// Map a metadata or open error without calling every failure "missing".
#[must_use]
pub fn map_metadata_error(err: &io::Error) -> ModelPathError {
    match err.kind() {
        ErrorKind::NotFound => ModelPathError::Missing,
        ErrorKind::PermissionDenied => ModelPathError::NotReadable,
        _ => ModelPathError::Unavailable,
    }
}

/// Label derived only from the entry's `loaded` flag.
#[must_use]
pub fn loaded_status_label(loaded: bool) -> &'static str {
    if loaded {
        "Loaded"
    } else {
        "Not loaded"
    }
}

/// Headline and body derived from backend and chat flags.
///
/// A disconnected backend never reports chat as available.
#[must_use]
pub fn chat_status_text(
    backend_connected: bool,
    chat_available: bool,
) -> (&'static str, &'static str) {
    if backend_connected && chat_available {
        (
            "Available",
            "An inference backend is connected. This window still does not generate tokens.",
        )
    } else if !backend_connected {
        (
            "Not available",
            "No inference backend is connected. This window does not generate tokens.",
        )
    } else {
        (
            "Not available",
            "Chat is unavailable. This window does not generate tokens.",
        )
    }
}

/// Short display form of a path. Home prefixes become `~`, and long values
/// keep only the tail so absolute machine paths are not painted in full.
#[must_use]
pub fn redacted_path(path: &Path) -> String {
    let mut text = path.to_string_lossy().into_owned();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if !home.is_empty()
            && (text == home.as_ref()
                || text.starts_with(&format!("{home}/"))
                || text.starts_with(&format!("{home}\\")))
        {
            text.replace_range(..home.len(), "~");
        }
    }
    const MAX_CHARS: usize = 48;
    let count = text.chars().count();
    if count > MAX_CHARS {
        let tail: String = text.chars().skip(count - (MAX_CHARS - 1)).collect();
        text = format!("…{tail}");
    }
    text
}

/// Check one explicit path and return metadata.
///
/// The opened handle is closed without `read`. On Unix the open uses
/// `O_NONBLOCK` so a path swapped to a FIFO cannot block the worker.
pub fn register_gguf_path(path: &Path) -> Result<RegisteredGguf, ModelPathError> {
    inspect_gguf_path(path)
}

/// Filesystem inspection used by the background model worker and direct tests.
pub fn inspect_gguf_path(path: &Path) -> Result<RegisteredGguf, ModelPathError> {
    if !extension_is_gguf(path) {
        return Err(ModelPathError::NotGguf);
    }
    let meta = open_for_metadata(path)?;
    let identity = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Ok(RegisteredGguf {
        path: path.to_path_buf(),
        identity,
        file_name: display_file_name(path),
        size_bytes: meta.len(),
        loaded: false,
    })
}

fn extension_is_gguf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.as_encoded_bytes().eq_ignore_ascii_case(b"gguf"))
}

fn display_file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn open_for_metadata(path: &Path) -> Result<std::fs::Metadata, ModelPathError> {
    match open_handle(path) {
        Ok(file) => classify_opened(file),
        Err(err) => Err(map_open_error(path, &err)),
    }
}

#[cfg(unix)]
fn open_handle(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_handle(path: &Path) -> io::Result<File> {
    File::open(path)
}

fn classify_opened(file: File) -> Result<std::fs::Metadata, ModelPathError> {
    let meta = file.metadata().map_err(|err| map_metadata_error(&err))?;
    if meta.is_file() {
        Ok(meta)
    } else {
        Err(ModelPathError::NotAFile)
    }
}

fn map_open_error(path: &Path, err: &io::Error) -> ModelPathError {
    if err.kind() == ErrorKind::NotFound {
        return ModelPathError::Missing;
    }
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_file() => ModelPathError::NotAFile,
        _ => map_metadata_error(err),
    }
}

/// What the background worker was asked to do. Neither variant loads weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelJob {
    Register,
    Confirm,
}

/// One finished filesystem check. Applied later on the state thread.
#[derive(Debug)]
pub struct ModelJobResult {
    pub job: ModelJob,
    pub outcome: Result<RegisteredGguf, ModelPathError>,
}

/// In-memory catalog. Entries exist only for paths the user submitted.
#[derive(Debug, Default)]
pub struct ModelCatalog {
    draft: String,
    entries: Vec<RegisteredGguf>,
    selected: Option<usize>,
    confirmation: Option<PathBuf>,
    error: Option<ModelPathError>,
}

impl ModelCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_draft(&mut self, path: &str) {
        self.draft = path.to_owned();
    }

    #[must_use]
    pub fn draft(&self) -> &str {
        &self.draft
    }

    /// Validate the current draft on the caller. The egui frame uses
    /// [`crate::state::AppState::request_register_model`] instead.
    pub fn register_draft(&mut self) -> bool {
        let draft = self.draft.trim();
        if draft.is_empty() {
            self.note_failure(ModelPathError::Empty, false);
            return false;
        }
        match inspect_gguf_path(Path::new(draft)) {
            Ok(entry) => self.accept_registration(entry),
            Err(error) => {
                self.note_failure(error, false);
                false
            }
        }
    }

    /// Insert or refresh one already-inspected entry. No filesystem access.
    pub fn accept_registration(&mut self, entry: RegisteredGguf) -> bool {
        if let Some(index) = self
            .entries
            .iter()
            .position(|existing| existing.identity == entry.identity)
        {
            self.entries[index].size_bytes = entry.size_bytes;
            self.entries[index].file_name = entry.file_name;
            self.select_index(index);
        } else {
            self.entries.push(entry);
            self.select_index(self.entries.len() - 1);
        }
        self.error = None;
        true
    }

    #[must_use]
    pub fn entries(&self) -> &[RegisteredGguf] {
        &self.entries
    }

    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.entries.len() {
            return false;
        }
        self.select_index(index);
        true
    }

    fn select_index(&mut self, index: usize) {
        let still_confirmed = self.confirmation.as_ref().is_some_and(|confirmed| {
            self.entries
                .get(index)
                .is_some_and(|entry| entry.path == *confirmed)
        });
        if self.confirmation.is_some() && !still_confirmed {
            self.confirmation = None;
        }
        self.selected = Some(index);
    }

    #[must_use]
    pub fn error(&self) -> Option<&ModelPathError> {
        self.error.as_ref()
    }

    pub fn note_failure(&mut self, error: ModelPathError, clear_confirmation: bool) {
        self.error = Some(error);
        if clear_confirmation {
            self.confirmation = None;
        }
    }

    #[must_use]
    pub fn load_confirmation(&self) -> Option<&Path> {
        self.confirmation.as_deref()
    }

    pub fn remove_selected(&mut self) -> bool {
        let Some(index) = self.selected else {
            return false;
        };
        if index >= self.entries.len() {
            return false;
        }
        let removed = self.entries.remove(index);
        if self.confirmation.as_ref() == Some(&removed.path) {
            self.confirmation = None;
        }
        self.selected = if self.entries.is_empty() {
            None
        } else {
            Some(index.min(self.entries.len() - 1))
        };
        true
    }

    /// Re-check the selected file, then record a future-load confirmation.
    ///
    /// This does not set `loaded`. A missing or replaced non-file clears any
    /// previous confirmation. Callers on the egui thread use the worker API.
    pub fn confirm_load(&mut self) -> bool {
        let Some(index) = self.selected else {
            return false;
        };
        let Some(path) = self.entries.get(index).map(|entry| entry.path.clone()) else {
            return false;
        };
        match inspect_gguf_path(&path) {
            Ok(fresh) => self.accept_confirmation(fresh),
            Err(error) => {
                self.note_failure(error, true);
                false
            }
        }
    }

    /// Store confirmation for the current selection after a fresh inspection.
    ///
    /// No filesystem access. Returns false when the selection no longer matches.
    pub fn accept_confirmation(&mut self, fresh: RegisteredGguf) -> bool {
        let Some(index) = self.selected else {
            return false;
        };
        let Some(current) = self.entries.get(index) else {
            return false;
        };
        if current.identity != fresh.identity && current.path != fresh.path {
            return false;
        }
        let entry = &mut self.entries[index];
        entry.size_bytes = fresh.size_bytes;
        entry.file_name = fresh.file_name;
        entry.loaded = false;
        let path = entry.path.clone();
        self.confirmation = Some(path);
        self.error = None;
        true
    }

    #[must_use]
    pub fn weights_loaded(&self) -> bool {
        self.entries.iter().any(|entry| entry.loaded)
    }
}
