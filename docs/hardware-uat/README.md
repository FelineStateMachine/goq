# Bazzite hardware acceptance

The cross-host claim contract is
[`MATRIX.md`](MATRIX.md). A successful run on one machine establishes only
named reference-host evidence; it does not prove a hardware class or transfer
to another commit. Both required matrix rows must pass the same exact candidate
before unqualified hardware-proof language is allowed.

Matrix pass reports use the structured report, shared candidate-manifest, and
row evidence layout defined by `MATRIX.md`. Run
`scripts/tests/hardware-matrix.sh` before committing a status change. Its
fixture suite rejects incomplete reports, fabricated commits or artifact-set
digests, row/class mismatches, and premature claims in public documentation.

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

When the panel's native mode is already 1280x800, the native leg still runs with
width and height omitted from its Sigil configuration. The evidence records
`native_mode_relation=identical-to-fixed`; this proves native-mode resolution
without requiring a second pixel size that the panel cannot provide. Other
panels record `native_mode_relation=distinct-from-fixed`.

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

If more than one exact render-node/factory pair satisfies the complete UAT
contract, discovery fails closed and prints the eligible pairs. Select one pair
explicitly and rerun:

```sh
./run-bazzite-hardware-uat.sh UAT_ROOT FULL_COMMIT WORKFLOW_RUN_ID \
  --render-node /dev/dri/renderD129 \
  --va-encoder varenderD129h264enc
```

The two override options are inseparable, and the requested pair must pass the
same device, factory binding, programmatic property, CBR, and CQP checks as automatic
selection. The runner rejects reused output directories and concurrent hardware
runs.
Before generating its isolated configs, it enumerates accessible DRM render
nodes and uses Sigil's GObject probe to match each one to the exact dynamically
registered GstVA H.264 factory whose read-only `device-path` names that node.
No kernel-driver name or `gst-inspect` property formatting is required. The
chosen factory must advertise both CBR (for
the in-process fixed/native legs) and CQP (for the external compatibility leg).
The gate therefore does not assume either `/dev/dri/renderD128` or the generic
`vah264enc` factory. The selected pair is recorded in `summary.env`.
