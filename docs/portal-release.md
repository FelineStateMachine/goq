# Portal public release runbook (issue #5)

This runbook completes the repository-controlled portion of
[issue #5](https://github.com/FelineStateMachine/goq/issues/5). The first
supported Portal download is **macOS arm64 only**. x86_64, universal, Linux,
and Windows builds remain unavailable until they receive equivalent signing
and release gates.

Portal is always a compiled DMG download. There is no shell installer and no
fallback to an ad-hoc, demo, or development build.

## One-time GitHub and Apple setup

Protect the GitHub environment named `main` with required reviewers and limit
deployment branches/tags according to the repository's release policy. Add
these environment secrets:

| Secret | Purpose |
| --- | --- |
| `PORTAL_APPLE_CERTIFICATE_BASE64` | Base64-encoded Developer ID Application PKCS#12 certificate |
| `PORTAL_APPLE_CERTIFICATE_PASSWORD` | PKCS#12 import password |
| `PORTAL_APPLE_SIGNING_IDENTITY` | Exact `Developer ID Application: ...` keychain identity |
| `PORTAL_APPLE_ID` | Apple account used by the notarization service |
| `PORTAL_APPLE_PASSWORD` | App-specific password for notarization |
| `PORTAL_APPLE_TEAM_ID` | Apple Developer team identifier |

Do not put certificate bytes, passwords, API keys, or secret-derived output in
the repository, workflow artifacts, release notes, or website manifest. The
workflow imports the certificate into an ephemeral keychain and removes it in
an always-running cleanup step.

## Prepare an exact tag

Choose a SemVer tag such as `v0.3.0-alpha.1`. Before tagging, update every
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

Create and push the tag only from the reviewed clean release commit. First run
the **Sigil Release** workflow with `build-candidate`; it creates the shared
draft release and attaches the two verified Sigil candidate assets. Then
manually run **Portal release** for the same tag. Both workflows share a
concurrency group, so their release mutations cannot overlap.

## Publication transaction

The workflow performs this fail-closed sequence:

1. Re-verifies clean tag, exact `HEAD`, workspace versions, Tauri version, and
   demo-feature policy on Linux.
2. Uses a native `macos-26` arm64 runner and the protected `main` environment.
3. Builds without feature flags and requires Developer ID Application signing,
   hardened runtime, notarization, app and DMG stapling, strict code-signature
   verification, and Gatekeeper acceptance.
4. Confirms the executable contains only arm64 and that the DMG, digest, and
   JSON manifest agree with the source tag and commit.
5. Requires the existing draft to contain exactly the two Sigil candidate
   assets, then uploads exactly:

   - `Portal-VERSION-arm64.dmg`
   - `Portal-VERSION-arm64.dmg.sha256`
   - `Portal-VERSION-arm64.json`

6. Reads GitHub's remote asset list and requires the exact five-file combined
   pre-signature set. It deliberately leaves the release as a draft.

The operator next attaches the offline Sigil `.minisig`. The **Sigil Release**
`promote-signed-draft` operation requires exactly all six Portal and Sigil
assets, re-verifies the Sigil signature/archive/provenance and Portal
digest/manifest against the same tag, mounts the downloaded DMG on a native
arm64 macOS runner, repeats Developer ID/Gatekeeper/stapling checks, and only
then publishes the prerelease with final notes.

If signing, upload, or remote verification fails, the shared release remains a
draft for diagnosis. Do not replace assets on an existing tag; fix the source,
delete an unpublished failed draft after review, and create a new version/tag
when provenance changed.

For a local Apple-credentialed rehearsal on an arm64 Mac, use an empty directory
outside the repository:

```bash
scripts/package-macos-client.sh \
  --release-tag v0.3.0-alpha.1 \
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
  "asset": "Portal-0.3.0-alpha.1-arm64.dmg",
  "available": true,
  "checksum_asset": "Portal-0.3.0-alpha.1-arm64.dmg.sha256",
  "checksum_url": "https://github.com/FelineStateMachine/goq/releases/download/v0.3.0-alpha.1/Portal-0.3.0-alpha.1-arm64.dmg.sha256",
  "download_url": "https://github.com/FelineStateMachine/goq/releases/download/v0.3.0-alpha.1/Portal-0.3.0-alpha.1-arm64.dmg",
  "manifest_asset": "Portal-0.3.0-alpha.1-arm64.json",
  "manifest_url": "https://github.com/FelineStateMachine/goq/releases/download/v0.3.0-alpha.1/Portal-0.3.0-alpha.1-arm64.json",
  "platform": "macos",
  "release_tag": "v0.3.0-alpha.1",
  "release_url": "https://github.com/FelineStateMachine/goq/releases/tag/v0.3.0-alpha.1",
  "sha256": "REPLACE_WITH_VERIFIED_LOWERCASE_SHA256",
  "verification": "developer-id+hardened-runtime+notarized+stapled+gatekeeper",
  "version": "0.3.0-alpha.1"
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
