# Upstream Integration Policy

RustedOutClient began from the exact source base:

```text
hkder/ironvnc@999e00e3a3672efdbf8e8f307e7bd60875dee67e
```

IronVNC attribution is retained in [`NOTICE`](../NOTICE). Code derived from
that base remains available under the upstream dual MIT or Apache-2.0 terms;
see [`LICENSE-MIT`](../LICENSE-MIT) and
[`LICENSE-APACHE`](../LICENSE-APACHE).

## Integration rule

There is no blind merge from IronVNC or another VNC client. Every upstream
change must be reviewed from source, reduced to the lines needed by the
RustedOutClient product boundary, and brought in as a tested, traceable
cherry-pick or an equivalently reviewable local commit. Record the upstream
repository, source commit, selected files/commits, local commit, rationale, and
verification result.

For every proposed import:

1. Review the source diff and transitive dependency change before applying it.
2. Identify parser, decoder, allocation, authentication, networking, logging,
   clipboard, input, and process-lifecycle effects.
3. Preserve RustedOutClient's typed `TrustedSshProxy` production entry point,
   bounds, semantic action boundary, and exact cleanup ownership.
4. Add or update focused synthetic tests for the imported behavior, including
   malicious lengths or state transitions where relevant.
5. Run the locked formatting, all-target test, lint, release, audit,
   cargo-deny, source-policy, and supported-target dependency checks.
6. Record the upstream-to-local traceability and obtain security review before
   accepting the change.

## Surfaces that must not re-enter

An upstream change must not restore or add any of the following to production:

- general direct-TCP VNC host, port, endpoint, or password entry;
- saved VNC sessions or persistent ticket/password storage;
- security type None, RA2, VeNCrypt, ARD, or another authentication downgrade;
- SFTP, SCP, file transfer, remote filesystem browsing, or drag-and-drop file
  transport;
- arbitrary local or remote command execution, local shell invocation, or SSH
  options that weaken strict host verification or enable password prompts;
- a native-client listener or a non-loopback/multi-peer fallback listener;
- automatic fallback, inherited fallback environment, or arbitrary viewer
  arguments;
- logging or persistence of secrets, raw remote output, guest pixels, or
  clipboard contents;
- unbounded protocol-controlled allocation, text, geometry, decompression, or
  queue growth; or
- general Proxmox administration such as power, snapshot, migration, storage,
  network, LXC, SPICE, noVNC, or RDP operations.

If an upstream improvement cannot be isolated from one of these surfaces, do
not import it until the product security boundary is deliberately redesigned,
documented, tested, and reviewed.
