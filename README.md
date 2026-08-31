# RustedOutClient

RustedOutClient is a native macOS ARM64 Proxmox QEMU console client. It uses the
system OpenSSH client for strict host-verified transport and carries a bounded,
embedded RFB client over the owned SSH process's standard streams. The desktop
workspace supports multiple sessions, keyboard and pointer input, explicit
clipboard transfer, view-only mode, scaling, and guest Dynamic Resolution.

**Status:** unreleased and not accepted for production. The synthetic suite and
local security gates establish implementation readiness only. Native lab,
TigerVNC fallback, rollback, hosted CI, and rollout remain separate gates; the
Task 14B parser-smoke tranche is still parked.

The initial supported target is macOS 26 on ARM64. Direct TCP VNC, arbitrary
remote commands, file transfer, trust bypasses, password entry, and general
Proxmox administration are deliberately absent.

## Five-minute local start

No Proxmox host is required to build, test, or inspect the command surface.
Install the Rust toolchain selected by `rust-toolchain.toml`, then run:

```bash
rustup show
cargo build --locked
cargo test --all-targets --all-features --locked
cargo run --locked -- --help
```

Running `cargo run --locked --` opens the workspace. Host-contacting examples
below are shapes only; they require an approved local configuration and use
synthetic selectors:

```bash
cargo run --locked -- list
cargo run --locked -- probe 100 --json
cargo run --locked -- open "Sample VM" --view-only
```

Configuration lives in the user's private macOS application-support directory.
It stores targeting and UI preferences, never an SSH password, private key, VNC
ticket, guest pixels, or clipboard contents. Authentication and host trust stay
with system OpenSSH. See [Configuration and CLI](docs/configuration.md).

## Console boundaries

- Native RFB accepts VNC Authentication only over the verified SSH proxy. It
  creates no TCP listener and bounds protocol lengths, framebuffer allocation,
  decoder work, clipboard text, and queues.
- Clipboard defaults off and transfers one bounded text value only after an
  explicit direction-specific operator action.
- Dynamic Resolution uses physical backing pixels, rounds dimensions down to
  multiples of eight, debounces viewport changes, permits one request in
  flight, and keeps local Fit available after rejected, unsupported, or timed-
  out guest resize requests.
- **Open in TigerVNC** is a manual, temporary fallback. It snapshots a configured
  viewer, creates process-lifetime private artifacts, and relays one IPv4
  loopback peer to the owned SSH proxy. It is lower-assurance and not yet live-
  accepted.

## Documentation

- [Security policy](SECURITY.md)
- [Threat model](docs/threat-model.md)
- [Configuration and CLI](docs/configuration.md)
- [Native acceptance](docs/native-acceptance.md)
- [Migration and rollback](docs/migration.md)
- [Upstream integration policy](docs/upstream.md)

## License and upstream

RustedOutClient is derived from
[`hkder/ironvnc@999e00e3a3672efdbf8e8f307e7bd60875dee67e`](https://github.com/hkder/ironvnc/commit/999e00e3a3672efdbf8e8f307e7bd60875dee67e).
It is licensed under either the [Apache License, Version 2.0](LICENSE-APACHE) or
the [MIT License](LICENSE-MIT), at your option. See [NOTICE](NOTICE) for
attribution.
