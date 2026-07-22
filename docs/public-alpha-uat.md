# Public-alpha release-candidate UAT

Issue #6 is a hardware acceptance gate, not a unit-test checkbox. This guide
turns operator observations and diagnostics into a bounded evidence bundle for
one published Sigil asset set, one published Portal asset set, and one immutable
`vVERSION` tag. The harness validates supplied evidence; it does not run the
hardware exercise, infer observations, or claim that the gate passed.

Use the actual release candidates from a clean checkout of the exact tag. `HEAD`,
a branch name, a locally rebuilt binary, a different DMG, or a moving ref is not
accepted even if its source is equivalent. Initialization and verification must
run on macOS because the Portal DMG is assessed with the platform security tools.
They also require an authenticated GitHub CLI with network access to the public
`FelineStateMachine/goq` repository; downloaded or mirrored files cannot be used
as an offline substitute for the published release identity checks.

## 1. Initialize the bundle

Choose an absolute path on an encrypted operator-controlled volume. Do not put
the evidence bundle in the repository or on the shared host.

```bash
uat_dir="$PWD-private/goq-uat-rc1"
./scripts/public-alpha-uat.sh init \
  --evidence-dir "$uat_dir" \
  --release-tag v0.1.0-rc.1 \
  --sigil-archive /absolute/path/sigil-v0.1.0-rc.1-bazzite-x86_64.tar.gz \
  --sigil-bin /absolute/path/to/sigil \
  --portal-assets /absolute/path/to/portal-v0.1.0-rc.1-assets
```

`init` creates a mode `0700` directory and an atomic, mode `0600`, tab-separated
`manifest.tsv`. It records the resolved 40-character tagged commit, release tag,
the hashes of the signed Sigil archive/checksum/signature/public key, the exact
Sigil payload hash, all three Portal asset hashes, creation time, and the
evidence freshness window. The default window is seven days;
`--max-age-seconds` can set 3600 through 2592000 seconds.

Before creating the bundle the harness performs all of these fail-closed checks:

- `refs/tags/vVERSION` resolves exactly to the clean checkout's `HEAD`.
- GitHub's `FelineStateMachine/goq` tag ref resolves to that same 40-character
  commit, including through an annotated-tag chain, and the release carrying
  that exact tag is published, non-draft, and marked as a prerelease.
- The published release contains exactly the expected six Sigil and Portal
  filenames. Every supplied file's size and SHA-256 must equal GitHub's uploaded
  asset metadata; a locally valid or identically named substitution is rejected.
- `scripts/verify-sigil-release.sh` validates the archive, checksum, detached
  Minisign signature, source commit, and product contents against the reviewed
  repository key at `release/sigil-minisign.pub`.
- The supplied installed/tested `sigil` executable is byte-for-byte identical
  to `payload/release/sigil` inside that verified archive.
- The Portal directory contains exactly `Portal-VERSION-arm64.dmg`, its
  `.dmg.sha256`, and `Portal-VERSION-arm64.json`.
- `scripts/verify-portal-release.py assets` validates those three assets against
  the same clean exact tag.
- `gh attestation verify` validates all three Portal files against the exact
  `FelineStateMachine/goq/.github/workflows/portal-release.yml` signer workflow,
  `refs/tags/vVERSION`, tagged source commit, and GitHub-hosted runner boundary.
- `scripts/verify-macos-portal-signature.sh` validates the DMG, Developer ID
  signature, hardened runtime, notarization ticket, staple, Gatekeeper result,
  bundle identity/version, arm64-only executable, containment boundary, and the
  exact Apple TeamIdentifier pinned in `release/portal-apple-team-id.txt`.

If the reviewed Sigil key is still the `unconfigured` sentinel, Minisign or
macOS verification tools are unavailable, the Apple TeamIdentifier pin is
invalid, the GitHub API or attestations cannot be verified, the macOS verifier
is absent, or any verification cannot run, initialization fails. There is no
candidate, offline, or test bypass in the UAT command.

The bundle format is intentionally append-only at the command boundary. To
replace a mistaken record, initialize a new bundle. Do not edit the manifest or
normalized records.

## 2. Evidence envelope

Every input is a mode `0600` regular file at an absolute path. It begins with
these fields, copied from `manifest.tsv` where applicable:

```text
uat_schema=goq-public-alpha-evidence-v2
evidence_kind=controller
observed_at_unix=1760000000
git_commit=0123456789abcdef0123456789abcdef01234567
release_tag=v0.1.0-rc.1
sigil_sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
portal_sha256=abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789
```

Use a current `date +%s` taken when that exercise finishes. An input is rejected
when a required field is missing, duplicated, stale, future-dated, or bound to
anything other than the initialized release candidates.

Do not include node IDs, host/peer IDs, invitations, secrets, private keys, or
identity seeds. The harness rejects recognizable secret-bearing fields before
normalizing the allowlisted keys, caps each source at 1 MiB, and rejects
symlinks and group/other-writable inputs.

Record an observation only after its exercise has genuinely completed:

```bash
chmod 600 /absolute/path/to/controller.evidence
./scripts/public-alpha-uat.sh record \
  --evidence-dir "$uat_dir" \
  --kind controller \
  --file /absolute/path/to/controller.evidence
```

Supported kinds are `cold-boot`, `controller`, `mouse`, `soak`,
`network-direct`, `network-relay`, `reconnect`, `second-client`, and the
optional `loopback-preflight`.

## 3. Required hardware exercises

Append the listed kind-specific fields after the common envelope. `pass` means
the operator actually observed that result with the initialized release
candidates.

### Physically headless cold boot

Disconnect every display, remove any interactive SSH session, cold boot the
Bazzite host, and wait before making the first SSH connection. Run the existing
read-only inventory check with `--cold-boot`. Its output may be used directly
after the common envelope; unrecognized inventory fields are omitted during
normalization.

```bash
ssh tank@umpc 'cd /path/to/goq && ./scripts/bazzite-inventory.sh --cold-boot' \
  >> /absolute/path/to/cold-boot.evidence
```

The evidence must contain:

```text
cold_boot_result=pass
cold_boot_failure_count=0
cold_boot_insufficient_count=0
headless_connector_state=ok
gaming_autologin_session=ok
sigil_host_enabled=enabled
sigil_host_active=active
gamescope_pipewire_node=ok
gamescope_before_first_ssh=ok
sigil_unit_before_first_ssh=ok
sigil_ready_before_first_ssh=ok
```

This proves Gamescope and Sigil became ready before first SSH, not merely that
they were running after an operator repaired the boot.

### Physical controller and neutral release

Attach a physical controller to the Portal client and control an actual game
for at least five minutes. Exercise every named control class. Disconnect or
end the session while controls are active and observe the host returning both
buttons and axes to neutral.

```text
physical_controller_attached_to_portal=pass
actual_game_controlled=pass
controller_coverage=abxy,dpad,sticks,triggers,shoulders,start-back
neutral_release_on_disconnect=pass
neutral_buttons=pass
neutral_axes=pass
session_seconds=300
```

### Mouse buttons in Gamescope/an actual game

Pointer motion or hover changes are insufficient. In a bounded target slug,
observe at least five attempts and confirm both buttons are consumed by the
Gamescope target application or actual game:

```text
target_application=actual-game
left_click_consumed=pass
right_click_consumed=pass
consumption_observed_in_target=pass
click_attempts=10
```

### Sustained A/V and resource soak

Run an uninterrupted session for at least 3600 seconds and collect at least 60
samples. Report measured percentiles, not estimates:

```text
duration_seconds=3600
samples=60
capture_fps_p50=59.8
presentation_fps_p50=59.2
frame_interval_p95_ms=20
hitch_p99_ms=40
video_queue_p95_frames=1
decode_queue_p95_frames=1
audio_queue_p95_ms=50
av_skew_p95_ms=30
max_queue_age_p95_ms=45
cpu_p95_percent=50
gpu_p95_percent=70
rss_p95_mib=512
transport_drops=2
frontend_drops=3
audio_drops=0
latency_first_window_p95_ms=30
latency_last_window_p95_ms=33
disconnects=0
```

The fail-closed bounds are: capture and presentation p50 at least 55 fps;
frame interval p95 at most 25 ms; hitch p99 at most 50 ms; video and decoder
queues p95 at most two frames; audio queue and maximum queue age p95 at most
100 ms; A/V skew p95 at most 50 ms; CPU p95 at most 90%; GPU p95 at most 95%;
RSS p95 at most 2048 MiB; and zero disconnects. Last-window latency p95 must be
no more than 120% of the first window and may grow by no more than 5 ms. Drop
counts remain visible evidence rather than being silently discarded.

### Direct and difficult-NAT relay paths

These are separate records and separate sessions of at least ten minutes. Do
not relabel an ordinary direct connection as a relay exercise.

`network-direct`:

```text
path_mode=direct
nat_scenario=ordinary
session_seconds=600
rtt_p50_ms=10
rtt_p95_ms=20
input_ack_p95_ms=30
presentation_latency_p95_ms=60
packet_loss_percent=0.1
```

`network-relay` uses the same timing fields with:

```text
path_mode=relay
nat_scenario=difficult
```

Both records reject packet loss over 5%. The relay run must be forced or
observed under the documented difficult-NAT setup and confirmed as relayed by
Portal diagnostics.

### Reconnect and second-client admission

Perform at least ten reconnects with the exact candidates. Each must restore
the intended session state and resume from a keyframe:

```text
reconnect_cycles=10
reconnect_successes=10
reconnect_failures=0
state_preserved=pass
keyframe_recovery_p95_ms=900
```

Keyframe recovery p95 must not exceed 2000 ms.

While the authorized primary session remains playable, attempt a second client
at least three times:

```text
second_client_attempts=3
second_client_rejections=3
authorized_primary_uninterrupted=pass
rejection_reason=active-client
```

## 4. Optional loopback preflight

The existing loopback proof is useful preflight evidence, but it does not
replace any physical gate. Run it from the exact committed tree using release
mode. Its output includes an operationally sensitive `node_id`, so remove that
line before ingestion and add the common envelope:

```bash
./scripts/loopback-proof.sh --profile release \
  | sed '/^node_id=/d' \
  >> /absolute/path/to/loopback-preflight.evidence
chmod 600 /absolute/path/to/loopback-preflight.evidence
```

The normalized record requires:

```text
loopback_proof=ok
profile=release
host_sha256=<same value as sigil_sha256>
active_client_rejection=ok
reconnect_cycles=3
cleanup=ok
```

The harness ingests the output by value and never mutates the Bazzite host.

## 5. Verify and retain

Verification requires the exact same signed asset sets and tested Sigil
executable again. It reruns every release, Minisign, and macOS platform check;
it also reruns the live GitHub release/tag/digest and Portal provenance checks.
Stored hashes alone are not sufficient:

```bash
./scripts/public-alpha-uat.sh verify \
  --evidence-dir "$uat_dir" \
  --sigil-archive /absolute/path/sigil-v0.1.0-rc.1-bazzite-x86_64.tar.gz \
  --sigil-bin /absolute/path/to/sigil \
  --portal-assets /absolute/path/to/portal-v0.1.0-rc.1-assets
```

Success prints `public_alpha_uat=pass` plus the release tag, commit, artifact hashes, and
required gate count. Verification rejects missing records, expired timestamps,
changed tags, wrong artifacts, modified normalized evidence, unsafe modes,
symlinks, unpublished or non-prerelease releases, mismatched remote asset
digests, invalid Portal attestations, and unexpected bundle files.

That output means the supplied bundle satisfies the machine-checkable contract.
It is not evidence that an exercise occurred unless the inputs came from the
real operator run. Preserve the verified directory read-only as release
evidence, separately from publish credentials and product artifacts.

Run the fixture suite before changing the contract:

```bash
shellcheck scripts/public-alpha-uat.sh scripts/tests/public-alpha-uat.sh
./scripts/tests/public-alpha-uat.sh
```
