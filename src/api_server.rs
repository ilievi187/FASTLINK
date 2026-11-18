//! Configuration and telemetry for FASTLINK's native llama.cpp API.

use std::fmt;
use std::io::Write;
use std::net::SocketAddr;

use zeroize::Zeroizing;

#[derive(Clone)]
pub struct ApiKey(Zeroizing<String>);

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

impl ApiKey {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        if !(16..=256).contains(&value.len())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(
                "Use an API key of 16–256 letters, numbers, dots, underscores, or hyphens.",
            );
        }
        Ok(Self(Zeroizing::new(value.to_owned())))
    }

    pub fn generate() -> Result<String, getrandom::Error> {
        let mut random = [0u8; 32];
        getrandom::fill(&mut random)?;
        Ok(crate::hex(&random))
    }

    pub(crate) fn bearer_header(&self) -> Zeroizing<String> {
        Zeroizing::new(format!("Authorization: Bearer {}\r\n", self.0.as_str()))
    }

    pub(crate) fn key_file(&self) -> std::io::Result<tempfile::NamedTempFile> {
        let mut file = tempfile::NamedTempFile::new()?;
        writeln!(file, "{}", self.0.as_str())?;
        file.flush()?;
        Ok(file)
    }
}

#[derive(Clone, Debug)]
pub struct ApiServerOptions {
    pub bind: SocketAddr,
    pub model_id: String,
    pub key: ApiKey,
    pub auto_offload: bool,
    pub context_size: u32,
}

impl ApiServerOptions {
    pub fn new(host: &str, port: u16, model_id: &str, key: ApiKey) -> Result<Self, &'static str> {
        let host = host
            .parse::<std::net::IpAddr>()
            .map_err(|_| "Enter a numeric loopback host, such as 127.0.0.1 or ::1.")?;
        if !host.is_loopback() || port == 0 {
            return Err("Choose a loopback host and a nonzero API port. Use a TLS reverse proxy for remote clients.");
        }
        if model_id.is_empty()
            || model_id.len() > 96
            || !model_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(
                "Model ID must contain 1–96 letters, numbers, dots, underscores, or hyphens.",
            );
        }
        Ok(Self {
            bind: SocketAddr::new(host, port),
            model_id: model_id.to_owned(),
            key,
            auto_offload: false,
            context_size: 8192,
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.bind)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ApiMetrics {
    pub active_requests: Option<u64>,
    pub queued_requests: Option<u64>,
    pub generated_tokens: Option<u64>,
    pub decode_tokens_per_second: Option<f64>,
    pub prefill_tokens_per_second: Option<f64>,
}

pub fn parse_metrics(text: &str) -> ApiMetrics {
    let mut metrics = ApiMetrics::default();
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let (Some(name), Some(value)) = (fields.next(), fields.next()) else {
            continue;
        };
        let rate = || {
            value
                .parse::<f64>()
                .ok()
                .filter(|rate| rate.is_finite() && *rate >= 0.0)
        };
        match name {
            "llamacpp:requests_processing" => metrics.active_requests = value.parse().ok(),
            "llamacpp:requests_deferred" => metrics.queued_requests = value.parse().ok(),
            "llamacpp:tokens_predicted_total" => metrics.generated_tokens = value.parse().ok(),
            "llamacpp:predicted_tokens_seconds" => metrics.decode_tokens_per_second = rate(),
            "llamacpp:prompt_tokens_seconds" => metrics.prefill_tokens_per_second = rate(),
            _ => {}
        }
    }
    metrics
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelBuffer {
    pub device: String,
    pub mib: f64,
}

pub fn parse_model_buffers(log: &str) -> Vec<ModelBuffer> {
    let mut devices = std::collections::BTreeMap::new();
    for line in log.lines() {
        let Some((description, size)) = line.split_once("model buffer size =") else {
            continue;
        };
        let Some(device) = description.split_ascii_whitespace().next_back() else {
            continue;
        };
        let Some(size) = size
            .trim()
            .strip_suffix(" MiB")
            .and_then(|size| size.trim().parse::<f64>().ok())
        else {
            continue;
        };
        if size.is_finite() && size > 0.0 {
            devices.insert(device.to_owned(), size);
        }
    }
    devices
        .into_iter()
        .map(|(device, mib)| ModelBuffer { device, mib })
        .collect()
}

/// Run a headless API server. RPC endpoints are local SSH-forwarded listeners.
pub fn run_api_cli(args: &[String]) -> std::io::Result<()> {
    use std::io::{self, Read};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use crate::llama_rpc::{LlamaRpcConfig, ManagedLlamaServer};

    const USAGE: &str = "fastlink api --server PATH --model FILE.gguf --api-key-file FILE [--host 127.0.0.1] [--port 8080] [--model-id fastlink] [--rpc 127.0.0.1:PORT]... [--ctx 8192] [--auto-offload]";
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message.to_owned());
    if args == ["--help"] {
        println!("{USAGE}");
        return Ok(());
    }
    let mut flags = std::collections::HashMap::new();
    let mut rpc_servers = Vec::new();
    let mut auto_offload = false;
    let mut args = args.iter().map(String::as_str);
    while let Some(flag) = args.next() {
        if flag == "--auto-offload" {
            if auto_offload {
                return Err(invalid("Duplicate --auto-offload option."));
            }
            auto_offload = true;
            continue;
        }
        if !matches!(
            flag,
            "--server"
                | "--model"
                | "--api-key-file"
                | "--host"
                | "--port"
                | "--model-id"
                | "--rpc"
                | "--ctx"
        ) {
            return Err(invalid(USAGE));
        }
        let value = args
            .next()
            .filter(|value| !value.starts_with("--"))
            .ok_or_else(|| invalid("Missing API option value."))?;
        if flag == "--rpc" {
            if rpc_servers.len() == 16 {
                return Err(invalid("Configure at most 16 RPC workers."));
            }
            if rpc_servers.iter().any(|endpoint| endpoint == value) {
                return Err(invalid("Duplicate RPC worker."));
            }
            rpc_servers.push(value.to_owned());
        } else if flags.insert(flag, value).is_some() {
            return Err(invalid("Duplicate API option."));
        }
    }
    let required = |flag| flags.get(flag).copied().ok_or_else(|| invalid(USAGE));
    let server_bin = PathBuf::from(required("--server")?).canonicalize()?;
    let model_path = PathBuf::from(required("--model")?).canonicalize()?;
    if !model_path.is_file()
        || !model_path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("gguf"))
    {
        return Err(invalid("Select a local .gguf file."));
    }
    let key_path = required("--api-key-file")?;
    if !std::fs::metadata(key_path)?.is_file() {
        return Err(invalid("Select a regular API key file."));
    }
    let mut key_text = Zeroizing::new(String::new());
    std::fs::File::open(key_path)?
        .take(260)
        .read_to_string(&mut key_text)?;
    let key = ApiKey::parse(key_text.trim_end_matches(['\r', '\n'])).map_err(invalid)?;
    if key_text.len() >= 260 {
        return Err(invalid("The API key file must contain one key."));
    }
    let port = flags
        .get("--port")
        .unwrap_or(&"8080")
        .parse()
        .map_err(|_| invalid("Use a numeric API port."))?;
    let ctx_size = flags
        .get("--ctx")
        .unwrap_or(&"8192")
        .parse::<u32>()
        .ok()
        .filter(|size| *size > 0)
        .ok_or_else(|| invalid("Use a positive context size."))?;
    let mut api = ApiServerOptions::new(
        flags.get("--host").unwrap_or(&"127.0.0.1"),
        port,
        flags.get("--model-id").unwrap_or(&"fastlink"),
        key,
    )
    .map_err(invalid)?;
    api.auto_offload = auto_offload || !rpc_servers.is_empty();
    api.context_size = ctx_size;
    let logs = tempfile::tempdir()?;
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&cancel);
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed)).map_err(io::Error::other)?;
    let mut server = ManagedLlamaServer::spawn_api(
        &LlamaRpcConfig {
            server_bin,
            model_path,
            listen_host: api.bind.ip().to_string(),
            listen_port: port,
            n_gpu_layers: if api.auto_offload { 999 } else { 0 },
            rpc_servers,
            ctx_size,
            tensor_split: None,
            fastlink_v4: false,
            fastlink_pack: false,
            fastlink_rle: false,
            log_path: Some(logs.path().join("llama-server.log")),
        },
        &api,
    )
    .map_err(io::Error::other)?;
    match server.wait_ready_with_cancel(Duration::from_secs(600), &cancel) {
        Err(crate::llama_rpc::LlamaRpcError::StreamCancelled) => return Ok(()),
        result => result.map_err(io::Error::other)?,
    }
    println!(
        "FASTLINK API ready at {} (model {})",
        api.base_url(),
        api.model_id
    );
    std::io::stdout().flush()?;
    while !cancel.load(Ordering::Relaxed) {
        if !server.is_running()? {
            return Err(io::Error::other("The llama.cpp coordinator exited."));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_header_safe_and_redacted() {
        let secret = "example-key-for-testing";
        let key = ApiKey::parse(secret).unwrap();
        assert!(!format!("{key:?}").contains(secret));
        assert!(ApiKey::parse("secret\r\nX-Extra: value").is_err());
        assert_eq!(
            key.bearer_header().as_str(),
            format!("Authorization: Bearer {secret}\r\n")
        );
    }

    #[test]
    fn api_rejects_external_binds_and_uses_ipv6_url_brackets() {
        let key = ApiKey::parse("example-key-for-testing").unwrap();
        assert!(ApiServerOptions::new("0.0.0.0", 8080, "fastlink", key.clone()).is_err());
        assert!(ApiServerOptions::new("127.0.0.1", 0, "fastlink", key.clone()).is_err());
        assert_eq!(
            ApiServerOptions::new("::1", 8080, "fastlink", key)
                .unwrap()
                .base_url(),
            "http://[::1]:8080/v1"
        );
    }

    #[test]
    fn model_buffers_use_final_nonzero_allocations() {
        let log = "CPU model buffer size = 512.0 MiB\nRPC0[127.0.0.1:4321] model buffer size = 0.00 MiB\nRPC0[127.0.0.1:4321] model buffer size = 1978.25 MiB\nother model buffer size = NaN MiB";
        assert_eq!(
            parse_model_buffers(log),
            [
                ModelBuffer {
                    device: "CPU".into(),
                    mib: 512.0
                },
                ModelBuffer {
                    device: "RPC0[127.0.0.1:4321]".into(),
                    mib: 1978.25
                },
            ]
        );
    }

    #[test]
    fn metrics_distinguish_missing_values_and_real_zero() {
        let metrics = parse_metrics("# comment\nllamacpp:requests_processing 0\nllamacpp:requests_deferred 2\nllamacpp:predicted_tokens_seconds 9.75\nllamacpp:prompt_tokens_seconds NaN\n");
        assert_eq!(metrics.active_requests, Some(0));
        assert_eq!(metrics.queued_requests, Some(2));
        assert_eq!(metrics.decode_tokens_per_second, Some(9.75));
        assert_eq!(metrics.prefill_tokens_per_second, None);
        assert_eq!(metrics.generated_tokens, None);
    }
}
