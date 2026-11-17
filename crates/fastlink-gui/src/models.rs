//! Session-local registration of explicit GGUF paths.
//!
//! Validation records file metadata only. It does not copy the file, read
//! weight bytes, scan a directory, or persist anything. Filesystem checks run
//! on one background worker. Unix `O_NONBLOCK` prevents a FIFO open from
//! waiting, but does not bound `open`, metadata, or canonicalization on a
//! stalled filesystem; these OS calls have no cancellation or timeout here.
//! Remote mounts may also cause OS-managed network I/O. On Windows, device/UNC/NT
//! namespace paths and DOS reserved names are refused before `CreateFile`; that
//! gate is Windows-only, so `NUL.gguf` stays a valid Linux file name. A normal
//! Windows open likewise cannot be cancelled. The recorded confirmation identity
//! uses device/inode from the opened handle on Unix and a best-effort canonical
//! path on other targets.

use std::fs::File;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

/// Filesystem object identity captured from an opened `.gguf` handle.
///
/// Unix uses device and inode numbers. Other targets retain the canonical path
/// as a best-effort key; neither variant fingerprints file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenedFileIdentity {
    Unix { device: u64, inode: u64 },
    Path(PathBuf),
}

/// Metadata recorded for one user-supplied `.gguf` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredGguf {
    /// Path the user submitted. Later aliases do not replace it.
    pub path: PathBuf,
    /// Canonical path used for display and alias deduplication.
    pub identity: PathBuf,
    /// Object identity captured from the opened handle for later confirmation.
    pub opened_identity: OpenedFileIdentity,
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
    /// The file behind a registered path changed since it was inspected.
    Changed,
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
            Self::Changed => "That path changed since it was registered. Nothing was confirmed.",
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
///
/// `HOME` and `USERPROFILE` are both recognized. `/` and `\` compare equal,
/// and the home prefix match is ASCII case-insensitive.
#[must_use]
pub fn redacted_path(path: &Path) -> String {
    let mut homes = Vec::new();
    for key in ["HOME", "USERPROFILE"] {
        if let Some(value) = std::env::var_os(key) {
            let text = value.to_string_lossy().into_owned();
            if !text.is_empty() {
                homes.push(text);
            }
        }
    }
    let borrowed: Vec<&str> = homes.iter().map(String::as_str).collect();
    redact_path_text(&path.to_string_lossy(), &borrowed)
}

/// Redact `text` against explicit home prefixes. Used by [`redacted_path`].
#[must_use]
pub fn redact_path_text(text: &str, homes: &[&str]) -> String {
    let mut text = strip_home_prefix(text, homes);
    text = sanitize_rendered_text(&text);
    const MAX_CHARS: usize = 48;
    let count = text.chars().count();
    if count > MAX_CHARS {
        let tail: String = text.chars().skip(count - (MAX_CHARS - 1)).collect();
        text = format!("…{tail}");
    }
    text
}

fn strip_home_prefix(text: &str, homes: &[&str]) -> String {
    let norm = text.replace('\\', "/");
    for home in homes {
        let home_norm = home.trim_end_matches(['/', '\\']).replace('\\', "/");
        if home_norm.is_empty() {
            continue;
        }
        if eq_ignore_ascii_case(&norm, &home_norm) {
            return "~".to_owned();
        }
        let prefix = format!("{home_norm}/");
        if starts_with_ignore_ascii_case(&norm, &prefix) {
            return format!("~{}", &norm[home_norm.len()..]);
        }
    }
    norm
}

fn eq_ignore_ascii_case(left: &str, right: &str) -> bool {
    left.len() == right.len() && starts_with_ignore_ascii_case(left, right)
}

fn starts_with_ignore_ascii_case(text: &str, prefix: &str) -> bool {
    let mut chars = text.chars();
    for expected in prefix.chars() {
        match chars.next() {
            Some(actual) if actual.eq_ignore_ascii_case(&expected) => {}
            _ => return false,
        }
    }
    true
}

/// Replace control, bidi, line/paragraph separator, tag, and invisible filler
/// characters before UI rendering.
#[must_use]
pub fn sanitize_rendered_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if is_unsafe_render_char(c) {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

fn is_unsafe_render_char(c: char) -> bool {
    if c.is_control() {
        return true;
    }
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{FEFF}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{034F}'
            | '\u{115F}'
            | '\u{1160}'
            | '\u{2028}'..='\u{2029}'
            | '\u{2800}'
            | '\u{3164}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{FFA0}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
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
    #[cfg(windows)]
    if refused_windows_namespace(path) || refused_windows_file_name(path) {
        return Err(ModelPathError::NotAFile);
    }
    let file = match open_handle(path) {
        Ok(file) => file,
        Err(err) => return Err(map_open_error(path, &err)),
    };
    let meta = file.metadata().map_err(|err| map_metadata_error(&err))?;
    if !meta.is_file() {
        return Err(ModelPathError::NotAFile);
    }
    let identity = identity_of_opened(&file, path);
    let opened_identity = opened_file_identity(&meta, &identity);
    Ok(RegisteredGguf {
        path: path.to_path_buf(),
        identity: identity.clone(),
        opened_identity,
        file_name: sanitize_rendered_text(&display_file_name(&identity)),
        size_bytes: meta.len(),
        loaded: false,
    })
}

/// Windows namespace forms that can reach a device or a remote share before the
/// regular-file check. Pure string test, so it is unit-testable on any host.
///
/// `\\?\C:\...` and `\\?\Volume{...}\...` are valid extended-length local
/// paths and stay accepted; UNC, `\\.\`, `\??\`, pipe and `GLOBALROOT` forms are
/// refused.
#[must_use]
pub fn refused_windows_namespace(path: &Path) -> bool {
    let text = path.to_string_lossy().replace('\\', "/");
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("//./") || lower.starts_with("/??/") || lower.starts_with("//??/") {
        return true;
    }
    if let Some(rest) = lower.strip_prefix("//?/") {
        let local_drive = rest.len() >= 2
            && rest.as_bytes()[0].is_ascii_alphabetic()
            && rest.as_bytes()[1] == b':';
        let local_volume = rest.starts_with("volume{");
        return !(local_drive || local_volume);
    }
    lower.starts_with("//")
}

/// DOS reserved device names that must not be opened on Windows.
///
/// The base name is trimmed the way Win32 legacy name matching trims trailing
/// spaces. Applied only on Windows, so a Linux file named `NUL.gguf` stays valid.
#[must_use]
pub fn refused_windows_file_name(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let lower = name.to_string_lossy().to_ascii_lowercase();
    let stem = lower.split('.').next().unwrap_or("").trim_end_matches(' ');
    matches!(
        stem,
        "con"
            | "prn"
            | "aux"
            | "nul"
            | "com1"
            | "com2"
            | "com3"
            | "com4"
            | "com5"
            | "com6"
            | "com7"
            | "com8"
            | "com9"
            | "lpt1"
            | "lpt2"
            | "lpt3"
            | "lpt4"
            | "lpt5"
            | "lpt6"
            | "lpt7"
            | "lpt8"
            | "lpt9"
            | "com\u{00b9}"
            | "com\u{00b2}"
            | "com\u{00b3}"
            | "lpt\u{00b9}"
            | "lpt\u{00b2}"
            | "lpt\u{00b3}"
    )
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

/// Canonical display path of the opened file, best effort per platform.
///
/// On Linux `/proc/self/fd/N` names the file that `open` returned, so a symlink
/// swapped after that open cannot change the recorded path. Other Unix
/// targets and Windows fall back to a second `canonicalize` walk of the
/// submitted path, where the alias race is not fully excluded.
#[cfg(unix)]
fn identity_of_opened(file: &File, fallback: &Path) -> PathBuf {
    use std::os::fd::AsRawFd;
    let proc_path = format!("/proc/self/fd/{}", file.as_raw_fd());
    std::fs::canonicalize(&proc_path)
        .or_else(|_| std::fs::canonicalize(fallback))
        .unwrap_or_else(|_| fallback.to_path_buf())
}

/// Best-effort identity where `/proc/self/fd` is unavailable: a second walk.
#[cfg(not(unix))]
fn identity_of_opened(_file: &File, fallback: &Path) -> PathBuf {
    std::fs::canonicalize(fallback).unwrap_or_else(|_| fallback.to_path_buf())
}

#[cfg(unix)]
fn opened_file_identity(
    metadata: &std::fs::Metadata,
    _canonical_path: &Path,
) -> OpenedFileIdentity {
    use std::os::unix::fs::MetadataExt;
    OpenedFileIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(not(unix))]
fn opened_file_identity(
    _metadata: &std::fs::Metadata,
    canonical_path: &Path,
) -> OpenedFileIdentity {
    OpenedFileIdentity::Path(canonical_path.to_path_buf())
}

#[cfg(unix)]
fn open_handle(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

#[cfg(windows)]
fn open_handle(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    // Overlapped I/O does not make CreateFile itself cancellable or timed.
    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OVERLAPPED)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_handle(path: &Path) -> io::Result<File> {
    File::open(path)
}

fn map_open_error(path: &Path, err: &io::Error) -> ModelPathError {
    if err.kind() == ErrorKind::NotFound {
        return ModelPathError::Missing;
    }
    if err.kind() == ErrorKind::IsADirectory {
        return ModelPathError::NotAFile;
    }
    // Windows reports ERROR_ACCESS_DENIED when `CreateFile` targets a directory
    // (no FILE_FLAG_BACKUP_SEMANTICS), which std maps to PermissionDenied. Check
    // the path itself so a directory still reports `NotAFile`, not `NotReadable`.
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
    /// Catalog generation captured when the job was queued.
    pub generation: u64,
    /// Path the worker was asked to inspect. Compared with confirmation on failure.
    pub path: PathBuf,
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
    /// Bumped when the user changes which entry a queued result may touch.
    generation: u64,
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
                let clear = self.confirmation.as_deref() == Some(Path::new(draft));
                self.note_failure(error, clear);
                false
            }
        }
    }

    /// Insert or refresh one already-inspected entry. No filesystem access.
    pub fn accept_registration(&mut self, entry: RegisteredGguf) -> bool {
        let submitted = entry.path.clone();
        if let Some(index) = self.entries.iter().position(|existing| {
            existing.identity == entry.identity && existing.opened_identity == entry.opened_identity
        }) {
            self.entries[index].size_bytes = entry.size_bytes;
            self.select_index(index);
        } else if let Some(index) = self
            .entries
            .iter()
            .position(|existing| existing.path == entry.path)
        {
            // The same submitted alias now resolves to a different file. Replace
            // the stale row instead of adding a duplicate that cannot be confirmed.
            if self.confirmation.as_ref() == Some(&entry.path) {
                self.confirmation = None;
            }
            self.entries[index] = entry;
            self.select_index(index);
        } else {
            self.entries.push(entry);
            self.select_index(self.entries.len() - 1);
        }
        // Only the submitted text is cleared. A newer draft typed while the
        // worker was busy stays in the field.
        if Path::new(self.draft.trim()) == submitted.as_path() {
            self.draft.clear();
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

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.entries.len() {
            return false;
        }
        if self.selected != Some(index) {
            self.generation = self.generation.wrapping_add(1);
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
        self.generation = self.generation.wrapping_add(1);
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
        if current.identity != fresh.identity
            || current.opened_identity != fresh.opened_identity
            || current.path != fresh.path
        {
            self.confirmation = None;
            self.error = Some(ModelPathError::Changed);
            return false;
        }
        let entry = &mut self.entries[index];
        entry.size_bytes = fresh.size_bytes;
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

#[cfg(test)]
mod open_error_tests {
    use super::*;

    #[test]
    fn windows_directory_access_denied_maps_to_not_a_file() {
        // CreateFileW without FILE_FLAG_BACKUP_SEMANTICS fails with
        // ERROR_ACCESS_DENIED for a directory, which std reports as
        // PermissionDenied. The classifier must still say "not a file".
        let dir = std::env::temp_dir();
        let denied = io::Error::from(ErrorKind::PermissionDenied);
        assert_eq!(map_open_error(&dir, &denied), ModelPathError::NotAFile);
    }

    #[test]
    fn access_denied_on_a_regular_file_stays_not_readable() {
        let file = std::env::current_exe().expect("test executable");
        let denied = io::Error::from(ErrorKind::PermissionDenied);
        assert_eq!(map_open_error(&file, &denied), ModelPathError::NotReadable);
    }

    #[test]
    fn missing_open_error_stays_missing() {
        let path = std::env::temp_dir().join("fastlink-open-error-missing.gguf");
        let missing = io::Error::from(ErrorKind::NotFound);
        assert_eq!(map_open_error(&path, &missing), ModelPathError::Missing);
    }

    #[test]
    fn is_a_directory_error_maps_to_not_a_file() {
        let path = Path::new("synthetic.gguf");
        let err = io::Error::from(ErrorKind::IsADirectory);
        assert_eq!(map_open_error(path, &err), ModelPathError::NotAFile);
    }
}
