use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui;
use fastlink::api_server::{ApiKey, ApiMetrics, ApiServerOptions, ModelBuffer};
use fastlink::llama_rpc::fetch_api_metrics;
use zeroize::Zeroizing;

pub(crate) struct ApiPanel {
    enabled: bool,
    host: String,
    port: String,
    model_id: String,
    context_size: String,
    key: Zeroizing<String>,
    pub(crate) local_auto_offload: bool,
    key_error: Option<String>,
}

impl Default for ApiPanel {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "127.0.0.1".into(),
            port: "8080".into(),
            model_id: "fastlink".into(),
            context_size: "8192".into(),
            key: Zeroizing::default(),
            local_auto_offload: false,
            key_error: None,
        }
    }
}

impl ApiPanel {
    pub(crate) fn draw(&mut self, ui: &mut egui::Ui, active: bool, live: Option<&ApiLive>) {
        ui.add_enabled(
            !active,
            egui::Checkbox::new(&mut self.enabled, "Serve OpenAI-compatible API"),
        );
        if !self.enabled {
            return;
        }
        ui.add_enabled_ui(!active, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Listen host");
                ui.add(egui::TextEdit::singleline(&mut self.host).desired_width(105.0));
                ui.label("Port");
                ui.add(egui::TextEdit::singleline(&mut self.port).desired_width(60.0));
                ui.label("Model ID");
                ui.add(egui::TextEdit::singleline(&mut self.model_id).desired_width(120.0));
                ui.label("Context tokens");
                ui.add(egui::TextEdit::singleline(&mut self.context_size).desired_width(75.0));
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("API key");
                ui.add(
                    egui::TextEdit::singleline(&mut *self.key)
                        .password(true)
                        .desired_width(195.0),
                );
                if ui.button("Generate key").clicked() {
                    match ApiKey::generate() {
                        Ok(key) => {
                            self.key = Zeroizing::new(key);
                            self.key_error = None;
                        }
                        Err(error) => self.key_error = Some(error.to_string()),
                    }
                }
            });
            ui.checkbox(
                &mut self.local_auto_offload,
                "Auto offload on local accelerators",
            );
        });
        if !self.key.is_empty() && ui.button("Copy API key").clicked() {
            ui.ctx().copy_text(self.key.to_string());
        }
        if let Some(error) = &self.key_error {
            ui.label(error);
        }
        if let Some(live) = live {
            live.draw(ui);
        }
    }

    pub(crate) fn options(
        &mut self,
        distributed: bool,
    ) -> Result<Option<ApiServerOptions>, String> {
        if !self.enabled {
            return Ok(None);
        }
        let port = self
            .port
            .trim()
            .parse()
            .map_err(|_| "Use a numeric API port.".to_owned())?;
        if self.key.is_empty() {
            self.key = Zeroizing::new(ApiKey::generate().map_err(|error| error.to_string())?);
        }
        let key = ApiKey::parse(&self.key).map_err(str::to_owned)?;
        let mut api = ApiServerOptions::new(self.host.trim(), port, self.model_id.trim(), key)
            .map_err(str::to_owned)?;
        api.auto_offload = distributed || self.local_auto_offload;
        api.context_size = self
            .context_size
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|size| *size > 0)
            .ok_or_else(|| "Use a positive context size.".to_owned())?;
        Ok(Some(api))
    }

    pub(crate) fn local_offloads(&self) -> bool {
        self.enabled && self.local_auto_offload
    }
}

pub(crate) struct ApiLive {
    pub(crate) options: ApiServerOptions,
    metrics: Option<ApiMetrics>,
    poll: Option<JoinHandle<Option<ApiMetrics>>>,
    next_poll: Instant,
    devices: Vec<ModelBuffer>,
}

impl ApiLive {
    pub(crate) fn new(options: ApiServerOptions, devices: Vec<ModelBuffer>) -> Self {
        Self {
            options,
            metrics: None,
            poll: None,
            next_poll: Instant::now(),
            devices,
        }
    }

    pub(crate) fn refresh(&mut self) {
        if self.poll.as_ref().is_some_and(|poll| poll.is_finished()) {
            self.metrics = self.poll.take().and_then(|poll| poll.join().ok()).flatten();
            self.next_poll = Instant::now() + Duration::from_secs(2);
        }
        if self.poll.is_none() && Instant::now() >= self.next_poll {
            let addr = self.options.bind;
            let key = self.options.key.clone();
            self.poll = thread::Builder::new()
                .name("fastlink-api-metrics".into())
                .spawn(move || fetch_api_metrics(addr, &key).ok())
                .ok();
            self.next_poll = Instant::now() + Duration::from_secs(2);
        }
    }

    fn draw(&self, ui: &mut egui::Ui) {
        let url = self.options.base_url();
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::Label::new(&url).selectable(true));
            if ui.button("Copy API URL").clicked() {
                ui.ctx().copy_text(url);
            }
            ui.label(format!("Model {}", self.options.model_id));
        });
        if let Some(metrics) = &self.metrics {
            let count = |value: Option<u64>| value.map_or("—".into(), |value| value.to_string());
            let rate =
                |value: Option<f64>| value.map_or("—".into(), |value| format!("{value:.2}"));
            ui.label(format!(
                "Active requests {} · Queued {} · Generated tokens {}",
                count(metrics.active_requests),
                count(metrics.queued_requests),
                count(metrics.generated_tokens)
            ));
            ui.label(format!(
                "Server decode {} tokens/sec · Prefill {} tokens/sec",
                rate(metrics.decode_tokens_per_second),
                rate(metrics.prefill_tokens_per_second)
            ));
        } else {
            ui.label("Waiting for server metrics…");
        }
        if !self.devices.is_empty() {
            egui::CollapsingHeader::new("Model memory placement").show(ui, |ui| {
                for device in &self.devices {
                    ui.label(format!("{}  ·  {:.2} MiB", device.device, device.mib));
                }
            });
        }
    }
}
