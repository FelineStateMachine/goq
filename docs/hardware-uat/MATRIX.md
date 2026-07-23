# Goq hardware proof matrix

This file is the normative contract for unqualified hardware-proof claims.
One successful machine is useful reference-host evidence, but it does not prove
a hardware class or transfer automatically to a later commit.

## Required rows

Both rows must pass with the same exact source commit and candidate artifact
set before Goq may use `matrix-proven` or an unqualified `hardware-proven`
claim. Hardware and vendor identifiers are evidence, not admission rules;
render-node and encoder selection remain capability-based.

| Row ID | Required class | Status | Evidence report |
| --- | --- | --- | --- |
| native-1280x800-handheld | Upstream SteamOS Gamescope handheld with a native 1280x800 panel and integrated GPU | pending | pending |
| physically-headless-desktop-dgpu | Physically headless Gamescope desktop with a discrete GPU on SteamOS or a SteamOS-inspired distribution | pending | pending |

The handheld row prevents Bazzite-specific Gamescope behavior from standing in
for upstream SteamOS. The desktop row may use SteamOS or a SteamOS-inspired
distribution such as Bazzite or CachyOS. A display manager, render-node number,
encoder factory name, driver name, or GPU vendor is not part of the product
contract.

The committed GPD Pocket 4/Bazzite run at
[`7920c5d21434`](7920c5d21434/REPORT.md) is reference-host evidence only. Its
panel is natively 2560x1600, so its 1280x800 encoded leg does not satisfy the
native-1280x800 handheld row. It is not a discrete-GPU or physically headless
desktop and cannot satisfy that row either.

## Claim levels

- `reference-host proven` means one named host passed one named, immutable
  commit and evidence run. Public text must name that host and commit or link
  its report.
- `matrix-proven` means both required rows passed the same exact development
  candidate commit and artifact set.
- `release-matrix-proven` means both required rows passed the same immutable,
  signed Sigil and Portal release candidates under the public-alpha contract.

A later commit does not inherit an earlier proof. Until both required rows
pass, public claims must remain qualified as reference-host evidence and must
not say `hardware-proven` without qualification.

## Evidence contract

Each passing row must link a committed report under `docs/hardware-uat/`. The
report and its sanitized supporting files must bind:

- the matrix row ID, exact source commit, workflow run, and one shared
  `Candidate artifact set SHA256` digest;
- host model, integrated/discrete topology, connector state, native resolution
  and refresh rate;
- OS, kernel, Gamescope, Mesa, GStreamer, and encoder plugin versions;
- the capability-discovered render node and encoder factory, without assuming
  `/dev/dri/renderD128`, `amdgpu`, or `vah264enc`;
- fixed and native capture outcomes, post-encode drops, and measured cadence;
- exact Portal session evidence for transport, video, audio, input,
  reconnects, and second-client rejection appropriate to the claim;
- restoration of the installed service, configuration, identity, and release
  activation after the run.

The two reports must name the same exact commit and `Candidate artifact set
SHA256`. Unsigned development evidence can establish `matrix-proven`, but
never `release-matrix-proven`.

Committed evidence must retain the existing redaction boundary: no endpoint or
peer IDs, invitations, identities, secrets, private addresses, or unsanitized
raw logs.
