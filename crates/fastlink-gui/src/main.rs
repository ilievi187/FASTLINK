//! `fastlink-gui` — FASTLINK Linux desktop shell (pre-alpha).
//!
//! First milestone: local device overview and local device management only.

use std::time::Duration;

use eframe::egui;
use fastlink_gui::cli::Options;
use fastlink_gui::inventory::InventoryCommand;
use fastlink_gui::state::AppState;
use fastlink_gui::ui;

fn main() -> eframe::Result {
    let options = match Options::parse(std::env::args_os().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("fastlink-gui: {error}\n\n{}", Options::usage());
            std::process::exit(2);
        }
    };

    let command = match options {
        Options::Run(command) => command,
        Options::Help => {
            println!("{}", Options::usage());
            return Ok(());
        }
        Options::Version => {
            println!("fastlink-gui {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FASTLINK — Local devices")
            .with_inner_size([1120.0, 760.0])
            .with_min_inner_size([780.0, 520.0]),
        ..Default::default()
    };

    eframe::run_native(
        "FASTLINK",
        native_options,
        Box::new(move |cc| {
            configure_style(&cc.egui_ctx);
            Ok(Box::new(GuiApp::new(command)))
        }),
    )
}

fn configure_style(ctx: &egui::Context) {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(theme, |style| {
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(12.0, 6.0);
        });
    }
}

struct GuiApp {
    state: AppState,
}

impl GuiApp {
    fn new(command: InventoryCommand) -> Self {
        let mut state = AppState::new(command);
        state.refresh();
        Self { state }
    }
}

impl eframe::App for GuiApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.state.poll() {
            ctx.request_repaint();
        }
        if self.state.loader().is_busy() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if self.state.loaded_age().is_some() {
            // Keep the relative "Updated …" footer honest without a polling loop.
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui::draw(ui, &mut self.state);
    }
}
