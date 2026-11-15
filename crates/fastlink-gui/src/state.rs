//! Application state and the background inventory loader.
//!
//! The loader runs the CLI on a worker thread so the window stays responsive.
//! Opt-in state is intentionally session-only: this milestone creates no
//! pairing, listener or persisted consent.

use std::collections::{BTreeSet, HashSet};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crate::inventory::{Device, DeviceInventory, InventoryCommand, LoadError};

/// What the central panel should show right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Nothing has been loaded yet.
    Idle,
    /// The first load is in flight and no snapshot exists yet.
    Loading,
    /// A snapshot exists (possibly being refreshed).
    Devices,
    /// The first load failed and there is no snapshot.
    Error,
}

/// Result produced by one inventory invocation.
pub type LoadResult = Result<DeviceInventory, LoadError>;

/// Owns the worker channel for one inventory invocation at a time.
#[derive(Debug, Default)]
pub struct InventoryLoader {
    receiver: Option<Receiver<LoadResult>>,
}

impl InventoryLoader {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.receiver.is_some()
    }

    /// Start `command` on a worker thread. Returns `Ok(false)` when a load is
    /// already running.
    pub fn start(&mut self, command: InventoryCommand) -> Result<bool, LoadError> {
        self.start_with(move || command.run())
    }

    /// Start an arbitrary provider. The app passes a command invocation; tests
    /// use this to hold the loading state open deterministically.
    pub fn start_with<F>(&mut self, task: F) -> Result<bool, LoadError>
    where
        F: FnOnce() -> LoadResult + Send + 'static,
    {
        if self.receiver.is_some() {
            return Ok(false);
        }
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("fastlink-gui-inventory".to_owned())
            .spawn(move || {
                let result = task();
                let _ = sender.send(result);
            })
            .map_err(|err| {
                LoadError::new(format!("could not start inventory worker: {err}")).with_hint(
                    "The operating system refused a new thread. See docs/GUI.md for manual CLI checks.",
                )
            })?;
        self.receiver = Some(receiver);
        Ok(true)
    }

    /// Non-blocking poll for the finished worker.
    pub fn poll(&mut self) -> Option<LoadResult> {
        let receiver = self.receiver.as_ref()?;
        match receiver.try_recv() {
            Ok(result) => {
                self.receiver = None;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.receiver = None;
                Some(Err(LoadError::new(
                    "the inventory worker stopped without a result",
                )))
            }
        }
    }
}

/// All mutable state owned by the desktop shell.
#[derive(Debug)]
pub struct AppState {
    command: InventoryCommand,
    loader: InventoryLoader,
    inventory: Option<DeviceInventory>,
    last_error: Option<LoadError>,
    loaded_at: Option<Instant>,
    selected_id: Option<String>,
    opted_in: BTreeSet<String>,
}

impl AppState {
    #[must_use]
    pub fn new(command: InventoryCommand) -> Self {
        Self {
            command,
            loader: InventoryLoader::new(),
            inventory: None,
            last_error: None,
            loaded_at: None,
            selected_id: None,
            opted_in: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn command(&self) -> &InventoryCommand {
        &self.command
    }

    #[must_use]
    pub fn loader(&self) -> &InventoryLoader {
        &self.loader
    }

    #[must_use]
    pub fn loader_mut(&mut self) -> &mut InventoryLoader {
        &mut self.loader
    }

    #[must_use]
    pub fn inventory(&self) -> Option<&DeviceInventory> {
        self.inventory.as_ref()
    }

    #[must_use]
    pub fn last_error(&self) -> Option<&LoadError> {
        self.last_error.as_ref()
    }

    #[must_use]
    pub fn loaded_age(&self) -> Option<Duration> {
        self.loaded_at.map(|instant| instant.elapsed())
    }

    #[must_use]
    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }

    #[must_use]
    pub fn selected_device(&self) -> Option<&Device> {
        let inventory = self.inventory.as_ref()?;
        let selected = self.selected_id.as_deref()?;
        inventory
            .devices
            .iter()
            .find(|device| device.id == selected)
    }

    #[must_use]
    pub fn is_opted_in(&self, id: &str) -> bool {
        self.opted_in.contains(id)
    }

    #[must_use]
    pub fn opted_in_count(&self) -> usize {
        self.opted_in.len()
    }

    #[must_use]
    pub fn screen(&self) -> Screen {
        if self.inventory.is_some() {
            Screen::Devices
        } else if self.loader.is_busy() {
            Screen::Loading
        } else if self.last_error.is_some() {
            Screen::Error
        } else {
            Screen::Idle
        }
    }

    /// Kick off a refresh. A busy loader is left untouched, so a double click
    /// cannot start duplicate processes.
    pub fn refresh(&mut self) {
        if self.loader.is_busy() {
            return;
        }
        self.last_error = None;
        if let Err(err) = self.loader.start(self.command.clone()) {
            self.last_error = Some(err);
        }
    }

    /// Collect a finished worker result. Returns `true` when the UI should
    /// repaint because state changed.
    pub fn poll(&mut self) -> bool {
        let Some(result) = self.loader.poll() else {
            return false;
        };
        self.apply_result(result);
        true
    }

    /// Apply a completed inventory load.
    ///
    /// On failure, the last successful snapshot is kept (and marked stale by
    /// the UI) instead of replacing real data with an error screen.
    pub fn apply_result(&mut self, result: LoadResult) {
        match result {
            Ok(inventory) => self.apply_inventory(inventory),
            Err(err) => self.last_error = Some(err),
        }
    }

    fn apply_inventory(&mut self, inventory: DeviceInventory) {
        let ids: HashSet<&str> = inventory
            .devices
            .iter()
            .map(|device| device.id.as_str())
            .collect();
        self.opted_in.retain(|id| ids.contains(id.as_str()));

        let selection_is_current = self
            .selected_id
            .as_deref()
            .is_some_and(|id| ids.contains(id));
        if !selection_is_current {
            let next = inventory
                .devices
                .iter()
                .find(|device| device.is_local())
                .or_else(|| inventory.devices.first())
                .map(|device| device.id.clone());
            self.selected_id = next;
        }

        self.inventory = Some(inventory);
        self.last_error = None;
        self.loaded_at = Some(Instant::now());
    }

    /// Select a device that exists in the current snapshot.
    pub fn select(&mut self, id: &str) -> bool {
        let known = self
            .inventory
            .as_ref()
            .is_some_and(|inventory| inventory.devices.iter().any(|device| device.id == id));
        if known {
            self.selected_id = Some(id.to_owned());
        }
        known
    }

    /// Toggle explicit, session-only opt-in for a known device.
    pub fn toggle_opt_in(&mut self, id: &str) -> bool {
        let next = !self.is_opted_in(id);
        self.set_opt_in(id, next)
    }

    /// Set opt-in for a known device. Unknown ids are ignored.
    pub fn set_opt_in(&mut self, id: &str, opted_in: bool) -> bool {
        let known = self
            .inventory
            .as_ref()
            .is_some_and(|inventory| inventory.devices.iter().any(|device| device.id == id));
        if !known {
            return false;
        }
        if opted_in {
            self.opted_in.insert(id.to_owned());
        } else {
            self.opted_in.remove(id);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::parse_inventory_json;
    use std::sync::mpsc;

    fn inventory(devices: &str) -> DeviceInventory {
        parse_inventory_json(&format!(
            r#"{{"schema_version":1,"devices":[{devices}],"status":"local_only"}}"#
        ))
        .expect("test inventory must parse")
    }

    fn local_only() -> DeviceInventory {
        inventory(r#"{"id":"local","name":"test-host","gpu_devices":[],"network_interfaces":[]}"#)
    }

    fn two_devices() -> DeviceInventory {
        inventory(
            r#"{"id":"peer-a","name":"peer-a"},{"id":"local","name":"test-host"},{"id":"peer-b"}"#,
        )
    }

    fn error() -> LoadError {
        LoadError::new("inventory failed")
    }

    #[test]
    fn starts_idle() {
        let state = AppState::new(InventoryCommand::new("fastlink"));
        assert_eq!(state.screen(), Screen::Idle);
        assert!(state.inventory().is_none());
        assert!(state.selected_id().is_none());
        assert_eq!(state.opted_in_count(), 0);
    }

    #[test]
    fn successful_load_selects_local_device_without_opting_in() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        state.apply_result(Ok(local_only()));

        assert_eq!(state.screen(), Screen::Devices);
        assert_eq!(state.selected_id(), Some("local"));
        assert_eq!(
            state.selected_device().unwrap().name.as_deref(),
            Some("test-host")
        );
        assert_eq!(state.opted_in_count(), 0);
        assert!(state.loaded_age().is_some());
        assert!(state.last_error().is_none());
    }

    #[test]
    fn local_device_wins_default_selection() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        state.apply_result(Ok(two_devices()));
        assert_eq!(state.selected_id(), Some("local"));
    }

    #[test]
    fn empty_inventory_has_no_selection_and_no_fake_devices() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        state.apply_result(Ok(parse_inventory_json(
            r#"{"schema_version":1,"devices":[],"status":"local_only"}"#,
        )
        .unwrap()));
        assert_eq!(state.screen(), Screen::Devices);
        assert!(state.inventory().unwrap().devices.is_empty());
        assert!(state.selected_id().is_none());
    }

    #[test]
    fn failed_first_load_shows_error() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        state.apply_result(Err(error()));
        assert_eq!(state.screen(), Screen::Error);
        assert_eq!(state.last_error().unwrap().summary, "inventory failed");
    }

    #[test]
    fn failed_refresh_keeps_last_snapshot() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        state.apply_result(Ok(local_only()));
        let selected = state.selected_id().map(str::to_owned);
        state.apply_result(Err(error()));

        assert_eq!(state.screen(), Screen::Devices);
        assert_eq!(state.last_error().unwrap().summary, "inventory failed");
        assert_eq!(state.selected_id().map(str::to_owned), selected);
        assert_eq!(state.inventory().unwrap().devices.len(), 1);
    }

    #[test]
    fn refresh_prunes_stale_selection_and_opt_ins() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        state.apply_result(Ok(two_devices()));
        assert!(state.select("peer-a"));
        assert!(state.set_opt_in("peer-a", true));
        assert!(state.set_opt_in("local", true));
        assert_eq!(state.opted_in_count(), 2);

        state.apply_result(Ok(local_only()));
        assert_eq!(state.selected_id(), Some("local"));
        assert_eq!(state.opted_in_count(), 1);
        assert!(state.is_opted_in("local"));
        assert!(!state.is_opted_in("peer-a"));
    }

    #[test]
    fn refresh_keeps_valid_selection() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        state.apply_result(Ok(two_devices()));
        assert!(state.select("peer-b"));
        state.apply_result(Ok(two_devices()));
        assert_eq!(state.selected_id(), Some("peer-b"));
    }

    #[test]
    fn opt_in_requires_a_known_device_and_is_reversible() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        assert!(!state.set_opt_in("local", true));
        assert!(!state.toggle_opt_in("local"));
        assert_eq!(state.opted_in_count(), 0);

        state.apply_result(Ok(local_only()));
        assert!(state.toggle_opt_in("local"));
        assert!(state.is_opted_in("local"));
        assert_eq!(state.opted_in_count(), 1);
        assert!(state.toggle_opt_in("local"));
        assert!(!state.is_opted_in("local"));
        assert_eq!(state.opted_in_count(), 0);
        assert!(!state.toggle_opt_in("missing"));
    }

    #[test]
    fn selection_requires_a_known_device() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        assert!(!state.select("local"));
        state.apply_result(Ok(local_only()));
        assert!(!state.select("missing"));
        assert!(state.select("local"));
    }

    #[test]
    fn loading_screen_requires_a_busy_loader_without_snapshot() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        let (release, gate) = mpsc::channel::<()>();
        state
            .loader_mut()
            .start_with(move || {
                let _ = gate.recv();
                Ok(local_only())
            })
            .expect("worker must start");

        assert!(state.loader().is_busy());
        assert_eq!(state.screen(), Screen::Loading);
        assert!(!state.poll());

        release.send(()).unwrap();
        let mut waited = 0;
        while state.loader().is_busy() && waited < 200 {
            thread::sleep(Duration::from_millis(10));
            state.poll();
            waited += 1;
        }
        assert!(!state.loader().is_busy());
        assert_eq!(state.screen(), Screen::Devices);
    }

    #[test]
    fn loader_rejects_a_second_concurrent_start() {
        let mut loader = InventoryLoader::new();
        let (release, gate) = mpsc::channel::<()>();
        assert!(loader
            .start_with(move || {
                let _ = gate.recv();
                Err(error())
            })
            .unwrap());
        assert!(!loader
            .start_with(|| Ok(local_only()))
            .expect("busy is not an error"));
        release.send(()).unwrap();

        let mut waited = 0;
        while loader.is_busy() && waited < 200 {
            thread::sleep(Duration::from_millis(10));
            if loader.poll().is_some() {
                break;
            }
            waited += 1;
        }
        assert!(!loader.is_busy());
    }
}
