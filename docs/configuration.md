# Configuration and CLI Guide

RustedOutClient is an unreleased macOS ARM64 Proxmox console client. This guide
documents the schema and commands implemented at this repository head. It does
not imply that native or TigerVNC live acceptance has passed.

## Configuration file

The application reads:

```text
~/Library/Application Support/RustedOutClient/config.json
```

The application directory is mode `0700`; `config.json` is mode `0600`.
Updates are written to a private temporary file, synchronized, and atomically
renamed into place. The configuration contains targeting and UI preferences,
which can still identify private infrastructure even though it contains no
credential. Do not print or publish a real file.

The current schema is:

```json
{
  "schema_version": 1,
  "profile": {
    "name": "Example Proxmox",
    "ssh_target": "operator@example.invalid",
    "node": "pve-sample"
  },
  "inventory_refresh_seconds": 15,
  "fallback_viewer": null,
  "clipboard_enabled": false,
  "favorites": [
    {
      "vmid": 100,
      "alias": "Sample VM",
      "scale_mode": "fit",
      "view_only": false,
      "sort_position": 0
    }
  ],
  "display": {
    "scale_mode": "fit",
    "view_only": false
  }
}
```

`schema_version` must be `1`. The profile `name` is a display label.
`ssh_target` must be 1 to 255 printable, non-whitespace ASCII characters and
cannot begin with `-`. `node` is 1 to 64 characters, begins with an ASCII
letter or digit, and otherwise contains only ASCII letters, digits, `_`, `-`,
or `.`. VMIDs are in the range `100..=99_999_999`. Inventory refresh is in the
range 5 to 300 seconds.

Supported scale values are `fit` and `one_to_one`. The fallback viewer is
either `null` or an absolute path to the viewer executable.

The application never stores an SSH password, private key, VNC ticket, VNC
password, host fingerprint, guest pixels, clipboard contents, or raw process
output. Authentication comes from a system OpenSSH public key or agent. Fixed
runtime options enable only public-key authentication and disable GSSAPI,
hostbased, password, and keyboard-interactive authentication. Establish the
intended host key in the user's OpenSSH `known_hosts` through a trusted external
process before using the app. Runtime SSH uses strict host-key checking and
fails closed for unknown or changed keys; RustedOutClient does not offer an
accept-new or trust-bypass switch.

### Legacy import boundary

The source includes a narrow importer that can propose only `ssh_target`,
`node`, and the fallback viewer from an existing helper configuration. It does
not import a password or ticket. The application does not automatically run
that importer at startup. Until an operator-facing import flow is accepted,
create or review the new configuration explicitly and keep the old file
read-only as described in [Migration and rollback](migration.md).

### Inventory preload boundary

The application keeps a non-secret `inventory-v1.json` cache beside
`config.json`. The shared application directory must remain mode `0700`, and an
existing cache must be a regular mode-`0600` file; replacement uses a private
mode-`0600` temporary file and atomic rename. Inventory still identifies private
infrastructure, so do not print or publish the cache.

At configured startup, a valid cached inventory is marked stale and displayed
immediately. The session manager then starts or verifies its one owned OpenSSH
ControlMaster, performs a live inventory refresh, replaces the cache, and marks
the new inventory live. Loading or refreshing this inventory does not run `qm
vncproxy`, create a VNC ticket, or start an RFB/framebuffer session in the
background.

Opening a VM is a separate explicit action. Before ticket creation, the owned
master is rechecked, fresh inventory is fetched, and the selected VM is proved
present and running. A stale preload therefore improves workspace startup but
never authorizes a console or supplies the final VM state used to open one.

## Commands

Running without a subcommand opens the native workspace:

```bash
cargo run --locked --
```

The command-line surface is deliberately narrow:

```text
rustedoutclient list
rustedoutclient probe <selector> [--timeout-seconds <1..300>] [--json]
rustedoutclient open <selector> [--fullscreen] [--view-only]
                     [--viewer native|tiger-vnc]
```

- `list` refreshes inventory and prints VMID, status, and name. The operation
  has a 30-second deadline.
- `probe` opens the native preflight path without a long-running GUI. Its
  timeout defaults to 30 seconds and must be between 1 and 300 seconds. `--json`
  emits the bounded structured result.
- `open` launches the native workspace and selects one VM. `--fullscreen` and
  `--view-only` are presence flags. `native` is the default viewer;
  `tiger-vnc` requests the explicit fallback.
- A selector is either an exact decimal VMID or an unambiguous,
  ASCII-case-insensitive VM name from live inventory. Ambiguous or missing
  names fail rather than choosing a VM.

Commands that contact a node require an approved local configuration. The
following shapes are examples only and deliberately use no real host:

```bash
cargo run --locked -- list
cargo run --locked -- probe 100 --json
cargo run --locked -- open "Sample VM" --view-only
```

There is no CLI option for a direct VNC host, port, endpoint, ticket, or
password, and there is no arbitrary remote-command or file-transfer mode.

## Dynamic Resolution

Dynamic Resolution is enabled for native sessions by default. A request starts
from the console viewport's physical backing pixels, not logical UI points:
the logical viewport is multiplied by the current macOS scale factor before it
is normalized.

The requested guest size:

- is at least `640x480`;
- is at most `8192` on either axis;
- cannot exceed 33,554,432 pixels or the checked framebuffer byte ceiling; and
- rounds each axis down to a multiple of eight. For example, a `1600x900`
  backing-pixel viewport requests `1600x896`.

The viewport must remain stable for 250 ms. Only one guest resize is in flight;
newer geometry replaces the pending value. An in-flight request has a
two-second outcome deadline. The UI distinguishes `Requested`, `Pending`,
`Applied`, `Rejected`, `Unsupported`, and `Timed out`. A failure does not end
the console session. `Fit to Window` remains a local rendering option and must
continue working even when the guest cannot resize.

An `Applied` result depends on the VM display device and guest video driver.
RustedOutClient does not alter VM hardware or install guest drivers.

QEMU's VNC server forwards `SetDesktopSize` only when the selected virtual
display implements QEMU UI-info updates. A Proxmox `virtio` display maps to
`virtio-vga` and provides that path; the legacy `default`/`std` VGA path
rejects the request as an invalid screen layout. QEMU reports a forwarded
request with ExtendedDesktopSize result `4`. RustedOutClient treats that reply
as `Pending` even when it still carries the old framebuffer dimensions, and
does not report `Applied` until a later server update matches the requested
size. The guest must also have a working VirtIO GPU driver and honor the
display event; capable VM hardware alone is not acceptance.

This handling follows the
[RFB ExtendedDesktopSize result contract](https://github.com/rfbproto/rfbproto/blob/master/rfbproto.rst#extendeddesktopsize-pseudo-encoding)
and the
[QEMU 10.1.2 SetDesktopSize implementation](https://gitlab.com/qemu-project/qemu/-/blob/v10.1.2/ui/vnc.c).

## Clipboard

Clipboard integration defaults off. Enabling it merely makes the explicit
direction-specific actions available: **Send Clipboard to Guest** reads the
host clipboard at that moment, and **Receive Clipboard from Guest** consumes
one buffered remote value. Neither direction synchronizes automatically. Text
must be valid UTF-8 and is limited to one MiB. Contents are not logged or
persisted.

## Manual TigerVNC fallback

The fallback is an explicit, temporary compatibility path and is not automatic.
It requires `fallback_viewer` to name an absolute executable path. At launch,
RustedOutClient validates and privately snapshots at most 64 MiB of the opened
viewer executable, creates a process-lifetime mode-`0600` VNC password artifact,
and starts the viewer with fixed VNC-authentication, shared-session, and remote-
resize arguments in a cleared environment.

The fallback binds one ephemeral IPv4 listener on `127.0.0.1`, accepts one
loopback peer, and relays it to the owned verified SSH proxy. It never exposes a
LAN listener or general VNC endpoint. Its temporary files, listener, viewer,
and proxy must be removed or reaped on every terminal path.

TigerVNC is third-party software and this route has a same-host loopback race
and temporary on-disk password material that the embedded client avoids. It
does not count as accepted until the separate fallback gate in
[Native acceptance](native-acceptance.md) passes.

## More information

- [Security policy](../SECURITY.md)
- [Threat model](threat-model.md)
- [Upstream policy](upstream.md)
- [Migration and rollback](migration.md)
- [Native acceptance](native-acceptance.md)
