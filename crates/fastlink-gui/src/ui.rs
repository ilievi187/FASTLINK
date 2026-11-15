//! egui rendering for the FASTLINK local-device shell.
//!
//! `draw` takes a plain [`egui::Ui`], so the exact same code path can be
//! rendered headlessly in tests through [`egui::Context::run_ui`].

use eframe::egui::{self, Align, Color32, Frame, Layout, Margin, RichText, ScrollArea, Stroke, Ui};

use crate::inventory::Device;
use crate::state::{AppState, Screen};
use crate::view;

const ACCENT: Color32 = Color32::from_rgb(96, 186, 255);
const OK: Color32 = Color32::from_rgb(94, 214, 143);

/// Draw the whole application inside the root UI.
pub fn draw(ui: &mut Ui, state: &mut AppState) {
    egui::Panel::top("fastlink_top_bar").show(ui, |ui| top_bar(ui, state));
    egui::Panel::bottom("fastlink_footer").show(ui, |ui| footer(ui, state));
    egui::Panel::left("fastlink_workspace")
        .resizable(false)
        .default_size(304.0)
        .min_size(250.0)
        .show(ui, |ui| workspace_panel(ui, state));
    egui::CentralPanel::default().show(ui, |ui| central_panel(ui, state));
}

fn top_bar(ui: &mut Ui, state: &mut AppState) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new("FASTLINK").size(24.0).strong());
            ui.label(
                RichText::new("Local device overview · pre-alpha")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        });

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let busy = state.loader().is_busy();
            let refresh =
                ui.add_enabled(!busy, egui::Button::new(RichText::new("Refresh").strong()));
            if refresh.clicked() {
                state.refresh();
            }
            if busy {
                ui.spinner();
            }

            let accent = accent_color(ui);
            let warn = ui.visuals().warn_fg_color;
            let muted = ui.visuals().weak_text_color();
            if let Some(inventory) = state.inventory() {
                let count = inventory.devices.len();
                let noun = if count == 1 { "device" } else { "devices" };
                ui.label(RichText::new(format!("{count} {noun}")).small().weak());
                let color = status_color(ui, inventory.status.as_deref());
                badge(
                    ui,
                    &view::status_headline(inventory.status.as_deref()),
                    color,
                );
            } else if busy {
                badge(ui, "Loading", accent);
            } else if state.last_error().is_some() {
                badge(ui, "Inventory error", warn);
            } else {
                badge(ui, "Not loaded", muted);
            }
        });
    });
    ui.add_space(8.0);
}

fn accent_color(ui: &Ui) -> Color32 {
    if ui.visuals().dark_mode {
        ACCENT
    } else {
        Color32::from_rgb(21, 101, 192)
    }
}

fn ok_color(ui: &Ui) -> Color32 {
    if ui.visuals().dark_mode {
        OK
    } else {
        Color32::from_rgb(21, 115, 71)
    }
}

fn status_color(ui: &Ui, status: Option<&str>) -> Color32 {
    match status.map(str::trim) {
        Some("local_only") => ok_color(ui),
        Some("") | None => ui.visuals().weak_text_color(),
        Some(_) => accent_color(ui),
    }
}

fn workspace_panel(ui: &mut Ui, state: &AppState) {
    ui.add_space(10.0);
    ui.label(
        RichText::new("WORKSPACE")
            .small()
            .strong()
            .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(6.0);

    let non_local = state.inventory().map(|inventory| {
        inventory
            .devices
            .iter()
            .filter(|device| !device.is_local())
            .count()
    });
    let (peer_status, peer_body) = match non_local {
        None => (
            "Unknown until inventory loads".to_owned(),
            "Refresh to read what the local CLI reports. Pairing is not implemented in this milestone.",
        ),
        Some(0) => (
            "No paired peers".to_owned(),
            "Peering is not implemented yet. Nothing on the network is contacted by this window.",
        ),
        Some(count) => (
            format!(
                "{count} non-local device{} reported",
                if count == 1 { "" } else { "s" }
            ),
            "Pairing status is unknown. This build reports inventory but does not pair devices.",
        ),
    };
    empty_block(ui, "Paired peers", &peer_status, peer_body);

    empty_block(
        ui,
        "Models",
        "None linked",
        "No model catalog, download, or transfer exists in this milestone.",
    );
    empty_block(
        ui,
        "Chat",
        "Not available",
        "Inference and chat are not implemented. This shell only shows device inventory.",
    );

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(6.0);
    ui.label(RichText::new("Safety").strong());
    ui.label(
        RichText::new(
            "Inventory is read-only. The shell only runs the command shown in the footer. \
             It does not scan the network, open listening sockets, download models, run \
             inference, or start GPU compute.",
        )
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(6.0);
    ui.label(
        RichText::new(
            "Opt-in is session-only and explicit. Closing the window discards it; no pairing \
             or consent is persisted.",
        )
        .small()
        .color(ui.visuals().weak_text_color()),
    );
}

fn empty_block(ui: &mut Ui, title: &str, headline: &str, body: &str) {
    ui.add_space(4.0);
    Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(8)
        .inner_margin(10)
        .show(ui, |ui| {
            ui.label(RichText::new(title).strong());
            ui.label(
                RichText::new(headline)
                    .small()
                    .color(ui.visuals().warn_fg_color),
            );
            ui.label(
                RichText::new(body)
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        });
}

fn central_panel(ui: &mut Ui, state: &mut AppState) {
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(4.0);
            ui.heading("Devices");
            ui.label(
                RichText::new(
                    "Every value below comes from the fastlink devices --json contract. \
                     Missing or null values are shown as Unknown and are never estimated.",
                )
                .small()
                .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(10.0);

            match state.screen() {
                Screen::Idle => idle_view(ui, state),
                Screen::Loading => loading_view(ui, state),
                Screen::Error => error_view(ui, state),
                Screen::Devices => devices_view(ui, state),
            }
        });
}

fn idle_view(ui: &mut Ui, state: &mut AppState) {
    Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(10)
        .inner_margin(18)
        .show(ui, |ui| {
            ui.label(RichText::new("No inventory loaded yet").size(18.0).strong());
            ui.label(
                RichText::new(
                    "Refresh to invoke the FASTLINK CLI and read the local device contract. \
                     Nothing is collected unless you ask.",
                )
                .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(8.0);
            if ui.button("Refresh device inventory").clicked() {
                state.refresh();
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("Source: {}", state.command().command_line()))
                    .monospace()
                    .small()
                    .weak(),
            );
        });
}

fn loading_view(ui: &mut Ui, state: &AppState) {
    Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(10)
        .inner_margin(18)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new("Reading device inventory…")
                        .size(18.0)
                        .strong(),
                );
            });
            ui.label(
                RichText::new("Waiting for the CLI. The read is bounded by a timeout.")
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new(state.command().command_line())
                    .monospace()
                    .small()
                    .weak(),
            );
        });
}

fn error_view(ui: &mut Ui, state: &mut AppState) {
    let error = state.last_error().cloned();
    let warn = ui.visuals().warn_fg_color;
    Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(Stroke::new(1.0, warn.gamma_multiply(0.7)))
        .corner_radius(10)
        .inner_margin(18)
        .show(ui, |ui| {
            ui.label(
                RichText::new("Could not read device inventory")
                    .size(18.0)
                    .strong(),
            );
            if let Some(error) = &error {
                ui.label(RichText::new(&error.summary).color(warn));
                if let Some(detail) = &error.detail {
                    ui.add_space(6.0);
                    Frame::new()
                        .fill(ui.visuals().extreme_bg_color)
                        .corner_radius(6)
                        .inner_margin(8)
                        .show(ui, |ui| {
                            ui.label(RichText::new(detail).monospace().small());
                        });
                }
                if let Some(hint) = &error.hint {
                    ui.add_space(6.0);
                    ui.label(RichText::new("What to try").small().strong());
                    ui.label(
                        RichText::new(hint)
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                }
            }
            ui.add_space(8.0);
            if ui.button("Try again").clicked() {
                state.refresh();
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("Source: {}", state.command().command_line()))
                    .monospace()
                    .small()
                    .weak(),
            );
        });
}

fn devices_view(ui: &mut Ui, state: &mut AppState) {
    if let Some(error) = state.last_error().cloned() {
        stale_banner(ui, &error);
        ui.add_space(8.0);
    }

    // Clone the small device list so the cards can mutate selection/opt-in
    // while the rest of the snapshot is read-only.
    let (devices, status, schema_version) = {
        let inventory = state
            .inventory()
            .expect("Screen::Devices always has an inventory snapshot");
        (
            inventory.devices.clone(),
            inventory.status.clone(),
            inventory.schema_version,
        )
    };

    ui.label(
        RichText::new(format!(
            "schema v{schema_version} · {} · {} reported by {}",
            view::status_headline(status.as_deref()),
            count_label(devices.len()),
            state.command().command_line()
        ))
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(10.0);

    if devices.is_empty() {
        Frame::new()
            .fill(ui.visuals().faint_bg_color)
            .corner_radius(10)
            .inner_margin(18)
            .show(ui, |ui| {
                ui.label(RichText::new("No devices reported").size(18.0).strong());
                ui.label(
                    RichText::new(
                        "The inventory command returned schema v1 with an empty devices array. \
                         This window does not fabricate a device.",
                    )
                    .color(ui.visuals().weak_text_color()),
                );
                ui.add_space(8.0);
                if ui.button("Refresh").clicked() {
                    state.refresh();
                }
            });
        return;
    }

    for device in &devices {
        device_card(ui, state, device);
    }
}

fn count_label(count: usize) -> String {
    let noun = if count == 1 { "device" } else { "devices" };
    format!("{count} {noun}")
}

fn stale_banner(ui: &mut Ui, error: &crate::inventory::LoadError) {
    let warn = ui.visuals().warn_fg_color;
    Frame::new()
        .fill(warn.gamma_multiply(0.12))
        .stroke(Stroke::new(1.0, warn.gamma_multiply(0.6)))
        .corner_radius(8)
        .inner_margin(10)
        .show(ui, |ui| {
            ui.label(
                RichText::new("Showing the last successful snapshot")
                    .strong()
                    .color(warn),
            );
            ui.label(RichText::new(&error.summary).small());
            if let Some(hint) = &error.hint {
                ui.label(
                    RichText::new(hint)
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
        });
}

fn device_card(ui: &mut Ui, state: &mut AppState, device: &Device) {
    let selected = state.selected_id() == Some(device.id.as_str());
    let accent = accent_color(ui);
    let border = if selected {
        Stroke::new(1.5, accent)
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    };

    Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(border)
        .corner_radius(10)
        .inner_margin(14)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    let title = RichText::new(view::device_title(device.name.as_deref()))
                        .size(20.0)
                        .strong();
                    let response = ui
                        .add(egui::Label::new(title).sense(egui::Sense::click()))
                        .on_hover_text("Select this device");
                    if response.clicked() {
                        state.select(&device.id);
                    }
                    ui.label(
                        RichText::new(format!("id: {}", device.id))
                            .monospace()
                            .small()
                            .weak(),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    if selected {
                        badge(ui, "Selected", accent);
                    } else if ui.small_button("Select").clicked() {
                        state.select(&device.id);
                    }
                    ui.label(
                        RichText::new(view::device_role(device.is_local()))
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                });
            });

            ui.add_space(8.0);
            let ram = view::ram_summary(device.ram_total_bytes, device.ram_available_bytes);
            egui::Grid::new(format!("fastlink_device_{}", device.id))
                .num_columns(2)
                .spacing([18.0, 6.0])
                .show(ui, |ui| {
                    field(
                        ui,
                        "Operating system",
                        &view::optional_text(device.os.as_deref()),
                        device.os.is_some(),
                    );
                    field(
                        ui,
                        "Architecture",
                        &view::optional_text(device.architecture.as_deref()),
                        device.architecture.is_some(),
                    );
                    field(
                        ui,
                        "CPU",
                        &view::cpu_label(device.cpu_logical_cores),
                        device.cpu_logical_cores.is_some(),
                    );
                    field(
                        ui,
                        "RAM total",
                        &ram.total,
                        device.ram_total_bytes.is_some(),
                    );
                    field(
                        ui,
                        "RAM available",
                        &ram.available,
                        device.ram_available_bytes.is_some(),
                    );
                    let used = ram.used_percent.map_or_else(
                        || view::UNKNOWN.to_owned(),
                        |percent| format!("{percent}% used"),
                    );
                    field(ui, "RAM in use", &used, ram.used_percent.is_some());
                    field(
                        ui,
                        "Compute backend",
                        &view::backend_label(device.backend.as_deref()),
                        device.backend.is_some(),
                    );
                    field(
                        ui,
                        "GPU devices",
                        &view::items_summary(device.gpu_devices.as_deref()),
                        device.gpu_devices.is_some(),
                    );
                    field(
                        ui,
                        "Network interfaces",
                        &view::items_summary(device.network_interfaces.as_deref()),
                        device.network_interfaces.is_some(),
                    );
                });

            if let Some(note) = ram.note {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(note)
                        .small()
                        .color(ui.visuals().warn_fg_color),
                );
            }

            entries_block(ui, "GPU entries reported", device.gpu_devices.as_deref());
            entries_block(
                ui,
                "Network entries reported",
                device.network_interfaces.as_deref(),
            );

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let mut opted_in = state.is_opted_in(&device.id);
                if ui
                    .checkbox(&mut opted_in, RichText::new(view::OPT_IN_LABEL).strong())
                    .changed()
                {
                    state.set_opt_in(&device.id, opted_in);
                }
                if opted_in {
                    badge(ui, "Session opt-in", ok_color(ui));
                }
            });
            ui.label(
                RichText::new(view::OPT_IN_DETAIL)
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        });
    ui.add_space(10.0);
}

fn field(ui: &mut Ui, label: &str, value: &str, known: bool) {
    ui.label(
        RichText::new(label)
            .small()
            .color(ui.visuals().weak_text_color()),
    );
    if known {
        ui.label(value);
    } else {
        ui.label(RichText::new(value).italics().weak());
    }
    ui.end_row();
}

fn entries_block(ui: &mut Ui, title: &str, items: Option<&[serde_json::Value]>) {
    let entries = view::item_entries(items);
    if entries.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.label(RichText::new(title).small().strong());
    for entry in entries {
        ui.label(RichText::new(entry).monospace().small());
    }
}

fn badge(ui: &mut Ui, text: &str, color: Color32) {
    Frame::new()
        .fill(color.gamma_multiply(0.16))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.55)))
        .corner_radius(255)
        .inner_margin(Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().strong().color(color));
        });
}

fn footer(ui: &mut Ui, state: &AppState) {
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        match state.loaded_age() {
            Some(age) => {
                ui.label(
                    RichText::new(format!("Updated {}", view::format_age(age)))
                        .small()
                        .weak(),
                );
            }
            None => {
                ui.label(RichText::new("Not loaded").small().weak());
            }
        }
        ui.label(
            RichText::new("·")
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        ui.label(
            RichText::new(format!("Source: {}", state.command().command_line()))
                .monospace()
                .small()
                .weak(),
        );
        ui.label(
            RichText::new("·")
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        ui.label(
            RichText::new(format!("Opted in this session: {}", state.opted_in_count()))
                .small()
                .weak(),
        );
    });
    ui.add_space(2.0);
    ui.label(
        RichText::new(
            "No network scans, listening sockets, downloads, inference, or GPU compute. \
             Window rendering uses the normal OpenGL driver.",
        )
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(4.0);
}
