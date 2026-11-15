//! Headless egui tests.
//!
//! No window, GPU or display server is required: `egui::Context::run_ui`
//! executes the exact UI function used by the eframe shell, and the output is
//! tessellated on the CPU to catch paint-time failures.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use egui::{Context, Pos2, RawInput, Rect, Vec2};
use fastlink_gui::discovery::parse_discovery_json;
use fastlink_gui::inventory::{parse_inventory_json, DeviceInventory, InventoryCommand, LoadError};
use fastlink_gui::state::{AppState, Screen};

const FIXTURE: &str = include_str!("../testdata/devices-v1.json");
const DISCOVERY_FIXTURE: &str = include_str!("../testdata/discovery-v1.json");
const FIXTURE_ID: &str = "0123456789abcdef0123456789abcdef";

fn inventory(json: &str) -> DeviceInventory {
    parse_inventory_json(json).expect("test inventory must parse")
}

fn fixture_inventory() -> DeviceInventory {
    inventory(FIXTURE)
}

fn null_inventory() -> DeviceInventory {
    inventory(
        r#"{
            "schema": "fastlink.devices.v1",
            "schema_version": 1,
            "scope": "local_only",
            "discovery_mode": null,
            "node_count": 1,
            "nodes": [{
                "node_id": null,
                "node_id_source": null,
                "hostname": null,
                "os": null,
                "os_release": null,
                "arch": null,
                "logical_cpu_count": null,
                "ram_total_bytes": null,
                "ram_available_bytes": null,
                "ram_free_bytes": null,
                "gpus": [],
                "capabilities": {
                    "capacity_bytes": null,
                    "throughput_bps": null,
                    "network_connectivity": null,
                    "remote_peers": []
                },
                "unknown_fields": ["node_id", "hostname", "gpus"]
            }]
        }"#,
    )
}

fn render(ctx: &Context, state: &mut AppState, size: Vec2) {
    let input = RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| fastlink_gui::ui::draw(ui, state));
    let shapes = output.shapes.clone();
    let primitives = ctx.tessellate(shapes, output.pixels_per_point);
    // No GPU upload happens in this test; discard the font-texture delta
    // explicitly (epaint asserts on silently dropped deltas).
    output.textures_delta.clear();
    assert!(!primitives.is_empty(), "UI must paint something");
}

fn render_resting_frames(state: &mut AppState) {
    let ctx = Context::default();
    let size = Vec2::new(1200.0, 800.0);
    // Panels learn their size on the first frame.
    render(&ctx, state, size);
    render(&ctx, state, size);
}

fn wait_for_result(state: &mut AppState) {
    let mut waited = 0;
    while state.loader().is_busy() {
        assert!(waited < 400, "inventory worker did not finish");
        state.poll();
        if !state.loader().is_busy() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
        waited += 1;
    }
}

#[test]
fn idle_screen_renders_headlessly() {
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    assert_eq!(state.screen(), Screen::Idle);
    render_resting_frames(&mut state);
}

#[test]
fn loading_screen_renders_headlessly() {
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    let (release, gate) = mpsc::channel::<()>();
    state
        .loader_mut()
        .start_with(move || {
            let _ = gate.recv();
            Ok(fixture_inventory())
        })
        .expect("worker must start");

    assert_eq!(state.screen(), Screen::Loading);
    render_resting_frames(&mut state);

    release.send(()).expect("release worker");
    wait_for_result(&mut state);
    assert_eq!(state.screen(), Screen::Devices);
    assert_eq!(state.selected_id(), Some(FIXTURE_ID));
    render_resting_frames(&mut state);
}

#[test]
fn error_screen_renders_headlessly() {
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state.apply_result(Err(LoadError::new(
        "could not start `fastlink devices --json`",
    )
    .with_detail("No such file or directory (os error 2)")
    .with_hint("Pass --fastlink-bin /path/to/fastlink")));

    assert_eq!(state.screen(), Screen::Error);
    render_resting_frames(&mut state);
}

#[test]
fn stale_snapshot_with_error_banner_renders_headlessly() {
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state.apply_result(Ok(fixture_inventory()));
    state.apply_result(Err(
        LoadError::new("inventory refresh failed").with_hint("Check the CLI path")
    ));

    assert_eq!(state.screen(), Screen::Devices);
    render_resting_frames(&mut state);
}

#[test]
fn empty_inventory_renders_an_honest_empty_state() {
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state.apply_result(Ok(inventory(
        r#"{
            "schema": "fastlink.devices.v1",
            "schema_version": 1,
            "scope": "local_only",
            "discovery_mode": "passive_host_evidence",
            "node_count": 0,
            "nodes": []
        }"#,
    )));

    assert_eq!(state.screen(), Screen::Devices);
    assert!(state.inventory().unwrap().nodes.is_empty());
    render_resting_frames(&mut state);
}

#[test]
fn null_and_unknown_values_render_headlessly() {
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state.apply_result(Ok(null_inventory()));

    assert_eq!(state.screen(), Screen::Devices);
    assert_eq!(state.selected_id(), None);
    assert!(!state.set_opt_in(FIXTURE_ID, true));
    render_resting_frames(&mut state);
}

#[test]
fn local_opt_in_and_selection_render_headlessly() {
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state.apply_result(Ok(fixture_inventory()));

    assert_eq!(state.selected_id(), Some(FIXTURE_ID));
    assert!(state.is_local_node(FIXTURE_ID));
    assert!(state.toggle_opt_in(FIXTURE_ID));
    assert!(state.is_opted_in(FIXTURE_ID));
    render_resting_frames(&mut state);
}

#[test]
fn narrow_window_and_long_values_do_not_panic() {
    let long_name = "n".repeat(600);
    let long_id = "i".repeat(200);
    let json = format!(
        r#"{{
            "schema": "fastlink.devices.v1",
            "schema_version": 1,
            "scope": "local_only",
            "discovery_mode": "passive_host_evidence",
            "node_count": 1,
            "nodes": [{{
                "node_id": "{long_id}",
                "node_id_source": "machine-id",
                "hostname": "{long_name}",
                "os": "linux",
                "os_release": "{long_name}",
                "arch": "x86_64",
                "logical_cpu_count": 256,
                "ram_total_bytes": 18446744073709551615,
                "ram_available_bytes": 1,
                "ram_free_bytes": 1,
                "gpus": [{{
                    "card": "card0",
                    "name": "{long_name}",
                    "driver": "{long_name}",
                    "vendor_id": "0x0000",
                    "pci_device_id": "0x0001",
                    "vram_total_bytes": 18446744073709551615,
                    "vram_free_bytes": 1
                }}],
                "capabilities": {{
                    "capacity_bytes": 18446744073709551615,
                    "throughput_bps": 0,
                    "network_connectivity": "unknown",
                    "remote_peers": ["{long_name}"]
                }},
                "unknown_fields": ["capabilities.network_connectivity"]
            }}]
        }}"#
    );
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state.apply_result(Ok(inventory(&json)));

    let ctx = Context::default();
    let size = Vec2::new(640.0, 480.0);
    render(&ctx, &mut state, size);
    render(&ctx, &mut state, size);
}

#[test]
fn discovery_candidates_render_headlessly_with_unpaired_warning() {
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state.apply_discovery_result(Ok(
        parse_discovery_json(DISCOVERY_FIXTURE).expect("discovery fixture")
    ));

    assert!(state.discovery().is_some());
    assert!(!state.discovery_busy());
    assert!(state.discovery_age().is_some());
    render_resting_frames(&mut state);
}

#[test]
fn empty_unavailable_discovery_renders_headlessly() {
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    let discovery = parse_discovery_json(
        r#"{
            "schema": "fastlink.discover.v1",
            "scope": "user_opt_in_lan_tailscale",
            "providers": [
                {"provider": "lan_mdns", "status": "unavailable"},
                {"provider": "tailscale", "status": "unavailable"}
            ],
            "nodes": []
        }"#,
    )
    .expect("empty discovery");
    state.apply_discovery_result(Ok(discovery));
    render_resting_frames(&mut state);
}

#[test]
fn safe_discovery_error_renders_headlessly() {
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state.apply_discovery_result(Err(LoadError::new(
        "Discovery response did not match the expected schema. No candidates were shown.",
    )));
    assert!(state.discovery().is_none());
    assert!(state.discovery_error().is_some());
    render_resting_frames(&mut state);
}
