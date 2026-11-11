# FLB4 admission bounds — 2026-11-11

The experimental v4 batched tensor transport applies symmetric encoding and decoding limits to prevent malicious frame headers from causing enormous memory allocations:

- Maximum **4,096 tensor operations** in one frame.
- Maximum **65,536 chunk updates** across one frame.
- Maximum **512 MiB total declared tensor bytes** across one frame.
- Maximum **512 MiB total encoded chunk bytes** across one frame.
- Maximum **4,096 bytes** per session, tensor, or worker identifier.
- Nonempty tensors must have matching declared lengths, chunk sizes and chunk counts.
- Buffered decode rejects trailing bytes; streaming read consumes one frame.

Larger transfers require segmentation into multiple batches; these bounds are specific to the experimental FLB4 path and do not change the file-sync CLI. Workers validate sizes and chunk geometry before reserving staging buffers.

These checks do not provide authentication, encryption, or real distributed GPU performance. The experimental server remains loopback-only; do not expose it directly to the network.
