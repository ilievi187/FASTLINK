//! egui rendering for the FASTLINK local-device shell.
//!
//! `draw` takes a plain [`egui::Ui`], so the exact same code path can be
//! rendered headlessly in tests through [`egui::Context::run_ui`].

use std::path::PathBuf;

use eframe::egui::{self, Align, Color32, Frame, Layout, Margin, RichText, ScrollArea, Stroke, Ui};

use crate::discovery::{CandidateNode, DiscoveryInventory, ProviderStatus, DISCOVERY_SCOPE};
use crate::inventory::GpuDevice;
use crate::models::redacted_path;
use crate::state::{AppState, Screen};
use crate::view::{self, FieldRow};

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
                RichText::new("Local devices, opt-in discovery, session GGUF catalog · pre-alpha")
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
                let count = inventory.nodes.len();
                let noun = if count == 1 { "node" } else { "nodes" };
                ui.label(RichText::new(format!("{count} {noun}")).small().weak());
                let color = status_color(ui, Some(inventory.scope.as_str()));
                badge(ui, &view::scope_headline(Some(&inventory.scope)), color);
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

fn status_color(ui: &Ui, scope: Option<&str>) -> Color32 {
    match scope.map(str::trim) {
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
        let local_index = inventory.local_node_index();
        inventory
            .nodes
            .iter()
            .enumerate()
            .filter(|(index, _)| local_index != Some(*index))
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
                "{count} non-local node{} reported",
                if count == 1 { "" } else { "s" }
            ),
            "Pairing status is unknown. This build reports local inventory only and does not pair nodes.",
        ),
    };
    empty_block(ui, "Paired peers", &peer_status, peer_body);

    let registered = state.model_catalog().entries().len();
    let (model_status, model_body) = if registered == 0 {
        (
            "None registered".to_owned(),
            "Enter one .gguf path in the catalog. Nothing is scanned, copied, or loaded.",
        )
    } else {
        (
            format!("{registered} registered"),
            "Select and confirm a GGUF here, then load it in Chat.",
        )
    };
    empty_block(ui, "Models", &model_status, model_body);

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(6.0);
    ui.label(RichText::new("Safety").strong());
    ui.label(
        RichText::new(
            "Local inventory is read-only. Network discovery runs only after an explicit \
             Discover devices click. Chat loads weights after an explicit Load model (CPU) click.",
        )
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(6.0);
    ui.label(
        RichText::new(
            "Opt-in is session-only, applies to the local node, and is never persisted. \
             Closing the window discards it; no pairing or consent is stored.",
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

            model_catalog_section(ui, state);
            discovery_section(ui, state);
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

    // Clone the small node list so cards can mutate selection/opt-in while the
    // rest of the snapshot is read-only.
    let (nodes, schema, schema_version, scope, discovery_mode, local_index) = {
        let inventory = state
            .inventory()
            .expect("Screen::Devices always has an inventory snapshot");
        (
            inventory.nodes.clone(),
            inventory.schema.clone(),
            inventory.schema_version,
            inventory.scope.clone(),
            inventory.discovery_mode.clone(),
            inventory.local_node_index(),
        )
    };

    ui.label(
        RichText::new(format!(
            "{} · {} · {} {} · discovery: {} · {}",
            view::schema_headline(&schema, schema_version),
            view::scope_headline(Some(&scope)),
            nodes.len(),
            if nodes.len() == 1 { "node" } else { "nodes" },
            view::discovery_label(discovery_mode.as_deref()),
            state.command().command_line()
        ))
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(6.0);
    if scope == crate::inventory::LOCAL_ONLY_SCOPE && nodes.len() == 1 {
        ui.label(
            RichText::new(
                "The sole node in scope local_only is treated as the local device. Its node_id is an opaque backend identifier.",
            )
            .small()
            .color(ui.visuals().weak_text_color()),
        );
    }
    ui.add_space(10.0);

    if nodes.is_empty() {
        Frame::new()
            .fill(ui.visuals().faint_bg_color)
            .corner_radius(10)
            .inner_margin(18)
            .show(ui, |ui| {
                ui.label(RichText::new("No nodes reported").size(18.0).strong());
                ui.label(
                    RichText::new(
                        "The inventory command returned a local_only snapshot with an empty nodes array. \
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

    for (index, node) in nodes.iter().enumerate() {
        node_card(ui, state, node, local_index == Some(index));
    }
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

fn node_card(
    ui: &mut Ui,
    state: &mut AppState,
    node: &crate::inventory::InventoryNode,
    is_local: bool,
) {
    let selected = node
        .node_id
        .as_deref()
        .is_some_and(|id| state.selected_id() == Some(id));
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
                    let title = RichText::new(view::node_title(node.hostname.as_deref()))
                        .size(20.0)
                        .strong();
                    let can_select = node.node_id.is_some();
                    let title = ui.add(
                        egui::Label::new(title)
                            .sense(if can_select {
                                egui::Sense::click()
                            } else {
                                egui::Sense::hover()
                            }),
                    );
                    if can_select {
                        let title = title.on_hover_text("Select this node");
                        if title.clicked() {
                            if let Some(id) = node.node_id.as_deref() {
                                state.select(id);
                            }
                        }
                    }
                    let id_row = if node.node_id.is_some() {
                        RichText::new(format!(
                            "node_id: {}",
                            view::opaque_id_label(node.node_id.as_deref())
                        ))
                        .monospace()
                        .small()
                        .weak()
                    } else {
                        RichText::new("node_id: Unknown")
                            .monospace()
                            .small()
                            .italics()
                            .weak()
                    };
                    ui.label(id_row);
                });
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    if selected {
                        badge(ui, "Selected", accent);
                    } else if node.node_id.is_some() && ui.small_button("Select").clicked() {
                        if let Some(id) = node.node_id.as_deref() {
                            state.select(id);
                        }
                    }
                    ui.label(
                        RichText::new(view::node_role(is_local))
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                });
            });

            if node.node_id.is_none() {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "The CLI did not derive a node_id. Selection and opt-in stay unavailable; no identifier is invented.",
                    )
                    .small()
                    .color(ui.visuals().warn_fg_color),
                );
            }

            ui.add_space(8.0);
            let ram = view::ram_summary(
                node.ram_total_bytes,
                node.ram_available_bytes,
                node.ram_free_bytes,
            );
            egui::Grid::new(format!(
                "fastlink_node_{}",
                node.node_id.as_deref().unwrap_or("unknown")
            ))
            .num_columns(2)
            .spacing([18.0, 6.0])
            .show(ui, |ui| {
                for row in view::node_rows(node) {
                    field_row(ui, &row);
                }
                field(ui, "RAM total", &ram.total, node.ram_total_bytes.is_some());
                field(
                    ui,
                    "RAM available",
                    &ram.available,
                    node.ram_available_bytes.is_some(),
                );
                field(ui, "RAM free", &ram.free, node.ram_free_bytes.is_some());
                let used = ram.used_percent.map_or_else(
                    || view::UNKNOWN.to_owned(),
                    |percent| format!("{percent}% used"),
                );
                field(ui, "RAM in use", &used, ram.used_percent.is_some());
            });

            if let Some(note) = ram.note {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(note)
                        .small()
                        .color(ui.visuals().warn_fg_color),
                );
            }

            gpu_section(ui, node);
            capabilities_section(ui, node);
            unknown_fields_section(ui, node);

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);
            if is_local {
                let local_id = node.node_id.as_deref();
                let mut opted_in = local_id.is_some_and(|id| state.is_opted_in(id));
                let enabled = local_id.is_some();
                if ui
                    .add_enabled(
                        enabled,
                        egui::Checkbox::new(
                            &mut opted_in,
                            RichText::new(view::OPT_IN_LABEL).strong(),
                        ),
                    )
                    .changed()
                {
                    if let Some(id) = local_id {
                        state.set_opt_in(id, opted_in);
                    }
                }
                if opted_in {
                    badge(ui, "Session opt-in", ok_color(ui));
                }
                ui.label(
                    RichText::new(view::OPT_IN_DETAIL)
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            } else {
                ui.label(
                    RichText::new("Opt-in is available only for the local node.")
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
        });
    ui.add_space(10.0);
}

fn gpu_section(ui: &mut Ui, node: &crate::inventory::InventoryNode) {
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("GPUs").strong());
        ui.label(
            RichText::new(view::gpu_count_label(&node.gpus))
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    });
    if node.gpus.is_empty() {
        ui.label(
            RichText::new("No GPU metadata was reported by the CLI.")
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        return;
    }
    for (index, gpu) in node.gpus.iter().enumerate() {
        gpu_entry(ui, index, gpu);
    }
}

fn gpu_entry(ui: &mut Ui, index: usize, gpu: &GpuDevice) {
    ui.add_space(4.0);
    Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .corner_radius(6)
        .inner_margin(8)
        .show(ui, |ui| {
            ui.label(RichText::new(view::gpu_title(index, gpu)).strong());
            egui::Grid::new(format!("fastlink_gpu_{index}"))
                .num_columns(2)
                .spacing([14.0, 4.0])
                .show(ui, |ui| {
                    for row in view::gpu_rows(gpu) {
                        field_row(ui, &row);
                    }
                });
        });
}

fn capabilities_section(ui: &mut Ui, node: &crate::inventory::InventoryNode) {
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Capabilities").strong());
        ui.label(
            RichText::new("reported values only")
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    });
    egui::Grid::new(format!(
        "fastlink_capabilities_{}",
        node.node_id.as_deref().unwrap_or("unknown")
    ))
    .num_columns(2)
    .spacing([14.0, 4.0])
    .show(ui, |ui| {
        for row in view::capability_rows(&node.capabilities) {
            field_row(ui, &row);
        }
        for peer in &node.capabilities.remote_peers {
            ui.label(
                RichText::new("Reported peer")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
            ui.label(RichText::new(peer).monospace().small());
            ui.end_row();
        }
    });
    if !view::connectivity_known(node.capabilities.network_connectivity.as_deref()) {
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Network speed and connectivity were not measured; they remain Unknown and are not inferred.",
            )
            .small()
            .color(ui.visuals().weak_text_color()),
        );
    }
}

fn unknown_fields_section(ui: &mut Ui, node: &crate::inventory::InventoryNode) {
    if node.unknown_fields.is_empty() {
        return;
    }
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Unknown fields reported by the CLI").strong());
        ui.label(
            RichText::new(view::unknown_fields_count(&node.unknown_fields))
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    });
    for field in view::unknown_field_entries(&node.unknown_fields) {
        ui.label(RichText::new(field).monospace().small());
    }
}

fn model_catalog_section(ui: &mut Ui, state: &mut AppState) {
    ui.add_space(18.0);
    ui.separator();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.heading("GGUF catalog");
        badge(ui, "Session only", accent_color(ui));
    });
    ui.label(
        RichText::new(
            "Enter or paste one existing readable .gguf path. The shell records metadata only: \
             it does not copy the file, read weight bytes, scan a directory, or persist the path. \
             Confirming a load does not load a model.",
        )
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(6.0);

    let mut draft = state.model_catalog().draft().to_owned();
    let editor = ui.add(
        egui::TextEdit::singleline(&mut draft)
            .hint_text("One .gguf path")
            .desired_width(f32::INFINITY),
    );
    if editor.changed() {
        state.model_catalog_mut().set_draft(&draft);
    }
    let busy = state.model_fs_busy();
    if busy {
        ui.label(
            RichText::new("Checking that path on a background worker.")
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    }
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !busy,
                egui::Button::new(RichText::new("Register GGUF path").strong()),
            )
            .clicked()
        {
            state.model_catalog_mut().set_draft(&draft);
            state.request_register_model();
        }
        if ui
            .add_enabled(!busy, egui::Button::new("Remove selected"))
            .clicked()
        {
            state.remove_selected_model();
        }
        let can_confirm = !busy && selected_model_path(state).is_some();
        if ui
            .add_enabled(can_confirm, egui::Button::new("Confirm load"))
            .clicked()
        {
            state.request_confirm_model_load();
        }
    });

    if let Some(error) = state.model_catalog().error() {
        ui.add_space(4.0);
        ui.label(
            RichText::new(error.summary())
                .small()
                .color(ui.visuals().warn_fg_color),
        );
    }

    let entries = state.model_catalog().entries().to_vec();
    let selected = state.model_catalog().selected_index();
    let confirmation = state.model_catalog().load_confirmation().map(PathBuf::from);
    ui.add_space(6.0);
    if entries.is_empty() {
        ui.label(
            RichText::new("No GGUF path is registered in this session.")
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    }
    for (index, entry) in entries.iter().enumerate() {
        ui.push_id(index, |ui| {
            Frame::new()
                .fill(ui.visuals().faint_bg_color)
                .corner_radius(8)
                .inner_margin(10)
                .show(ui, |ui| {
                    ui.label(RichText::new(&entry.file_name).strong());
                    ui.label(
                        RichText::new(redacted_path(&entry.path))
                            .monospace()
                            .small(),
                    );
                    ui.label(
                        RichText::new(format!("{} bytes", entry.size_bytes))
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                    if selected == Some(index) {
                        badge(ui, "Selected", accent_color(ui));
                    } else if ui.add_enabled(!busy, egui::Button::new("Select")).clicked() {
                        state.select_registered_model(index);
                    }
                });
        });
        ui.add_space(4.0);
    }
    if let Some(path) = confirmation {
        ui.add_space(4.0);
        ui.label(format!(
            "Confirmed {}. Open Chat to load it.",
            redacted_path(&path)
        ));
    }
    ui.add_space(8.0);
}

fn selected_model_path(state: &AppState) -> Option<PathBuf> {
    let index = state.model_catalog().selected_index()?;
    state
        .model_catalog()
        .entries()
        .get(index)
        .map(|entry| entry.path.clone())
}

fn discovery_section(ui: &mut Ui, state: &mut AppState) {
    ui.add_space(18.0);
    ui.separator();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.heading("Network discovery");
        badge(ui, "Opt-in", accent_color(ui));
    });
    ui.label(
        RichText::new(format!(
            "Scope: {} ({DISCOVERY_SCOPE})",
            view::discovery_scope_label(DISCOVERY_SCOPE)
        ))
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    ui.label(
        RichText::new(
            "Nothing is scanned on launch or when local inventory is refreshed. Clicking \
             Discover devices runs fastlink discover --json and may contact your LAN/Tailscale \
             providers. Candidates remain unpaired and unverified.",
        )
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(6.0);

    let busy = state.discovery_busy();
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !busy,
                egui::Button::new(RichText::new("Discover devices").strong()),
            )
            .clicked()
        {
            state.discover();
        }
        if busy {
            ui.spinner();
            let cancelling = state.discovery_cancelling();
            ui.label(
                RichText::new(if cancelling {
                    "Cancelling discovery…"
                } else {
                    "Discovering (bounded by the CLI timeout)…"
                })
                .small()
                .color(ui.visuals().weak_text_color()),
            );
            if ui
                .add_enabled(!cancelling, egui::Button::new("Cancel"))
                .clicked()
            {
                state.cancel_discovery();
            }
        }
    });

    if let Some(age) = state.discovery_age() {
        ui.label(
            RichText::new(format!("Last discovery attempt: {}", view::format_age(age)))
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    }
    ui.label(
        RichText::new(format!("Source: {}", state.discovery_command_line()))
            .monospace()
            .small()
            .weak(),
    );

    if let Some(error) = state.discovery_error() {
        ui.add_space(6.0);
        let warn = ui.visuals().warn_fg_color;
        Frame::new()
            .fill(warn.gamma_multiply(0.12))
            .stroke(Stroke::new(1.0, warn.gamma_multiply(0.6)))
            .corner_radius(8)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.label(
                    RichText::new("Discovery failed safely")
                        .strong()
                        .color(warn),
                );
                ui.label(RichText::new(&error.summary).small());
                ui.label(
                    RichText::new(
                        "No addresses, endpoints, provider error text or candidates are shown.",
                    )
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
            });
    }

    if let Some(discovery) = state.discovery().cloned() {
        discovery_results(ui, &discovery);
    } else if !busy && state.discovery_error().is_none() {
        Frame::new()
            .fill(ui.visuals().faint_bg_color)
            .corner_radius(8)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.label(
                    RichText::new("Discovery is idle")
                        .strong()
                        .color(ui.visuals().weak_text_color()),
                );
                ui.label(
                    RichText::new(
                        "No discovery command has run in this session. Use Discover devices to opt in explicitly.",
                    )
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
            });
    }
    ui.add_space(8.0);
}

fn discovery_results(ui: &mut Ui, discovery: &DiscoveryInventory) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Providers").strong());
        ui.label(
            RichText::new(view::reported_count(discovery.providers.len()))
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    });
    if discovery.providers.is_empty() {
        ui.label(
            RichText::new("No providers were reported. This does not prove no providers exist.")
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    } else {
        for provider in &discovery.providers {
            let (color, text) = provider_badge(ui, provider.status);
            Frame::new()
                .fill(ui.visuals().faint_bg_color)
                .corner_radius(6)
                .inner_margin(8)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&provider.provider).monospace().strong());
                        badge(ui, text, color);
                    });
                    if provider.status == ProviderStatus::Unavailable {
                        ui.label(
                            RichText::new(
                                "Unavailable. The raw provider error is intentionally not displayed.",
                            )
                            .small()
                            .color(ui.visuals().weak_text_color()),
                        );
                    }
                });
            ui.add_space(4.0);
        }
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Candidates").strong());
        ui.label(
            RichText::new(view::reported_count(discovery.nodes.len()))
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    });
    ui.label(
        RichText::new(view::candidate_unpaired_warning())
            .small()
            .color(ui.visuals().warn_fg_color),
    );
    if discovery.nodes.is_empty() {
        ui.label(
            RichText::new(
                "No candidates were reported. This is not evidence that no devices exist.",
            )
            .small()
            .color(ui.visuals().weak_text_color()),
        );
        return;
    }
    for (index, candidate) in discovery.nodes.iter().enumerate() {
        discovery_candidate_card(ui, index, candidate);
    }
}

fn provider_badge(ui: &Ui, status: ProviderStatus) -> (Color32, &'static str) {
    match status {
        ProviderStatus::Available => (ok_color(ui), view::provider_status_label(status)),
        ProviderStatus::Unavailable => (
            ui.visuals().warn_fg_color,
            view::provider_status_label(status),
        ),
        ProviderStatus::Unknown => (
            ui.visuals().weak_text_color(),
            view::provider_status_label(status),
        ),
    }
}

fn discovery_candidate_card(ui: &mut Ui, index: usize, candidate: &CandidateNode) {
    let warn = ui.visuals().warn_fg_color;
    Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(Stroke::new(1.0, warn.gamma_multiply(0.45)))
        .corner_radius(10)
        .inner_margin(12)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(view::candidate_title(candidate.display_name.as_deref()))
                        .size(18.0)
                        .strong(),
                );
                badge(ui, "Unpaired · unverified", warn);
            });
            ui.label(
                RichText::new(format!(
                    "node_id: {}",
                    view::opaque_id_label(candidate.node_id.as_deref())
                ))
                .monospace()
                .small()
                .weak(),
            );
            ui.add_space(6.0);
            egui::Grid::new(format!(
                "fastlink_discovery_{index}_{}",
                candidate.node_id.as_deref().unwrap_or("unknown")
            ))
            .num_columns(2)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                for row in view::candidate_rows(candidate) {
                    field_row(ui, &row);
                }
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(view::candidate_unpaired_warning())
                    .small()
                    .color(warn),
            );
            if view::candidate_uses_tailscale(candidate) {
                ui.label(
                    RichText::new(view::candidate_tailnet_warning())
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
            ui.label(
                RichText::new("Read-only candidate. No Pair action exists in this build.")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        });
    ui.add_space(8.0);
}

fn field_row(ui: &mut Ui, row: &FieldRow) {
    ui.label(
        RichText::new(row.label)
            .small()
            .color(ui.visuals().weak_text_color()),
    );
    if row.known {
        ui.label(&row.value);
    } else {
        ui.label(RichText::new(&row.value).italics().weak());
    }
    ui.end_row();
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
        ui.label(RichText::new("·").small().weak());
        ui.label(
            RichText::new(format!("Source: {}", state.command().command_line()))
                .monospace()
                .small()
                .weak(),
        );
        ui.label(RichText::new("·").small().weak());
        ui.label(
            RichText::new(format!("Opted in this session: {}", state.opted_in_count()))
                .small()
                .weak(),
        );
    });
    ui.add_space(2.0);
    ui.label(
        RichText::new(
            "No automatic network scans, listening sockets, downloads, inference, or GPU compute. \
             A GGUF path is registered only when you enter one, and only as session metadata. \
             Discovery runs only after an explicit click. Window rendering uses the normal OpenGL driver.",
        )
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(4.0);
}
