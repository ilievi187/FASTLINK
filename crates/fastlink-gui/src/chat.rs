use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use eframe::egui;
use fastlink::llama_rpc::{
    spawn_completion_stream, CompletionEvent, CompletionRequest, CompletionStreamHandle,
    CompletionStreamOptions, LlamaCompletionMetrics, LlamaRpcConfig, ManagedLlamaServer,
};
use fastlink_gui::models::{inspect_gguf_path, RegisteredGguf};

struct Session {
    server: ManagedLlamaServer,
    _logs: tempfile::TempDir,
    model: PathBuf,
}

struct Startup {
    cancel: Arc<AtomicBool>,
    worker: JoinHandle<Result<Session, String>>,
}

#[derive(Default)]
pub(crate) struct Chat {
    executable: String,
    startup: Option<Startup>,
    session: Option<Session>,
    stream: Option<CompletionStreamHandle>,
    prompt: String,
    output: String,
    metrics: Option<LlamaCompletionMetrics>,
    status: String,
}

impl Chat {
    pub(crate) fn poll(&mut self) -> bool {
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
                            Err(error) => error.to_string(),
                        };
                        break;
                    }
                }
            }
        }
        if let Some(session) = &mut self.session {
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
        self.startup.is_some() || self.session.is_some()
    }

    pub(crate) fn draw(&mut self, ui: &mut egui::Ui, model: Option<&RegisteredGguf>) {
        ui.heading("Local chat");
        let active = self.startup.is_some() || self.session.is_some();
        ui.horizontal(|ui| {
            ui.label("llama-server executable");
            ui.add_enabled(
                !active,
                egui::TextEdit::singleline(&mut self.executable).hint_text("/path/to/llama-server"),
            );
            if ui
                .add_enabled(
                    !active && model.is_some() && !self.executable.trim().is_empty(),
                    egui::Button::new("Load model (CPU)"),
                )
                .clicked()
            {
                self.start(model.unwrap().clone());
            }
            if ui
                .add_enabled(active, egui::Button::new("Stop server"))
                .clicked()
            {
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
        ui.label(&self.status);
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
                let address = self.session.as_ref().unwrap().server.addr();
                let request = CompletionRequest::new(&self.prompt).with_n_predict(512);
                match spawn_completion_stream(
                    address,
                    &request,
                    &CompletionStreamOptions::default(),
                ) {
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
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.add(egui::Label::new(&self.output).selectable(true));
            });
        if active || self.startup.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }
    }

    fn start(&mut self, model: RegisteredGguf) {
        let executable = PathBuf::from(self.executable.trim());
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        match thread::Builder::new()
            .name("fastlink-model-load".into())
            .spawn(move || start_session(executable, model, &signal))
        {
            Ok(worker) => {
                self.status = "Loading model…".into();
                self.output.clear();
                self.metrics = None;
                self.startup = Some(Startup { cancel, worker });
            }
            Err(error) => self.status = error.to_string(),
        }
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
        self.stop();
        if let Some(job) = self.startup.take() {
            let _ = job.worker.join();
        }
    }
}

fn start_session(
    executable: PathBuf,
    model: RegisteredGguf,
    cancel: &AtomicBool,
) -> Result<Session, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("Startup cancelled".into());
    }
    let fresh = inspect_gguf_path(&model.path).map_err(|error| error.summary().to_owned())?;
    if fresh.opened_identity != model.opened_identity || fresh.identity != model.identity {
        return Err("GGUF changed. Confirm the model again before loading.".into());
    }
    let executable = executable
        .canonicalize()
        .map_err(|error| format!("llama-server executable: {error}"))?;
    let logs = tempfile::tempdir().map_err(|error| error.to_string())?;
    let reservation = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| error.to_string())?;
    let port = reservation
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    if cancel.load(Ordering::Relaxed) {
        return Err("Startup cancelled".into());
    }
    drop(reservation);
    let mut server = ManagedLlamaServer::spawn(&LlamaRpcConfig {
        server_bin: executable,
        model_path: fresh.identity,
        listen_host: "127.0.0.1".into(),
        listen_port: port,
        rpc_servers: Vec::new(),
        n_gpu_layers: 0,
        ctx_size: 4096,
        tensor_split: None,
        fastlink_v4: false,
        fastlink_pack: false,
        fastlink_rle: false,
        log_path: Some(logs.path().join("llama-server.log")),
    })
    .map_err(|error| error.to_string())?;
    server
        .wait_ready_with_cancel(Duration::from_secs(120), cancel)
        .map_err(|error| error.to_string())?;
    Ok(Session {
        server,
        _logs: logs,
        model: model.path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
            let result = start_session(PathBuf::new(), confirmed, &AtomicBool::new(false));
            assert_eq!(
                result.err().unwrap(),
                "GGUF changed. Confirm the model again before loading."
            );
        }
        #[cfg(not(unix))]
        {
            let result = start_session(PathBuf::new(), confirmed, &AtomicBool::new(false));
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
            &AtomicBool::new(true),
        );
        assert_eq!(result.err().unwrap(), "Startup cancelled");
    }
}
