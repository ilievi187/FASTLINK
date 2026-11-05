# Security policy — pre-alpha

There is **no security guarantee** for FASTLINK v0.1. The current TCP protocol is **unencrypted and unauthenticated**; the CLI refuses to bind the receiving server to a non-loopback interface.

**Do not expose its port to untrusted networks.** A remote experiment requires an encrypted and authenticated SSH tunnel or equivalent secure transport. Do not send secrets or confidential datasets.

The server accepts filenames only as basenames, bounds all input sizes and codecs, validates hashes, and writes through temporary output files. These are defensive checks, not a formal audit. Current gaps include resource quotas, concurrent transactions, robust crash-resume, symlink and filesystem race-hardening, OS sandboxing, and encrypted peer authentication.

Please report security problems privately through the repository owner before filing a public issue with exploitable details. The README and documentation are not a substitute for a security audit.
