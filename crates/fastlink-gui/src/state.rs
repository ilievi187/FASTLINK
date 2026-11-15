//! Application state and the background inventory loader.
//!
//! The loader runs the CLI on a worker thread so the window stays responsive.
//! The local device is the sole node in `scope:"local_only"`; its opaque hashed
//! `node_id` is the only identity used for selection and opt-in.
//!
//! Opt-in state is intentionally session-only: this milestone creates no
//! pairing, listener or persisted consent.

use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::discovery::{DiscoveryCommand, DiscoveryInventory};
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

/// One bounded command worker. Both local inventory and explicit discovery own
/// separate instances, so a local refresh can never start discovery.
#[derive(Debug)]
pub struct WorkerLoader<T> {
    receiver: Option<Receiver<Result<T, LoadError>>>,
}

impl<T> Default for WorkerLoader<T> {
    fn default() -> Self {
        Self { receiver: None }
    }
}

impl<T> WorkerLoader<T> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.receiver.is_some()
    }

    /// Start an arbitrary provider. The app passes a command invocation; tests
    /// use this to hold the loading state open deterministically.
    pub fn start_with<F>(&mut self, task: F) -> Result<bool, LoadError>
    where
        F: FnOnce() -> Result<T, LoadError> + Send + 'static,
        T: Send + 'static,
    {
        if self.receiver.is_some() {
            return Ok(false);
        }
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("fastlink-gui-worker".to_owned())
            .spawn(move || {
                let result = task();
                let _ = sender.send(result);
            })
            .map_err(|err| {
                LoadError::new(format!("could not start worker: {err}")).with_hint(
                    "The operating system refused a new thread. See docs/GUI.md for manual CLI checks.",
                )
            })?;
        self.receiver = Some(receiver);
        Ok(true)
    }

    /// Non-blocking poll for the finished worker.
    pub fn poll(&mut self) -> Option<Result<T, LoadError>> {
        let receiver = self.receiver.as_ref()?;
        match receiver.try_recv() {
            Ok(result) => {
                self.receiver = None;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.receiver = None;
                Some(Err(LoadError::new("the worker stopped without a result")))
            }
        }
    }
}

pub type InventoryLoader = WorkerLoader<DeviceInventory>;
pub type DiscoveryLoader = WorkerLoader<DiscoveryInventory>;

impl WorkerLoader<DeviceInventory> {
    /// Start local inventory on a worker thread.
    pub fn start(&mut self, command: InventoryCommand) -> Result<bool, LoadError> {
        self.start_with(move || command.run())
    }
}

impl WorkerLoader<DiscoveryInventory> {
    /// Start explicit opt-in discovery on a worker thread.
    pub fn start(&mut self, command: DiscoveryCommand) -> Result<bool, LoadError> {
        self.start_with(move || command.run())
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
    discovery_command: DiscoveryCommand,
    discovery_loader: DiscoveryLoader,
    discovery_cancel: Option<Arc<AtomicBool>>,
    discovery: Option<DiscoveryInventory>,
    discovery_error: Option<LoadError>,
    discovery_at: Option<Instant>,
}

impl AppState {
    #[must_use]
    pub fn new(command: InventoryCommand) -> Self {
        let discovery_command = DiscoveryCommand::new(command.clone());
        Self {
            command,
            loader: InventoryLoader::new(),
            inventory: None,
            last_error: None,
            loaded_at: None,
            selected_id: None,
            opted_in: BTreeSet::new(),
            discovery_command,
            discovery_loader: DiscoveryLoader::new(),
            discovery_cancel: None,
            discovery: None,
            discovery_error: None,
            discovery_at: None,
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
    pub fn discovery_busy(&self) -> bool {
        self.discovery_loader.is_busy()
    }

    #[must_use]
    pub fn discovery(&self) -> Option<&DiscoveryInventory> {
        self.discovery.as_ref()
    }

    #[must_use]
    pub fn discovery_error(&self) -> Option<&LoadError> {
        self.discovery_error.as_ref()
    }

    #[must_use]
    pub fn discovery_age(&self) -> Option<Duration> {
        self.discovery_at.map(|instant| instant.elapsed())
    }

    #[must_use]
    pub fn discovery_command_line(&self) -> String {
        self.discovery_command.command_line()
    }

    /// Explicit user action: start `fastlink discover --json`.
    ///
    /// This is the only method that starts discovery. Construction and
    /// `refresh()` never call it.
    pub fn discover(&mut self) -> bool {
        if self.discovery_loader.is_busy() {
            return false;
        }
        self.discovery_error = None;
        self.discovery = None;
        self.discovery_cancel = None;

        let token = Arc::new(AtomicBool::new(false));
        let command = self
            .discovery_command
            .clone()
            .with_cancel_token(token.clone());
        match self.discovery_loader.start(command) {
            Ok(true) => {
                self.discovery_cancel = Some(token);
                true
            }
            Ok(false) => false,
            Err(_) => {
                self.discovery_error = Some(LoadError::new(
                    "Discovery could not start. Check the configured FASTLINK binary.",
                ));
                false
            }
        }
    }

    /// Request real cancellation of the active discovery run. The worker runner
    /// kills the CLI process group; the UI stays busy until that cleanup result
    /// arrives.
    pub fn cancel_discovery(&mut self) -> bool {
        if !self.discovery_loader.is_busy() {
            return false;
        }
        let Some(token) = self.discovery_cancel.as_ref() else {
            return false;
        };
        !token.swap(true, Ordering::SeqCst)
    }

    #[must_use]
    pub fn discovery_cancelling(&self) -> bool {
        self.discovery_cancel
            .as_ref()
            .is_some_and(|token| token.load(Ordering::SeqCst))
    }

    /// Apply a completed discovery attempt. Kept public so headless state/UI
    /// tests can exercise results without network activity.
    pub fn apply_discovery_result(&mut self, result: Result<DiscoveryInventory, LoadError>) {
        self.discovery_cancel = None;
        match result {
            Ok(discovery) => {
                self.discovery = Some(discovery);
                self.discovery_error = None;
            }
            Err(err) => {
                self.discovery = None;
                self.discovery_error = Some(safe_discovery_error(err));
            }
        }
        self.discovery_at = Some(Instant::now());
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
        let mut changed = false;
        if let Some(result) = self.loader.poll() {
            self.apply_result(result);
            changed = true;
        }
        if let Some(result) = self.discovery_loader.poll() {
            if self.discovery_cancelling() {
                self.apply_discovery_result(Err(LoadError::new(
                    "Discovery was cancelled. No candidates were shown.",
                )));
            } else {
                self.apply_discovery_result(result);
            }
            changed = true;
        }
        changed
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
        // previous session opt-in rather than silently transferring consent.
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

    /// Toggle explicit, session-only opt-in for the local node.
    pub fn toggle_opt_in(&mut self, id: &str) -> bool {
        let next = !self.is_opted_in(id);
        self.set_opt_in(id, next)
    }

    /// Set opt-in for the local node only. Unknown and non-local ids are
    /// rejected: there are no remote controls in this milestone.
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

fn safe_discovery_error(error: LoadError) -> LoadError {
    let safe = error.summary.starts_with("Discovery ")
        && !crate::discovery::text_contains_address_like(&error.summary);
    if safe {
        LoadError::new(error.summary)
    } else {
        LoadError::new("Discovery failed safely. No candidates were shown.")
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
    fn discovery_errors_are_reduced_to_safe_ui_text() {
        let mut state = AppState::new(InventoryCommand::new("fastlink"));
        state.apply_discovery_result(Err(LoadError::new("provider failed at 192.168.1.9:41641")
            .with_detail("raw endpoint 192.168.1.9")
            .with_hint("see 192.168.1.9")));
        let error = state.discovery_error().expect("safe discovery error");
        assert!(!format!("{error:?}").contains("192.168.1.9"));
        assert!(!format!("{error:?}").contains("41641"));
        assert_eq!(
            error.summary,
            "Discovery failed safely. No candidates were shown."
        );
        assert!(error.detail.is_none());
        assert!(error.hint.is_none());
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
