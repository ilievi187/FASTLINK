//! Application state and the background inventory loader.
//!
//! The loader runs the CLI on a worker thread so the window stays responsive.
//! The local device is the sole node in `scope:"local_only"`; its opaque hashed
//! `node_id` is the only identity used for selection and the session-only marker.
//!
//! The marker is intentionally session-only and has no operational effect:
//! this milestone creates no pairing, listener, compute permission or
//! persisted state.

use std::collections::{BTreeSet, HashSet};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crate::inventory::{DeviceInventory, InventoryCommand, InventoryNode, LoadError};

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
    pub fn selected_node(&self) -> Option<&InventoryNode> {
        let inventory = self.inventory.as_ref()?;
        let selected = self.selected_id.as_deref()?;
        inventory
            .nodes
            .iter()
            .find(|node| node.node_id.as_deref() == Some(selected))
    }

    /// Sole node in `scope:"local_only"`, when present.
    #[must_use]
    pub fn local_node(&self) -> Option<&InventoryNode> {
        self.inventory.as_ref()?.local_node()
    }

    /// Opaque id of the local node, if the backend could derive one.
    #[must_use]
    pub fn local_node_id(&self) -> Option<&str> {
        self.inventory.as_ref()?.local_node_id()
    }

    #[must_use]
    pub fn is_local_node(&self, id: &str) -> bool {
        self.local_node_id() == Some(id)
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
        let local_id = inventory.local_node_id().map(str::to_owned);

        // Consent is local-device-only. A changed local node id invalidates the
        // previous session marker rather than silently transferring intent.
        self.opted_in
            .retain(|id| local_id.as_deref() == Some(id.as_str()));

        let known_ids: HashSet<&str> = inventory
            .nodes
            .iter()
            .filter_map(|node| node.node_id.as_deref())
            .collect();
        let selection_is_current = self
            .selected_id
            .as_deref()
            .is_some_and(|id| known_ids.contains(id));
        if !selection_is_current {
            self.selected_id = local_id
                .filter(|id| known_ids.contains(id.as_str()))
                .or_else(|| inventory.nodes.iter().find_map(|node| node.node_id.clone()));
        }

        self.inventory = Some(inventory);
        self.last_error = None;
        self.loaded_at = Some(Instant::now());
    }

    /// Select a node that exists in the current snapshot.
    pub fn select(&mut self, id: &str) -> bool {
        let known = self.inventory.as_ref().is_some_and(|inventory| {
            inventory
                .nodes
                .iter()
                .any(|node| node.node_id.as_deref() == Some(id))
        });
        if known {
            self.selected_id = Some(id.to_owned());
        }
        known
    }

    /// Toggle the session-only visual-intent marker for the local node.
    pub fn toggle_opt_in(&mut self, id: &str) -> bool {
        let next = !self.is_opted_in(id);
        self.set_opt_in(id, next)
    }

    /// Set the session-only visual-intent marker for the local node only.
    /// Unknown and non-local ids are rejected, and the marker has no
    /// operational effect.
    pub fn set_opt_in(&mut self, id: &str, opted_in: bool) -> bool {
        if !self.is_local_node(id) {
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

    const FIXTURE: &str = include_str!("../testdata/devices-v1.json");
    const FIXTURE_ID: &str = "0123456789abcdef0123456789abcdef";

    fn fixture() -> DeviceInventory {
        parse_inventory_json(FIXTURE).expect("fixture must parse")
    }

    fn inventory_with_id(id: &str) -> DeviceInventory {
        parse_inventory_json(&FIXTURE.replace(FIXTURE_ID, id)).expect("test inventory must parse")
    }

    fn inventory_without_id() -> DeviceInventory {
        let json = FIXTURE.replace(&format!("\"node_id\":\"{FIXTURE_ID}\""), "\"node_id\":null");
        parse_inventory_json(&json).expect("null-id inventory must parse")
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
    fn successful_load_selects_the_opaque_local_node_without_opting_in() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        state.apply_result(Ok(fixture()));

        assert_eq!(state.screen(), Screen::Devices);
        assert_eq!(state.selected_id(), Some(FIXTURE_ID));
        assert_eq!(state.local_node_id(), Some(FIXTURE_ID));
        assert_ne!(state.local_node_id(), Some("local"));
        assert_eq!(
            state.selected_node().unwrap().hostname.as_deref(),
            Some("fixture-host")
        );
        assert_eq!(state.opted_in_count(), 0);
        assert!(state.loaded_age().is_some());
        assert!(state.last_error().is_none());
    }

    #[test]
    fn empty_inventory_has_no_selection_and_no_fake_nodes() {
        let json = r#"{
            "schema": "fastlink.devices.v1",
            "schema_version": 1,
            "scope": "local_only",
            "discovery_mode": "passive_host_evidence",
            "node_count": 0,
            "nodes": []
        }"#;
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        state.apply_result(Ok(parse_inventory_json(json).unwrap()));
        assert_eq!(state.screen(), Screen::Devices);
        assert!(state.inventory().unwrap().nodes.is_empty());
        assert!(state.selected_id().is_none());
        assert!(state.local_node().is_none());
    }

    #[test]
    fn node_without_an_id_cannot_be_selected_or_opted_in() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        state.apply_result(Ok(inventory_without_id()));
        assert_eq!(state.screen(), Screen::Devices);
        assert!(state.local_node().is_some());
        assert_eq!(state.local_node_id(), None);
        assert!(state.selected_id().is_none());
        assert!(!state.set_opt_in("local", true));
        assert!(!state.toggle_opt_in("local"));
        assert_eq!(state.opted_in_count(), 0);
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
        state.apply_result(Ok(fixture()));
        let selected = state.selected_id().map(str::to_owned);
        state.apply_result(Err(error()));

        assert_eq!(state.screen(), Screen::Devices);
        assert_eq!(state.last_error().unwrap().summary, "inventory failed");
        assert_eq!(state.selected_id().map(str::to_owned), selected);
        assert_eq!(state.inventory().unwrap().nodes.len(), 1);
    }

    #[test]
    fn refresh_prunes_opt_in_when_local_node_id_changes() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        state.apply_result(Ok(fixture()));
        assert!(state.set_opt_in(FIXTURE_ID, true));
        assert_eq!(state.opted_in_count(), 1);

        let new_id = "fedcba9876543210fedcba9876543210";
        state.apply_result(Ok(inventory_with_id(new_id)));
        assert_eq!(state.selected_id(), Some(new_id));
        assert_eq!(state.opted_in_count(), 0);
        assert!(!state.is_opted_in(FIXTURE_ID));
        assert!(!state.is_opted_in(new_id));
    }

    #[test]
    fn refresh_keeps_valid_selection_and_opt_in() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        state.apply_result(Ok(fixture()));
        assert!(state.set_opt_in(FIXTURE_ID, true));
        state.apply_result(Ok(fixture()));
        assert_eq!(state.selected_id(), Some(FIXTURE_ID));
        assert!(state.is_opted_in(FIXTURE_ID));
        assert_eq!(state.opted_in_count(), 1);
    }

    #[test]
    fn opt_in_is_local_node_only_and_reversible() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        assert!(!state.set_opt_in(FIXTURE_ID, true));
        assert!(!state.toggle_opt_in(FIXTURE_ID));
        assert_eq!(state.opted_in_count(), 0);

        state.apply_result(Ok(fixture()));
        assert!(!state.set_opt_in("some-other-node", true));
        assert!(!state.set_opt_in("local", true));
        assert_eq!(state.opted_in_count(), 0);

        assert!(state.toggle_opt_in(FIXTURE_ID));
        assert!(state.is_opted_in(FIXTURE_ID));
        assert!(state.is_local_node(FIXTURE_ID));
        assert_eq!(state.opted_in_count(), 1);
        assert!(state.toggle_opt_in(FIXTURE_ID));
        assert!(!state.is_opted_in(FIXTURE_ID));
        assert_eq!(state.opted_in_count(), 0);
        assert!(!state.toggle_opt_in("missing"));
    }

    #[test]
    fn selection_requires_a_known_node_id() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        assert!(!state.select(FIXTURE_ID));
        state.apply_result(Ok(fixture()));
        assert!(!state.select("missing"));
        assert!(!state.select("local"));
        assert!(state.select(FIXTURE_ID));
    }

    #[test]
    fn loading_screen_requires_a_busy_loader_without_snapshot() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        let (release, gate) = mpsc::channel::<()>();
        state
            .loader_mut()
            .start_with(move || {
                let _ = gate.recv();
                Ok(fixture())
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
        assert_eq!(state.selected_id(), Some(FIXTURE_ID));
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
            .start_with(|| Ok(fixture()))
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
