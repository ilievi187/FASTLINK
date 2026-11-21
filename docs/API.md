# FASTLINK inference API

FASTLINK serves one GGUF through the llama.cpp coordinator. The Linux app can
run it on the coordinator CPU, automatically offload to available local
accelerators, or use configured SSH-connected RPC workers. Each worker exposes
the CPU or GPU devices chosen when its RPC server starts. The coordinator uses
llama.cpp's memory fitting and layer placement and transfers tensors to the
selected devices. CPU memory on the coordinator still holds any tensors the
runtime places there. Actual memory usage and speed depend on the model,
context, worker backends, and network.

## Start from the Linux app

1. Add prepared workers in **Devices and models** and register and confirm a
   local GGUF. See [worker setup](GUI.md#distribute-across-trusted-ssh-workers).
2. Open **Chat** and select **Serve OpenAI-compatible API**. Set the listen
   host, port, model ID, context size, and key. An empty key is generated at
   launch. The default URL is `http://127.0.0.1:8080/v1` and the default model
   ID is `fastlink`.
3. Choose **Distribute and load model** for workers or **Load model (CPU)** for
   local inference. For a local accelerator build, explicitly enable
   **Auto offload on local accelerators** to offer the local offload action.
4. Once ready, use **Copy API URL** and **Copy API key** for your client.
   **Stop server** closes the endpoint and the workers and tunnels FASTLINK owns.

The API panel shows llama.cpp's active and queued request counts, generated
tokens, decode and prefill throughput, and model buffer sizes by backend when
reported. Active requests count work in progress, including GUI requests. Idle
connections and client identities are not tracked.

## Endpoints

- `GET /v1/models` lists the configured model ID.
- `POST /v1/chat/completions` accepts OpenAI-style messages. Set
  `"stream": true` for server-sent events.
- `GET /metrics` returns the coordinator's Prometheus metrics.

Send `Authorization: Bearer <API key>` to each endpoint. llama.cpp's Jinja
chat templates handle client-supplied `tools` and `tool_choice` for compatible
GGUFs. Tool behavior depends on the model and template. Applications execute
their own tools. [llama.cpp server documentation](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)

## Headless server

```sh
fastlink api \
  --server /path/to/llama-server \
  --model /path/to/model.gguf \
  --api-key-file /path/to/private-key-file \
  --host 127.0.0.1 --port 8080 --model-id fastlink
```

The key file contains a single secret on one line. Restrict access to that
file. `fastlink api --help` lists options. Add `--ctx TOKEN_COUNT` for the
desired context. Repeat `--rpc 127.0.0.1:LOCAL_PORT` for worker tunnels you
manage separately. Use `--auto-offload` for local GPU offload. Worker endpoints
trigger automatic fitting as well. For explicit layer placement, add
`--tensor-split 1,1,1` with a weight for each selected device, in llama.cpp's
reported device order. These are relative layer weights, not memory budgets.
One RPC worker can expose multiple devices (for example `Vulkan0,CPU`, using
GPU VRAM and system RAM on that host). The pinned b12503 engine accepts up to
16 device weights, which is not the same as 16 mixed-device hosts. Omit the
option to use upstream default placement. Ctrl+C and SIGTERM stop the coordinator.

The authenticated HTTP listener accepts configurable IPv4 or IPv6 loopback
addresses. For applications on other hosts or in a separate container network,
use an authenticated SSH local forward or a TLS reverse proxy to the loopback
listener. For proxied streaming, preserve the Authorization header and disable
response buffering on the proxy. Keep raw llama.cpp RPC on worker loopback.

## Compatible clients

Use the API URL ending in `/v1`, the bearer key, and the exact model ID from
FASTLINK. These clients require a model that meets their context and tool-use
needs.

- **Hermes** — run `hermes model` and choose **Custom endpoint**. Enter the
  URL, API key, and model ID. [Hermes custom providers](https://hermes-agent.nousresearch.com/docs/integrations/providers#custom--self-hosted-llm-providers)
- **OpenCode** — register a custom OpenAI-compatible provider with the API URL
  as its base URL, the key as its credential, and the FASTLINK model ID in
  its model map. [OpenCode custom providers](https://opencode.ai/docs/providers/#custom)
- **Open WebUI** — add an OpenAI-compatible connection in the admin settings.
  Use a URL reachable from the WebUI backend. [Open WebUI compatible providers](https://docs.openwebui.com/getting-started/quick-start/connect-a-provider/starting-with-openai-compatible/)
