# Bazzite hardware acceptance

The repository's hardware acceptance is a two-stage, commit-pinned gate:

1. `.github/workflows/sigil-hardware-uat.yml` builds and inspects an unsigned
   development candidate for one clean commit already on `main`.
2. `scripts/run-bazzite-hardware-uat.sh` verifies that artifact and exercises it
   against a live Gamescope PipeWire session on a Bazzite host.

The runner is intentionally disruptive for the duration of the test. It stops
the installed `sigil-host.service`, runs isolated fixed and native candidates,
then restores the original service. Invocation-specific systemd rollback timers
and a host-wide lock prevent an interrupted or concurrent run from stranding the
host. A run can report `hardware_uat=pass` only after verifying the original
service, config, identity, and release link.

The enforced performance target is 1280x800 at at least 55 encoded frames per
second. Native resolution is a functional compatibility leg: its actual cadence
is recorded without claiming that every native mode meets the fixed performance
target. Both legs require 300 captured frames with zero post-encode drops, ten
authenticated Iroh/MoQ sessions, ten grouped-v3 sessions, bounded forced-IDR
recovery, input acknowledgement, direct-path confirmation, and zero sequence
gaps.

Each evidence directory is named after the first 12 characters of its exact
source commit. It contains the aggregate result, both bounded capture logs, and
the GitHub Actions provenance used to obtain the candidate. Probe session IDs
are deliberately excluded from committed evidence.

To run the gate, download the artifact produced by the pinned workflow into a
new owner-only directory matching:

```text
$HOME/.local/state/goq-hardware-uat.<12-char-commit>.<6-char-suffix>
```

Place the archive, its checksum, `release-manifest.json`, and
`uat-provenance.txt` in an owner-only `incoming/` subdirectory on the Bazzite
host, copy the runner to that host, then execute there:

```sh
./run-bazzite-hardware-uat.sh UAT_ROOT FULL_COMMIT WORKFLOW_RUN_ID
```

The runner rejects reused output directories and concurrent hardware runs.
