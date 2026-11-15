# M2 Discovery Contract (Linux, opt-in)

Status: implementation contract for the first M2 discovery slice. This is not evidence that remote discovery, pairing, or inference already works.

## User and privacy boundary

- Discovery starts only after an explicit user action (`Discover devices` in the GUI or `fastlink discover` in a terminal). Never scan on app startup or in the background.
- The only permitted scope is the user's LAN and Tailscale network. No port-range scans, subnet sweeps, or fallback probes.
- Raw private addresses may be held transiently by the discovery/connection layer only. Never serialize, log, persist, display, commit, or publish them. Keep endpoint fields out of the public JSON types; do not rely solely on UI redaction.
- A discovered candidate is untrusted until the peer proves its identity through the secure pairing protocol. Candidate metadata must not enter the scheduler or model-placement plan.
- A tailnet peer is not automatically a FASTLINK worker. Do not label it worker-ready unless an advertised FASTLINK service is verified.

## Initial command/UI contract

The GUI invokes `fastlink discover --json` only when its Discover button is clicked. The CLI emits one bounded JSON document with this shape (nullable fields are explicit `null`; parsers may ignore additional fields):

```json
{
  "schema": "fastlink.discover.v1",
  "scope": "user_opt_in_lan_tailscale",
  "providers": [
    {"provider":"lan_mdns","status":"available","message":null},
    {"provider":"tailscale","status":"unavailable","message":"Tailscale CLI unavailable"}
  ],
  "nodes": [
    {
      "node_id":"opaque-id",
      "display_name":"Example device",
      "os":null,
      "arch":null,
      "logical_cpu_count":null,
      "ram_total_bytes":null,
      "ram_available_bytes":null,
      "gpus":[],
      "transports":["lan"],
      "worker_state":"unverified",
      "trust_state":"unpaired"
    }
  ]
}
```

A node may expose a display name, opaque node ID, verified platform/resource fields (if advertised), transport labels (`lan` or `tailscale`), and an explicit trust/worker state. Unknown measurements stay `null` or empty; never infer performance, GPU, memory, or worker readiness from a hostname or a tailnet listing.

No `ip`, `addr`, `endpoint`, MAC, or raw Tailscale peer address field may appear in serialized output. Error messages must name the provider and safe failure class without echoing command output that can contain addresses. Bound provider runtime and output size.

The GUI must show the opt-in scope and that unpaired metadata is unverified, provide a cancel/refresh path, and never imply that a generic Tailscale peer is a FASTLINK worker. Discovery must remain idle on launch and on refresh of the local inventory view.

## M2 completion gates

1. Unit tests cover valid/invalid provider records, output-schema redaction, malformed/oversized output, unavailable providers, timeouts, and cancellation.
2. An isolated loopback worker test proves the real discovery-to-pairing path without broadcasting on the LAN or allocating a GPU.
3. Pairing authenticates both peers and establishes encrypted transport before any model, prompt, activation, or KV data can be sent. Existing keyed-BLAKE3 frame MACs are integrity protection, not transport encryption; they do not by themselves satisfy this gate.
4. Removal revokes local trust and credentials; reconnect does not silently re-pair.
5. No real distributed model inference or performance claim until an explicitly approved, safe hardware/model setup produces tokens across two distinct hosts.
