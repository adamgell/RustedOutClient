# Security Policy

## Supported versions

RustedOutClient has not completed native lab acceptance and has no accepted or
supported production release. Repository builds are pre-release evaluation
artifacts, even when local and CI checks pass.

| Version | Supported |
| --- | --- |
| Unreleased repository builds | No; security reports are still welcome |
| Published production releases | None |

This table will be replaced with explicit supported release lines after an
accepted release exists.

## Reporting a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/adamgell/RustedOutClient/security/advisories/new)
for `adamgell/RustedOutClient`. Please do not open a public issue containing
exploit details, credentials, private infrastructure, guest pixels, clipboard
contents, or raw process output. Include the affected commit or release,
platform, impact, preconditions, and the smallest synthetic reproduction that
does not contain private data.

Valid reports are not limited to the examples below. An apparent violation of a
documented boundary in supported code remains reportable even when the related
product feature is otherwise out of scope.

## System scope and initial deployment

The initial product scope is a macOS 26 ARM64 desktop and CLI client that opens a
Proxmox QEMU console through the system `/usr/bin/ssh`, then runs an embedded RFB
client over the owned SSH child's standard streams. Covered components are:

- configuration, inventory cache, and process-owned runtime storage;
- profile, CLI, GUI, diagnostics, and local clipboard boundaries;
- system OpenSSH command construction, host trust, control master, inventory,
  proxy, and child-process lifecycle;
- RFB negotiation, VNC Authentication, decoding, framebuffer delivery, input,
  clipboard, and dynamic resolution;
- the manual TigerVNC fallback's viewer snapshot, password file, one loopback
  relay, and cleanup lifecycle; and
- dependency policy and the merge-blocking macOS CI workflow.

The detailed architecture and residual risks are in
[`docs/threat-model.md`](docs/threat-model.md).

## Attacker-controlled inputs and trust boundaries

Treat profile/configuration values, CLI selectors, local clipboard text, the
configured fallback executable, OpenSSH configuration and known-hosts state,
inventory JSON, every byte returned by the RFB peer, and third-party dependency
updates as boundary inputs. A compromised Proxmox account or node can control
inventory and console protocol responses within the permissions of that
account. A local process running as the same user may be able to observe process
memory or interfere with user-owned files; private modes reduce accidental
disclosure but are not a sandbox against an already-compromised account.

## Security invariants

The following are product requirements, not optional deployment advice:

- Production SSH uses only system `/usr/bin/ssh` with strict host-key checking
  and batch mode. Fixed options set `PreferredAuthentications=publickey` and
  `PubkeyAuthentication=yes`, while GSSAPI, hostbased, password, and keyboard-
  interactive authentication are disabled. Trust and public-key or agent setup
  happen in OpenSSH outside the app; changed or unknown host keys fail closed.
- The app executes only the reviewed control-master, inventory, and per-session
  `qm vncproxy` command shapes. Profile, node, and VM identifiers are validated;
  there is no arbitrary remote command, SFTP, SCP, or file-transfer surface.
- VNC Authentication type 2 is accepted only on the verified SSH proxy type.
  VNC Authentication is not encryption; SSH supplies confidentiality and server
  identity. Security type None, RA2, VeNCrypt, ARD, and unknown modes are not
  accepted.
- Native sessions bind no TCP listener. The manual TigerVNC fallback may bind
  exactly one ephemeral IPv4 listener on `127.0.0.1`, accepts one loopback peer,
  and relays only to its owned verified SSH stream.
- Passwords, VNC tickets, private keys, process-environment snapshots, raw
  stderr, host fingerprints, framebuffer pixels, and clipboard contents are
  never persisted or logged. The fallback's required obfuscated eight-byte
  password file is a mode-0600 process-lifetime exception and must be removed on
  success, failure, cancellation, and drop.
- The private diagnostic event log is limited to one active one-MiB file and one
  backup. It contains only typed lifecycle, VMID, dimension, resize, public
  failure, RFB phase/kind, and OS error-kind values; it never accepts raw
  process, transport, remote-text, clipboard, or framebuffer payloads.
- Server-controlled lengths, dimensions, rectangle counts, encoded payloads,
  text, clipboard text, framebuffer allocation, inventory output, stderr
  capture, and viewer snapshot size are bounded before allocation or use.
- Application and VNC queues are bounded. UI actions are semantic and
  nonblocking; framebuffer pressure is coalesced, while control or cleanup
  pressure remains typed and visible.
- Dynamic Resolution uses backing pixels, minimum and maximum bounds, checked
  pixel limits, multiples-of-eight rounding, 250 ms viewport stabilization, one
  in-flight request, a two-second outcome deadline, and explicit
  Requested/Pending/Applied/Rejected/Unsupported/TimedOut states. Failure is
  nonterminal and local Fit remains available.
- Configuration/cache directories and runtime directories are private. Durable
  configuration and cache replacement uses private temporary files and atomic
  rename; runtime tickets, sockets, snapshots, and password files have one
  process owner and bounded lifetimes.
- Every SSH child, VNC task, fallback viewer, listener, temporary file, and
  runtime artifact has exact ownership. Cleanup is bounded and reports loss of
  cleanup truth; the app never searches for or terminates unrelated processes.

## Severity and reportability context

Impact depends on the violated boundary and deployment preconditions:

- Critical or High candidates include credential/ticket/private-key disclosure,
  host-verification bypass, arbitrary command execution, non-loopback listener
  exposure, authentication downgrade, persistent guest-pixel or clipboard
  disclosure, or unbounded malicious-RFB memory/cpu exhaustion.
- High or Medium candidates include same-user temporary-file substitution,
  failure to reap an owned child, cross-session input or clipboard delivery,
  bypass of view-only state, resize storms, or reliable process-level denial of
  service from a supported remote peer.
- Lower-severity issues may include bounded availability failures, inaccurate
  redacted diagnostics, or defense-in-depth gaps with no demonstrated boundary
  crossing.

These examples calibrate triage only. Report uncertain impact privately rather
than withholding it.

## Out-of-scope product functionality

The initial product does not provide direct TCP VNC, arbitrary VNC host/port or
password entry, saved VNC sessions, SFTP, SCP, file transfer, remote filesystem
browsing, VM power control, snapshots, migration, storage/network management,
LXC, SPICE, noVNC, RDP, or general Proxmox administration. A path that
accidentally introduces one of these surfaces is in scope as a boundary
violation.

## Known limitations and compensating controls

- Native and fallback live acceptance has not been executed for an accepted
  release. Synthetic tests and local builds establish readiness, not live
  operational acceptance.
- The parser-smoke tranche and its workflow remain deferred with Task 14B, so
  the complete native acceptance gate is not ready.
- `ttf-parser` 0.25.1 is unmaintained through
  `owned_ttf_parser -> ab_glyph -> epaint -> egui`. No compatible maintained
  replacement exists on the accepted egui/eframe 0.31 line. CI keeps the warning
  visible; review is due by 2027-02-28.
- `quick-xml` 0.39.4 remains in Cargo metadata through the unsupported
  `eframe -> egui-winit -> smithay-clipboard -> smithay-client-toolkit ->
  wayland-scanner -> quick-xml` Linux/Wayland path. It is absent from the
  supported `aarch64-apple-darwin` all-feature graph. CI generates that complete
  locked graph without tree prefixes and fails closed before ignoring only
  `RUSTSEC-2026-0194` and `RUSTSEC-2026-0195` in `cargo audit`; review is due by
  2027-02-28.
- TigerVNC fallback is manual, temporary, and explicitly lower-assurance than
  the embedded path. It requires a configured absolute viewer executable and
  does not count as accepted until its separate live gate passes.

See [`docs/native-acceptance.md`](docs/native-acceptance.md) for the gates that
must distinguish local/CI readiness, native lab acceptance, fallback acceptance,
rollback proof, and rollout.
