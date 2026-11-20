use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::api_panel::{ApiLive, ApiPanel};
use eframe::egui;
use fastlink::api_server::{parse_model_buffers, ApiServerOptions};
use fastlink::llama_rpc::{
    spawn_authenticated_completion_stream, spawn_completion_stream, CompletionEvent,
    CompletionRequest, CompletionStreamHandle, CompletionStreamOptions, LlamaCompletionMetrics,
    LlamaRpcConfig, ManagedLlamaServer,
};
use fastlink_gui::models::{inspect_gguf_path, RegisteredGguf};

#[cfg(target_os = "linux")]
use crate::workers::{
    build_worker_backend, discovered_worker, merge_workers, ssh_file, SshOptions, Tunnel, Worker,
    WorkerBackendMode,
};

#[cfg(not(target_os = "linux"))]
struct Worker;

#[cfg(not(target_os = "linux"))]
#[derive(Default)]
struct SshOptions {}

struct Session {
    server: ManagedLlamaServer,
    #[cfg(target_os = "linux")]
    tunnels: Vec<Tunnel>,
    _logs: tempfile::TempDir,
    model: PathBuf,
    api: Option<ApiLive>,
}

struct Startup {
    cancel: Arc<AtomicBool>,
    worker: JoinHandle<Result<Session, String>>,
}

#[cfg(target_os = "linux")]
struct AddedDevice {
    id: String,
    name: String,
    address: std::net::Ipv4Addr,
    backend_mode: WorkerBackendMode,
    gpu_device: String,
}

#[cfg(target_os = "linux")]
struct AddDeviceJob {
    cancel: Arc<AtomicBool>,
    worker: JoinHandle<Result<AddedDevice, &'static str>>,
}

#[derive(Default)]
pub(crate) struct Chat {
    #[cfg(target_os = "linux")]
    ssh_user: String,
    #[cfg(target_os = "linux")]
    added_devices: Vec<AddedDevice>,
    #[cfg(target_os = "linux")]
    add_job: Option<AddDeviceJob>,
    #[cfg(target_os = "linux")]
    device_status: String,
    executable: String,
    #[cfg(target_os = "linux")]
    workers: String,
    #[cfg(target_os = "linux")]
    known_hosts: String,
    #[cfg(target_os = "linux")]
    identity_file: String,
    #[cfg(target_os = "linux")]
    start_rpc: bool,
    #[cfg(target_os = "linux")]
    tensor_split: String,
    startup: Option<Startup>,
    session: Option<Session>,
    stream: Option<CompletionStreamHandle>,
    prompt: String,
    output: String,
    metrics: Option<LlamaCompletionMetrics>,
    status: String,
    api: ApiPanel,
}

impl Chat {
    pub(crate) fn new() -> Self {
        let mut chat = Self::default();
        #[cfg(target_os = "linux")]
        {
            chat.ssh_user = std::env::var("USER").unwrap_or_default();
        }
        chat.executable = std::env::var_os("FASTLINK_LLAMA_SERVER")
            .map(PathBuf::from)
            .or_else(|| {
                find_on_path(
                    if cfg!(windows) {
                        "llama-server.exe"
                    } else {
                        "llama-server"
                    }
                    .as_ref(),
                )
            })
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        chat
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn add_device(&mut self, candidate: &fastlink_gui::discovery::CandidateNode) {
        let Some(id) = &candidate.node_id else { return };
        if self.add_job.is_some() || self.startup.is_some() || self.session.is_some() {
            self.device_status = "Finish the current device action or stop the model first.".into();
            return;
        }
        if self.added_devices.iter().any(|device| device.id == *id) {
            self.device_status = "This device is already added.".into();
            return;
        }
        if self.added_devices.len() == 16 {
            self.device_status = "Remove a device before adding another.".into();
            return;
        }
        let id = id.clone();
        let name = candidate
            .display_name
            .clone()
            .unwrap_or_else(|| "Discovered device".into());
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        match thread::Builder::new()
            .name("fastlink-add-device".into())
            .spawn(move || {
                use fastlink::discovery::{resolve_selected_ipv4, DiscoveryConfig};
                let address = resolve_selected_ipv4(&DiscoveryConfig::default(), &id, &signal)?;
                Ok(AddedDevice {
                    id,
                    name,
                    address,
                    backend_mode: WorkerBackendMode::Cpu,
                    gpu_device: "Vulkan0".into(),
                })
            }) {
            Ok(worker) => {
                self.add_job = Some(AddDeviceJob { cancel, worker });
                self.device_status = "Locating selected device…".into();
            }
            Err(error) => self.device_status = error.to_string(),
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn device_status(&self) -> &str {
        &self.device_status
    }

    #[cfg(target_os = "linux")]
    fn poll_added_device(&mut self) -> bool {
        if self
            .add_job
            .as_ref()
            .is_some_and(|job| job.worker.is_finished())
        {
            let job = self.add_job.take().unwrap();
            match job.worker.join() {
                Ok(Ok(device)) => {
                    if self
                        .added_devices
                        .iter()
                        .any(|added| added.address == device.address)
                    {
                        self.device_status = "That SSH address is already added.".into();
                    } else {
                        self.device_status =
                            format!("Added {}. Open Chat to load the model.", device.name);
                        self.added_devices.push(device);
                    }
                }
                Ok(Err(error)) => self.device_status = error.into(),
                Err(_) => self.device_status = "Could not finish adding the device.".into(),
            }
            return true;
        }
        self.add_job.is_some()
    }

    pub(crate) fn poll(&mut self) -> bool {
        #[cfg(target_os = "linux")]
        let adding_device = self.poll_added_device();
        #[cfg(not(target_os = "linux"))]
        let adding_device = false;
        if self
            .startup
            .as_ref()
            .is_some_and(|job| job.worker.is_finished())
        {
            let job = self.startup.take().unwrap();
            match job.worker.join() {
                Ok(Ok(session)) if !job.cancel.load(Ordering::Relaxed) => {
                    self.session = Some(session);
                    self.status = "Model ready".into();
                }
                _ if job.cancel.load(Ordering::Relaxed) => self.status = "Stopped".into(),
                Ok(Err(error)) => self.status = error,
                _ => self.status = "Model startup worker failed".into(),
            }
        }
        if let Some(stream) = &self.stream {
            for _ in 0..128 {
                match stream.events().try_recv() {
                    Ok(CompletionEvent::Delta { text, .. }) => self.output.push_str(&text),
                    Ok(CompletionEvent::Finished { metrics }) => self.metrics = Some(metrics),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        let stream = self.stream.take().unwrap();
                        self.status = match stream.join() {
                            Ok(_) => "Response complete".into(),
                            Err(fastlink::llama_rpc::LlamaRpcError::StreamCancelled) => {
                                "Response cancelled".into()
                            }
                            Err(error) => format!("Prompt failed: {error}"),
                        };
                        break;
                    }
                }
            }
        }
        #[cfg(target_os = "linux")]
        if self.session.as_mut().is_some_and(|session| {
            session
                .tunnels
                .iter_mut()
                .any(|tunnel| !matches!(tunnel.is_running(), Ok(true)))
        }) {
            self.stop();
            self.status =
                "A worker tunnel closed. Check the worker and load the model again.".into();
        }
        if let Some(session) = &mut self.session {
            if let Some(api) = &mut session.api {
                api.refresh();
            }
            match session.server.is_running() {
                Ok(true) => {}
                result => {
                    self.stream = None;
                    self.session = None;
                    self.status = match result {
                        Ok(false) => "llama-server exited. Load the model again to retry.".into(),
                        Err(error) => format!("Could not check llama-server: {error}"),
                        Ok(true) => unreachable!(),
                    };
                }
            }
        }
        adding_device || self.startup.is_some() || self.session.is_some()
    }

    pub(crate) fn draw(&mut self, ui: &mut egui::Ui, model: Option<&RegisteredGguf>) {
        egui::ScrollArea::vertical().show(ui, |ui| self.draw_contents(ui, model));
    }

    fn draw_contents(&mut self, ui: &mut egui::Ui, model: Option<&RegisteredGguf>) {
        ui.heading("Chat");
        let active = self.startup.is_some() || self.session.is_some();
        #[cfg(target_os = "linux")]
        let adding = self.add_job.is_some();
        #[cfg(not(target_os = "linux"))]
        let adding = false;
        if adding {
            ui.label("Locating selected device…");
        }
        #[cfg(target_os = "linux")]
        {
            if !self.added_devices.is_empty() {
                ui.heading("Added devices");
                ui.horizontal(|ui| {
                    ui.label("SSH username");
                    ui.add_enabled(!active, egui::TextEdit::singleline(&mut self.ssh_user));
                });
                let mut remove = None;
                for (index, device) in self.added_devices.iter_mut().enumerate() {
                    ui.push_id(index, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!("{} · {}", device.name, device.address));
                            ui.add_enabled_ui(!active, |ui| {
                                egui::ComboBox::from_id_salt(("worker_backend", index))
                                    .selected_text(device.backend_mode.label())
                                    .width(92.0)
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut device.backend_mode,
                                            WorkerBackendMode::Cpu,
                                            WorkerBackendMode::Cpu.label(),
                                        );
                                        ui.selectable_value(
                                            &mut device.backend_mode,
                                            WorkerBackendMode::Gpu,
                                            WorkerBackendMode::Gpu.label(),
                                        );
                                        ui.selectable_value(
                                            &mut device.backend_mode,
                                            WorkerBackendMode::Both,
                                            WorkerBackendMode::Both.label(),
                                        );
                                    });
                                if device.backend_mode != WorkerBackendMode::Cpu {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut device.gpu_device)
                                            .hint_text("Vulkan0 / CUDA0 / ROCm0")
                                            .desired_width(110.0),
                                    );
                                }
                            });
                            if ui
                                .add_enabled(!active, egui::Button::new("Remove"))
                                .clicked()
                            {
                                remove = Some(index);
                            }
                        });
                    });
                }
                if let Some(index) = remove {
                    self.added_devices.remove(index);
                }
            }
            ui.label("Manual SSH workers (optional)");
            ui.add_enabled(
                !active,
                egui::TextEdit::multiline(&mut self.workers)
                    .hint_text("user@host SSH_PORT RPC_PORT [CPU|Vulkan0|Vulkan0,CPU]"),
            );
            if !self.workers.trim().is_empty() || !self.added_devices.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Layer weights (optional)");
                    ui.add_enabled(
                        !active,
                        egui::TextEdit::singleline(&mut self.tensor_split)
                            .hint_text("Automatic · or 1,1,1")
                            .desired_width(160.0),
                    ).on_hover_text("Relative weights in llama.cpp device order, not GiB. A GPU + CPU worker contributes two devices. Leave blank for upstream fitting.");
                });
                ui.add_enabled(
                    !active,
                    egui::TextEdit::singleline(&mut self.known_hosts)
                        .hint_text("Known-hosts file (optional)")
                        .desired_width(420.0),
                );
                ui.add_enabled(
                    !active,
                    egui::TextEdit::singleline(&mut self.identity_file)
                        .hint_text("SSH private key file (optional)")
                        .desired_width(420.0),
                );
                ui.add_enabled(
                    !active,
                    egui::Checkbox::new(&mut self.start_rpc, "Start managed RPC workers on load"),
                )
                .on_hover_text("Starts ggml-rpc-server over SSH on each Linux worker using its selected backend profile (default CPU). Stop server closes these sessions.");
                if self.start_rpc {
                    ui.label("Each worker needs a matching ggml-rpc-server build for its selected backend and must allow SSH forwarding and a terminal.");
                }
            }
        }
        #[cfg(target_os = "linux")]
        let distributed = !self.workers.trim().is_empty() || !self.added_devices.is_empty();
        #[cfg(not(target_os = "linux"))]
        let distributed = false;
        ui.horizontal(|ui| {
            ui.label("llama-server executable");
            ui.add_enabled(
                !active,
                egui::TextEdit::singleline(&mut self.executable)
                    .hint_text("/path/to/llama-server")
                    .desired_width(360.0),
            );
        });
        self.api.draw(
            ui,
            active,
            self.session
                .as_ref()
                .and_then(|session| session.api.as_ref()),
        );
        let can_load = !active && !adding && model.is_some() && !self.executable.trim().is_empty();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    can_load,
                    egui::Button::new(if self.api.local_offloads() {
                        "Load model (auto offload)"
                    } else {
                        "Load model (CPU)"
                    }),
                )
                .clicked()
            {
                self.start(model.unwrap().clone(), false);
            }
            if distributed
                && ui
                    .add_enabled(can_load, egui::Button::new("Distribute and load model"))
                    .clicked()
            {
                self.start(model.unwrap().clone(), true);
            }
            let stop = if self.startup.is_some() {
                "Cancel load"
            } else {
                "Stop server"
            };
            if ui.add_enabled(active, egui::Button::new(stop)).clicked() {
                self.stop();
            }
        });
        if let Some(path) = self
            .session
            .as_ref()
            .map(|session| session.model.as_path())
            .or(model.map(|entry| entry.path.as_path()))
        {
            ui.label(
                path.file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy(),
            );
        } else {
            ui.label("Register and confirm a GGUF in Devices and models.");
        }
        ui.add(egui::Label::new(&self.status).selectable(true));
        if self.session.is_none() {
            ui.label("Load a model to send a prompt.");
        }
        ui.add_enabled(
            self.session.is_some() && self.stream.is_none(),
            egui::TextEdit::multiline(&mut self.prompt).hint_text("Prompt"),
        );
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.session.is_some()
                        && self.stream.is_none()
                        && !self.prompt.trim().is_empty(),
                    egui::Button::new("Send"),
                )
                .clicked()
            {
                let session = self.session.as_ref().unwrap();
                let address = session.server.addr();
                let request = CompletionRequest::new(&self.prompt).with_n_predict(512);
                let options = CompletionStreamOptions::default();
                let stream = if let Some(api) = &session.api {
                    spawn_authenticated_completion_stream(
                        address,
                        &request,
                        &options,
                        api.options.key.clone(),
                    )
                } else {
                    spawn_completion_stream(address, &request, &options)
                };
                match stream {
                    Ok(stream) => {
                        self.output.clear();
                        self.metrics = None;
                        self.status = "Generating…".into();
                        self.stream = Some(stream);
                    }
                    Err(error) => self.status = error.to_string(),
                }
            }
            if ui
                .add_enabled(self.stream.is_some(), egui::Button::new("Cancel response"))
                .clicked()
            {
                self.stream.as_ref().unwrap().cancel();
                self.status = "Cancelling response…".into();
            }
        });
        if let Some(metrics) = &self.metrics {
            if metrics.predicted_n > 0 && metrics.predicted_ms > 0.0 {
                ui.label(format!(
                    "{:.2} tokens/sec · {} generated tokens",
                    metrics.predicted_n as f64 * 1000.0 / metrics.predicted_ms,
                    metrics.predicted_n
                ));
            }
        }
        egui::ScrollArea::vertical()
            .id_salt("chat_response")
            .max_height(360.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.add(egui::Label::new(&self.output).selectable(true));
            });
        if active || self.startup.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }
    }

    fn start(&mut self, model: RegisteredGguf, distributed: bool) {
        #[cfg(target_os = "linux")]
        let (workers, ssh) = if distributed {
            match self.distributed_settings() {
                Ok(settings) => settings,
                Err(error) => {
                    self.status = error;
                    return;
                }
            }
        } else {
            (Vec::new(), SshOptions::default())
        };
        #[cfg(not(target_os = "linux"))]
        let (workers, ssh) = (Vec::new(), SshOptions {});
        #[cfg(target_os = "linux")]
        let tensor_split = if distributed && !self.tensor_split.trim().is_empty() {
            match fastlink::api_server::parse_tensor_split(&self.tensor_split) {
                Ok(value) => Some(value),
                Err(error) => {
                    self.status = error.into();
                    return;
                }
            }
        } else {
            None
        };
        #[cfg(not(target_os = "linux"))]
        let tensor_split = None;
        let api = match self.api.options(distributed) {
            Ok(api) => api,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        let executable = PathBuf::from(self.executable.trim());
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        match thread::Builder::new()
            .name("fastlink-model-load".into())
            .spawn(move || {
                start_session(executable, model, workers, ssh, api, tensor_split, &signal)
            }) {
            Ok(worker) => {
                self.status = if distributed {
                    "Connecting to SSH workers and loading the model…".into()
                } else {
                    "Loading model locally…".into()
                };
                self.output.clear();
                self.metrics = None;
                self.startup = Some(Startup { cancel, worker });
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    #[cfg(target_os = "linux")]
    fn distributed_settings(&self) -> Result<(Vec<Worker>, SshOptions), String> {
        let added = self
            .added_devices
            .iter()
            .map(|device| {
                let backend = build_worker_backend(device.backend_mode, &device.gpu_device)
                    .map_err(|err| format!("{}: {err}", device.name))?;
                discovered_worker(self.ssh_user.trim(), device.address, backend)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let workers = merge_workers(&self.workers, added.into_iter())?;
        if workers.is_empty() {
            return Err("Add an SSH worker or choose Load model (CPU).".into());
        }
        Ok((
            workers,
            SshOptions {
                known_hosts: ssh_file(self.known_hosts.trim(), "known-hosts")?,
                identity_file: ssh_file(self.identity_file.trim(), "SSH private key")?,
                start_rpc: self.start_rpc,
            },
        ))
    }

    fn stop(&mut self) {
        self.stream = None;
        self.session = None;
        self.status = if let Some(job) = &self.startup {
            job.cancel.store(true, Ordering::Relaxed);
            "Stopping…".into()
        } else {
            "Stopped".into()
        };
    }
}

impl Drop for Chat {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(job) = self.add_job.take() {
            job.cancel.store(true, Ordering::Release);
            let _ = job.worker.join();
        }
        self.stop();
        if let Some(job) = self.startup.take() {
            let _ = job.worker.join();
        }
    }
}

fn find_on_path(name: &std::ffi::OsStr) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn resolve_server(executable: PathBuf) -> Result<PathBuf, String> {
    let executable = if let Ok(path) = executable.strip_prefix("~") {
        let home = std::env::var_os("HOME")
            .ok_or("Cannot resolve ~. Enter the absolute llama-server path.")?;
        PathBuf::from(home).join(path)
    } else if executable.parent() == Some(Path::new("")) {
        let name = executable.as_os_str();
        find_on_path(name)
            .ok_or("llama-server was not found on PATH. Enter its full executable path.")?
    } else {
        executable
    };
    let path = executable
        .canonicalize()
        .map_err(|error| format!("llama-server executable: {error}"))?;
    let metadata = path
        .metadata()
        .map_err(|error| format!("llama-server executable: {error}"))?;
    if !metadata.is_file() {
        return Err("Select the llama-server executable file.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("The selected llama-server file is not executable.".into());
        }
    }
    Ok(path)
}

fn start_session(
    executable: PathBuf,
    model: RegisteredGguf,
    workers: Vec<Worker>,
    ssh: SshOptions,
    api: Option<ApiServerOptions>,
    tensor_split: Option<String>,
    cancel: &AtomicBool,
) -> Result<Session, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("Startup cancelled".into());
    }
    let fresh = inspect_gguf_path(&model.path).map_err(|error| error.summary().to_owned())?;
    if fresh.opened_identity != model.opened_identity || fresh.identity != model.identity {
        return Err("GGUF changed. Confirm the model again before loading.".into());
    }
    let executable = resolve_server(executable)?;
    let logs = tempfile::tempdir().map_err(|error| error.to_string())?;
    #[cfg(target_os = "linux")]
    let tunnels = workers
        .iter()
        .enumerate()
        .map(|(index, worker)| {
            Tunnel::open(
                worker,
                &logs.path().join(format!("ssh-{index}.log")),
                &ssh,
                cancel,
            )
            .map_err(|error| format!("Worker {}: {error}", index + 1))
        })
        .collect::<Result<Vec<_>, _>>()?;
    #[cfg(target_os = "linux")]
    let rpc_servers: Vec<_> = tunnels.iter().map(Tunnel::endpoint).collect();
    #[cfg(not(target_os = "linux"))]
    let rpc_servers: Vec<String> = {
        let _ = ssh;
        debug_assert!(workers.is_empty());
        Vec::new()
    };
    let n_gpu_layers =
        if rpc_servers.is_empty() && !api.as_ref().is_some_and(|api| api.auto_offload) {
            0
        } else {
            999
        };
    let reservation = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| error.to_string())?;
    let port = reservation
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    if cancel.load(Ordering::Relaxed) {
        return Err("Startup cancelled".into());
    }
    drop(reservation);
    let config = LlamaRpcConfig {
        server_bin: executable,
        model_path: fresh.identity,
        listen_host: "127.0.0.1".into(),
        listen_port: port,
        rpc_servers,
        n_gpu_layers,
        ctx_size: 4096,
        tensor_split,
        fastlink_v4: false,
        fastlink_pack: false,
        fastlink_rle: false,
        log_path: Some(logs.path().join("llama-server.log")),
    };
    let mut server = if let Some(api) = &api {
        ManagedLlamaServer::spawn_api(&config, api)
    } else {
        ManagedLlamaServer::spawn(&config)
    }
    .map_err(|error| error.to_string())?;
    server
        .wait_ready_with_cancel(Duration::from_secs(600), cancel)
        .map_err(|error| error.to_string())?;
    let api = api.map(|options| {
        use std::io::Read;
        let mut text = String::new();
        if let Ok(file) = std::fs::File::open(server.log_path()) {
            let _ = file.take(1024 * 1024).read_to_string(&mut text);
        }
        let buffers = parse_model_buffers(&text);
        #[cfg(target_os = "linux")]
        let mut buffers = buffers;
        #[cfg(target_os = "linux")]
        for (index, tunnel) in tunnels.iter().enumerate() {
            let endpoint = tunnel.endpoint();
            for buffer in &mut buffers {
                if buffer.device.ends_with(&format!("[{endpoint}]")) {
                    if let Some((device, _)) = buffer.device.split_once('[') {
                        buffer.device = format!("Worker {} ({device})", index + 1);
                    }
                }
            }
        }
        ApiLive::new(options, buffers)
    });
    Ok(Session {
        server,
        #[cfg(target_os = "linux")]
        tunnels,
        _logs: logs,
        model: model.path,
        api,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn llama_server_selection_rejects_a_directory_and_missing_command() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_server(dir.path().into()).unwrap_err(),
            "Select the llama-server executable file."
        );
        assert!(
            resolve_server("fastlink-no-such-llama-server-fixture".into())
                .unwrap_err()
                .contains("PATH")
        );
    }

    #[test]
    fn chat_renders_a_delta_before_final_decode_metrics() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::sync::mpsc;
        use std::time::Instant;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (finish, wait) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut socket);
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            reader.read_exact(&mut vec![0; length]).unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
            socket
                .write_all(b"data: {\"content\":\"Hello\",\"stop\":false}\n\n")
                .unwrap();
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            socket.write_all(b"data: {\"stop\":true,\"timings\":{\"predicted_n\":2,\"predicted_ms\":250}}\n\n").unwrap();
        });

        let mut chat = Chat::default();
        chat.stream = Some(
            spawn_completion_stream(
                address,
                &CompletionRequest::new("Test"),
                &CompletionStreamOptions::default(),
            )
            .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while chat.output.is_empty() && Instant::now() < deadline {
            chat.poll();
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(chat.output, "Hello");
        assert!(chat.metrics.is_none());
        finish.send(()).unwrap();
        server.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while chat.stream.is_some() && Instant::now() < deadline {
            chat.poll();
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(chat.status, "Response complete");
        let metrics = chat.metrics.as_ref().unwrap();
        assert_eq!(metrics.predicted_n, 2);
        assert_eq!(metrics.predicted_ms, 250.0);
    }

    #[test]
    fn load_rejects_a_replaced_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.gguf");
        fs::write(&path, b"fixture").unwrap();
        let confirmed = inspect_gguf_path(&path).unwrap();
        fs::rename(&path, dir.path().join("renamed.gguf")).unwrap();
        #[cfg(unix)]
        {
            fs::write(&path, b"replacement").unwrap();
            let result = start_session(
                PathBuf::new(),
                confirmed,
                Vec::new(),
                SshOptions::default(),
                None,
                None,
                &AtomicBool::new(false),
            );
            assert_eq!(
                result.err().unwrap(),
                "GGUF changed. Confirm the model again before loading."
            );
        }
        #[cfg(not(unix))]
        {
            let result = start_session(
                PathBuf::new(),
                confirmed,
                Vec::new(),
                SshOptions::default(),
                None,
                None,
                &AtomicBool::new(false),
            );
            assert_eq!(
                result.err().unwrap(),
                "That path does not exist. Nothing was registered."
            );
        }
    }

    #[test]
    fn cancelled_start_never_launches_the_executable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.gguf");
        fs::write(&path, b"fixture").unwrap();
        let model = inspect_gguf_path(&path).unwrap();
        let result = start_session(
            std::env::current_exe().unwrap(),
            model,
            Vec::new(),
            SshOptions::default(),
            None,
            None,
            &AtomicBool::new(true),
        );
        assert_eq!(result.err().unwrap(), "Startup cancelled");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn distributed_load_rejects_invalid_layer_weights_before_starting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.gguf");
        fs::write(&path, b"fixture").unwrap();
        let mut chat = Chat::default();
        chat.workers = "alice@worker 22 50052 Vulkan0,CPU".into();
        chat.tensor_split = "0,0".into();
        chat.start(inspect_gguf_path(&path).unwrap(), true);
        assert!(chat.status.contains("at least one positive"));
        assert!(chat.startup.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn added_devices_default_to_cpu_and_apply_per_worker_opt_in_backends() {
        use crate::workers::WorkerBackend;

        let mut chat = Chat::default();
        chat.ssh_user = "alice".into();
        chat.added_devices = vec![
            AddedDevice {
                id: "node-1".into(),
                name: "cpu-box".into(),
                address: "192.0.2.10".parse().unwrap(),
                backend_mode: WorkerBackendMode::default(),
                gpu_device: "Vulkan0".into(),
            },
            AddedDevice {
                id: "node-2".into(),
                name: "gpu-box".into(),
                address: "192.0.2.11".parse().unwrap(),
                backend_mode: WorkerBackendMode::Gpu,
                gpu_device: "Vulkan0".into(),
            },
            AddedDevice {
                id: "node-3".into(),
                name: "mixed-box".into(),
                address: "192.0.2.12".parse().unwrap(),
                backend_mode: WorkerBackendMode::Both,
                gpu_device: "CUDA0".into(),
            },
        ];

        let (workers, _) = chat.distributed_settings().expect("valid mixed backends");
        assert_eq!(workers.len(), 3);
        assert_eq!(workers[0].backend(), &WorkerBackend::Cpu);
        assert_eq!(workers[1].backend(), &WorkerBackend::Gpu("Vulkan0".into()));
        assert_eq!(
            workers[2].backend(),
            &WorkerBackend::GpuAndCpu("CUDA0".into())
        );

        chat.added_devices[1].gpu_device = "CPU".into();
        let err = chat.distributed_settings().err().unwrap();
        assert!(err.contains("gpu-box"), "{err}");
    }
}
