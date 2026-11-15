//! Headless egui tests.
//!
//! No window, GPU or display server is required: `egui::Context::run_ui`
//! executes the exact UI function used by the eframe shell, and the output is
//! tessellated to catch paint-time failures.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use egui::{Context, Pos2, RawInput, Rect, Vec2};
use fastlink_gui::inventory::{parse_inventory_json, DeviceInventory, InventoryCommand, LoadError};
use fastlink_gui::state::{AppState, Screen};

fn inventory(json: &str) -> DeviceInventory {
    parse_inventory_json(json).expect("test inventory must parse")
}

fn contract_inventory() -> DeviceInventory {
    inventory(
        r#"{
            "schema_version": 1,
            "devices": [{
                "id": "local",
                "name": "headless-host",
                "os": "linux",
                "architecture": "x86_64",
                "cpu_logical_cores": 8,
                "ram_total_bytes": 17179869184,
                "ram_available_bytes": 8589934592,
                "gpu_devices": [],
                "backend": "cpu",
                "network_interfaces": []
            }],
            "status": "local_only"
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
            Ok(contract_inventory())
        })
        .expect("worker must start");

    assert_eq!(state.screen(), Screen::Loading);
    render_resting_frames(&mut state);

    release.send(()).expect("release worker");
    wait_for_result(&mut state);
    assert_eq!(state.screen(), Screen::Devices);
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
    state.apply_result(Ok(contract_inventory()));
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
        r#"{"schema_version":1,"devices":[],"status":"local_only"}"#,
    )));

    assert_eq!(state.screen(), Screen::Devices);
    assert!(state.inventory().unwrap().devices.is_empty());
    render_resting_frames(&mut state);
}

#[test]
fn unknown_values_and_opt_in_render_headlessly() {
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state.apply_result(Ok(inventory(
        r#"{
            "schema_version": 1,
            "devices": [
                {
                    "id": "local",
                    "name": null,
                    "os": null,
                    "architecture": null,
                    "cpu_logical_cores": null,
                    "ram_total_bytes": null,
                    "ram_available_bytes": null,
                    "gpu_devices": null,
                    "backend": null,
                    "network_interfaces": null
                },
                {
                    "id": "peer-a",
                    "name": "peer-a",
                    "gpu_devices": [{"vendor": "example"}],
                    "network_interfaces": ["eth0"]
                }
            ],
            "status": null
        }"#,
    )));

    assert_eq!(state.selected_id(), Some("local"));
    assert!(state.toggle_opt_in("local"));
    assert!(state.is_opted_in("local"));
    assert!(state.select("peer-a"));
    assert_eq!(state.selected_id(), Some("peer-a"));
    render_resting_frames(&mut state);
}

#[test]
fn narrow_window_and_long_values_do_not_panic() {
    let long_name = "n".repeat(600);
    let long_id = "i".repeat(200);
    let json = format!(
        r#"{{
            "schema_version": 1,
            "devices": [{{
                "id": "{long_id}",
                "name": "{long_name}",
                "os": "linux",
                "architecture": "x86_64",
                "cpu_logical_cores": 256,
                "ram_total_bytes": 18446744073709551615,
                "ram_available_bytes": 1,
                "gpu_devices": [{{"vendor": "example", "detail": "{long_name}"}}],
                "backend": "cpu",
                "network_interfaces": ["{long_name}"]
            }}],
            "status": "local_only"
        }}"#
    );
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state.apply_result(Ok(inventory(&json)));

    let ctx = Context::default();
    let size = Vec2::new(640.0, 480.0);
    render(&ctx, &mut state, size);
    render(&ctx, &mut state, size);
}
