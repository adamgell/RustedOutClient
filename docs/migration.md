# Migration and Rollback

Migration is side-by-side and reversible. Native acceptance and an explicit
operator choice must precede any switch of the default console route.

## Protected rollback assets

Keep these existing assets read-only throughout build, review, native
acceptance, and fallback acceptance:

```text
~/.local/bin/pve-vnc
~/.config/pve-vnc/config.json
~/Desktop/Open PVE VNC.command
/opt/homebrew/bin/vncviewer
```

Do not delete, overwrite, reformat, migrate in place, or print the old
configuration. It can reveal private infrastructure even if it contains no
credential. Metadata and SHA-256 hashes are enough to prove that the rollback
assets remain unchanged.

## Staged migration

1. Record each protected asset's mode, byte size, modification time, and
   SHA-256 without reading or publishing its contents.
2. Build and verify RustedOutClient at one clean exact commit. Install it under
   a distinct name such as `~/.local/bin/rustedoutclient`; do not replace the
   old helper or Desktop launcher.
3. Create the new private configuration described in
   [Configuration and CLI](configuration.md). If a controlled migration tool
   presents a legacy proposal, review only `ssh_target`, `node`, and fallback
   viewer. Reject any password, ticket, fingerprint, or other secret field.
   The current application does not automatically invoke the narrow source
   importer at startup.
4. Complete local/CI readiness. The Task 14B parser-smoke tranche passed at
   `78be68ee6a2487b4eaa597e56945ed86bdb123a7`; rerun its local and hosted gates
   at the final acceptance head rather than treating the baseline result as
   approval of later changes.
5. At an approved exact head, perform every native gate in
   [Native acceptance](native-acceptance.md), keeping evidence sanitized.
6. Perform the explicit TigerVNC fallback gate independently. A successful
   fallback does not substitute for native acceptance.
7. Re-prove the old helper workflow, verify all four rollback hashes/metadata
   are unchanged, and ask the operator whether to switch the default route.

No migration step changes Proxmox configuration, VM hardware, guest drivers,
OpenSSH trust, or installed TigerVNC. Those require separate authorization.

## Rollback

To roll back, close RustedOutClient and use the existing Desktop launcher or
`~/.local/bin/pve-vnc` exactly as before. Do not terminate unrelated processes
or alter the old configuration. If RustedOutClient left an owned child or
runtime artifact, retain only content-free diagnostics, stop rollout, and
resolve the cleanup failure before another attempt.

Rollback success proves that the prior route still works; it does not itself
accept the native client. A default-route change is complete only after exact-
head review, all local/hosted gates, live native acceptance, fallback
acceptance, rollback proof, and the operator's explicit decision.
