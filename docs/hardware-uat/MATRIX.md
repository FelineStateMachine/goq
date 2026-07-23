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

Each passing row must link a distinct committed report at
`docs/hardware-uat/<12-char-commit>/<row-id>/REPORT.md`. Both reports must
reference the same
`docs/hardware-uat/<12-char-commit>/candidate-artifacts.env`; its SHA-256 is
the `Candidate artifact set SHA256`. The manifest uses the exact
`goq-hardware-matrix-artifacts-v1` schema and binds the full commit, whether the
candidate is `development` or `release`, and distinct safe Sigil and Portal
asset names plus their nonzero SHA-256 digests. The repository gate recomputes
the manifest digest instead of trusting a report-provided value.

Each report must bind exactly one matrix row, exact source commit, numeric
workflow run, candidate-manifest path and digest, and adjacent `EVIDENCE.env`.
The evidence file uses the exact `goq-hardware-matrix-evidence-v1` schema.
Unknown, missing, empty, duplicated, or unterminated fields fail the gate. Its
structured fields bind:

- host model, integrated/discrete topology, connector state, native resolution
  and refresh rate; the handheld row requires upstream SteamOS, an integrated
  GPU, and a connected native 1280x800 panel, while the desktop row requires a
  discrete GPU and a physically headless SteamOS or SteamOS-inspired host;
- OS, kernel, Gamescope, Mesa, GStreamer, and encoder plugin versions;
- the capability-discovered render node and encoder factory, without assuming
  `/dev/dri/renderD128`, `amdgpu`, or `vah264enc`;
- passing fixed and native capture outcomes, zero post-encode drops, measured
  cadence, and at least 55 fps for the fixed 1280x800 leg;
- a preferred-Iroh/MoQ Portal session with passing video, audio, input,
  reconnect, and second-client rejection evidence;
- restoration of the installed service, configuration, identity, and release
  activation after the run.

The two reports must name the same exact commit and `Candidate artifact set
SHA256`. Unsigned development evidence can establish `matrix-proven`, but
never `release-matrix-proven`; the gate rejects that public claim unless both
reports use one release candidate manifest.

Committed evidence must retain the existing redaction boundary: no endpoint or
peer IDs, invitations, identities, secrets, private addresses, or unsanitized
raw logs. The gate proves that the committed evidence is complete, internally
consistent, commit-reachable, and bound to explicit candidate digests. Human
review still confirms that the sanitized values came from the named workflow
and machines; the repository must not fabricate replacement evidence merely to
make the gate pass.
