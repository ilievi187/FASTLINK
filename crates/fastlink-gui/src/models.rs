//! Session-local registration of one explicit GGUF path.
//!
//! Validation records file metadata only. It does not copy the file, read
//! weight bytes, scan a directory, or persist anything.

use std::fs::File;
use std::path::{Path, PathBuf};

/// Metadata recorded for one user-supplied `.gguf` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredGguf {
    pub path: PathBuf,
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
        }
    }
}

/// Check one explicit path and return metadata.
///
/// Opens the file only to prove it is readable, then drops the handle without
/// calling `read`.
pub fn register_gguf_path(path: &Path) -> Result<RegisteredGguf, ModelPathError> {
    let extension = path.extension().and_then(|ext| ext.to_str());
    if !extension.is_some_and(|ext| ext.eq_ignore_ascii_case("gguf")) {
        return Err(ModelPathError::NotGguf);
    }
    let meta = std::fs::metadata(path).map_err(|_| ModelPathError::Missing)?;
    if !meta.is_file() {
        return Err(ModelPathError::NotAFile);
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_owned();
    let _readable = File::open(path).map_err(|_| ModelPathError::NotReadable)?;
    Ok(RegisteredGguf {
        path: path.to_path_buf(),
        file_name,
        size_bytes: meta.len(),
        loaded: false,
    })
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

    /// Validate the current draft as one path and append it when accepted.
    pub fn register_draft(&mut self) -> bool {
        let draft = self.draft.trim();
        if draft.is_empty() {
            self.error = Some(ModelPathError::Empty);
            return false;
        }
        match register_gguf_path(Path::new(draft)) {
            Ok(entry) => {
                if let Some(index) = self
                    .entries
                    .iter()
                    .position(|existing| same_registered_file(&existing.path, &entry.path))
                {
                    self.select_index(index);
                } else {
                    self.entries.push(entry);
                    self.select_index(self.entries.len() - 1);
                }
                self.error = None;
                true
            }
            Err(error) => {
                self.error = Some(error);
                false
            }
        }
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
        let changed = self.confirmation.as_ref().is_some_and(|confirmed| {
            self.entries
                .get(index)
                .is_none_or(|entry| &entry.path != confirmed)
        });
        if changed {
            self.confirmation = None;
        }
        self.selected = Some(index);
    }

    #[must_use]
    pub fn error(&self) -> Option<&ModelPathError> {
        self.error.as_ref()
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
        if self.confirmation.as_ref().is_some_and(|confirmed| {
            self.selected
                .and_then(|selected| self.entries.get(selected))
                .is_none_or(|entry| &entry.path != confirmed)
        }) {
            self.confirmation = None;
        }
        true
    }

    /// Record that the user wants a future backend to load the selection.
    ///
    /// This does not open the file again and does not set `loaded`.
    pub fn confirm_load(&mut self) -> bool {
        let Some(index) = self.selected else {
            return false;
        };
        let Some(entry) = self.entries.get(index) else {
            return false;
        };
        self.confirmation = Some(entry.path.clone());
        true
    }

    #[must_use]
    pub fn weights_loaded(&self) -> bool {
        self.entries.iter().any(|entry| entry.loaded)
    }
}

fn same_registered_file(existing: &Path, candidate: &Path) -> bool {
    if existing == candidate {
        return true;
    }
    match (
        std::fs::canonicalize(existing),
        std::fs::canonicalize(candidate),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}
