# Portal public release runbook (issue #5)

This runbook completes the repository-controlled portion of
[issue #5](https://github.com/FelineStateMachine/goq/issues/5). The published
Portal download is **macOS arm64 only**.

Issue #5 asks for an explicit, recorded decision between arm64, x86_64, and
universal. The decision is **arm64-only, as separate-architecture builds rather
than a universal binary**. A universal DMG would double the download for every
user to serve a shrinking Intel population, and each additional published
architecture adds a notarization, Gatekeeper, and promotion path that must be
independently proven rather than assumed. macOS x86_64, Linux x86_64, and
Windows x86_64 are instead built and attested every release as unpublished
preview artifacts. Revisit this decision only with demand evidence, not by
default. See [Portal platform support](portal-platform-support.md) for the tier
definitions.

Portal is always a compiled DMG download. There is no shell installer and no
fallback to an ad-hoc, demo, or development build.

Portal is distributed **unsigned by Apple standards**: ad-hoc signed, not
notarized, not stapled, and not accepted by Gatekeeper without user action.
See the signing boundary in
[public release delivery](public-release-delivery.md) for why, and what
replaces Apple as the trust anchor.

The release build uses no optional Cargo features. In particular,
`demo-direct-node` and `experimental-non-macos-pointer-capture` are forbidden
from the default feature set and from the packaging command. The latter exists
only to compile the unverified Tao + browser Pointer Lock path during
non-macOS platform UAT; see
[Portal platform support](portal-platform-support.md).

## One-time GitHub setup

Protect the GitHub environment named `main` with required reviewers and limit
deployment branches/tags according to the repository's release policy.

The Portal publication job needs **no Apple secrets**, because Portal is not
Developer ID signed or notarized. The packaging gate actively refuses to run in
ad-hoc mode when any `APPLE_*` credential is present in the environment, so a
leaked or half-configured credential cannot silently produce a differently
signed artifact.

The job does require `attestations: write` and `id-token: write`, since the
GitHub/Sigstore build-provenance attestation is what replaces Apple as the
trust anchor.

## Prepare an exact tag

Choose a SemVer tag such as `v0.1.0`. Before tagging, update every
workspace package version and `src-tauri/tauri.conf.json` to the tag's version.
The shared verifier rejects a dirty worktree, a tag not resolving exactly to
`HEAD`, any version mismatch, a non-arm64 target, or a Portal manifest that
enables `demo-direct-node` by default.

Run the non-secret policy checks before pushing the tag:

```bash
python3 scripts/verify-portal-release.py website \
  --manifest website/portal-release.json
./scripts/tests/portal-release.sh
./scripts/verify-website.sh
```

Create and push the tag only from the reviewed clean release commit. Dispatch
the workflows from that tag ref, not from `main`; the Portal build refuses to
attest an input tag checked out by a different workflow ref. First run
the **Sigil Release** workflow with `build-candidate`; it creates the shared
draft release and attaches the two verified Sigil candidate assets. Then
manually run **Portal release** for the same tag. Both workflows share a
concurrency group, so their release mutations cannot overlap.

## Publication transaction

The workflow performs this fail-closed sequence:

1. Re-verifies clean tag, exact `HEAD`, workspace versions, Tauri version, and
   demo-feature policy on Linux.
2. Uses a native `macos-26` arm64 runner and the protected `main` environment.
3. Builds without feature flags in `--signing adhoc` mode, verifies the DMG,
   and requires the mandatory arm64 ad-hoc signature. It rejects the build if a
   certificate authority chain, an Apple TeamIdentifier, or a notarization
   ticket is present, so an ad-hoc release cannot quietly become something else.
4. Confirms the executable contains only arm64, the bundle identifier is
   `sh.goq.portal`, no host or credential artifact is inside the bundle, and the
   DMG, digest, and JSON manifest agree with the source tag and commit. The
   manifest records `signing: "adhoc"` and `notarized: false`.
5. Creates GitHub/Sigstore build-provenance attestations for the exact three
   Portal assets from the protected tag-ref workflow.
6. Requires the existing draft to contain exactly the two Sigil candidate
   assets, then uploads exactly:

   - `Portal-VERSION-arm64.dmg`
   - `Portal-VERSION-arm64.dmg.sha256`
   - `Portal-VERSION-arm64.json`

7. Reads GitHub's remote asset list and requires the exact five-file combined
   pre-signature set. It deliberately leaves the release as a draft.

## Unpublished preview matrix

The same workflow runs a `preview` job for macOS x86_64, Linux x86_64, and
Windows x86_64. It builds the exact release tag with no optional features,
records a `preview-manifest.json` with each artifact's SHA-256, attests the
build provenance, and uploads the result as the 14-day Actions artifact
`portal-preview-$tag-$target`.

The job holds `contents: read` only. It cannot attach an asset to the release,
so the published contract stays exactly the three macOS arm64 files and the
Sigil promotion gate still sees its expected six-asset set. Preview builds run
with `fail-fast: false` and are not a dependency of `publish`; a Windows
bundler failure must never block a signed macOS release.

Preview artifacts are unsigned. Do not publish them, link them, or hand them to
users as a download. Use them for the platform acceptance checklist in
[Portal platform support](portal-platform-support.md), and only from the tag's
own workflow run.

The operator next attaches the offline Sigil `.minisig`. The **Sigil Release**
`promote-signed-draft` operation requires exactly all six Portal and Sigil
assets, re-verifies the Sigil signature/archive/provenance and Portal
digest/manifest against the same tag, verifies every Portal asset's attestation
against the exact Portal workflow, source tag, and source commit, mounts the
downloaded DMG on a native arm64 macOS runner, repeats the ad-hoc signature,
architecture, identifier, and payload checks, confirms no notarization ticket
appeared, and only then publishes the prerelease with final notes.

If signing, upload, or remote verification fails, the shared release remains a
draft for diagnosis. Do not replace assets on an existing tag; fix the source,
delete an unpublished failed draft after review, and create a new version/tag
when provenance changed.

For a local rehearsal on an arm64 Mac, use an empty directory outside the
repository. No Apple credentials are needed, and any that are set will be
rejected:

```bash
scripts/package-macos-client.sh \
  --release-tag v0.1.0 \
  --signing adhoc \
  --output-dir /absolute/empty/release-directory
```

## Enable the reviewed website download

The checked-in `website/portal-release.json` starts with `available: false` and
contains no URL. After the prerelease is promoted:

1. Download all three GitHub assets on a separate machine.
2. Verify the digest file against the DMG and inspect the JSON manifest.
3. Verify Gatekeeper again on the downloaded DMG.
4. Submit a normal reviewed change that replaces only the `macos-arm64` entry
   with `available: true` and these exact fields:

```json
{
  "architecture": "arm64",
  "asset": "Portal-0.1.0-arm64.dmg",
  "available": true,
  "checksum_asset": "Portal-0.1.0-arm64.dmg.sha256",
  "checksum_url": "https://github.com/FelineStateMachine/goq/releases/download/v0.1.0/Portal-0.1.0-arm64.dmg.sha256",
  "download_url": "https://github.com/FelineStateMachine/goq/releases/download/v0.1.0/Portal-0.1.0-arm64.dmg",
  "manifest_asset": "Portal-0.1.0-arm64.json",
  "manifest_url": "https://github.com/FelineStateMachine/goq/releases/download/v0.1.0/Portal-0.1.0-arm64.json",
  "platform": "macos",
  "release_tag": "v0.1.0",
  "release_url": "https://github.com/FelineStateMachine/goq/releases/tag/v0.1.0",
  "sha256": "REPLACE_WITH_VERIFIED_LOWERCASE_SHA256",
  "verification": "adhoc-signed+github-attested+sha256",
  "version": "0.1.0"
}
```

`scripts/verify-website.sh` validates the manifest and JavaScript gate before
the existing protected `main` website deployment. A missing or invalid
manifest, fetch failure, or unavailable entry leaves the anchor without an
`href` and explains that no signed download is offered.

To withdraw a download, submit a reviewed manifest change back to:

```json
{
  "available": false,
  "reason": "Signed Portal download temporarily unavailable."
}
```

This disables the website link without substituting another artifact. GitHub
release deletion or withdrawal and website rollback are separate reviewed
operator actions.
