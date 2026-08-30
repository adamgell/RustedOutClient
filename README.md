# RustedOutClient

RustedOutClient is a security-focused fork of [IronVNC](https://github.com/hkder/ironvnc),
being narrowed into a hardened Proxmox console client.

This initial foundation intentionally presents only a minimal application shell:
**“Proxmox profile not configured.”** Future work adds typed Proxmox SSH integration
and hardened RFB layers on this preserved renderer, framebuffer, input, and encoding base.

## Build

The repository pins Rust 1.92.0. Build the current shell with:

```bash
cargo build
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
