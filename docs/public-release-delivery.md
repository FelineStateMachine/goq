# Public release delivery

Portal and Sigil have different delivery contracts. Portal is a compiled
desktop download. Sigil is a machine bootstrap for a dedicated Bazzite host.
Neither path may fall back to an unsigned development build.

## Release asset contract

The first public release is a draft or prerelease until capability-ticket
onboarding and the release-candidate hardware pass are complete.

Sigil publishes exactly these x86_64 Bazzite assets for a tag such as
`v0.3.0-alpha.1`:

- `sigil-v0.3.0-alpha.1-bazzite-x86_64.tar.gz`
- `sigil-v0.3.0-alpha.1-bazzite-x86_64.tar.gz.sha256`
- `sigil-v0.3.0-alpha.1-bazzite-x86_64.tar.gz.minisig`

Portal preserves the package builder's versioned macOS contract:

- `Portal-0.3.0-alpha.1-arm64.dmg`
- `Portal-0.3.0-alpha.1-arm64.dmg.sha256`
- `Portal-0.3.0-alpha.1-arm64.json`

Do not advertise x86_64, universal, or Linux Portal downloads until those exact
artifacts pass an equivalent signed packaging gate.

## Sigil bootstrap trust boundary

`website/install-sigil` is the public bootstrap source. Before the release
channel opens, replace its `unconfigured` publisher key with the dedicated
Minisign public key and review its fingerprint through a separate trusted
channel. The secret key and its passphrase must never enter the repository or a
general deployment environment.

The bootstrap:

1. Refuses root, non-Bazzite systems, unsupported architectures, and missing
   verification tools.
2. Uses the exact immutable release tag pinned beside the publisher key in the
   reviewed bootstrap source.
3. Downloads the versioned archive, checksum, and detached signature from that
   tag. Opening a new channel therefore requires a reviewed website commit.
4. Verifies the embedded publisher key before extraction, then verifies the
   exact asset checksum.
5. Extracts into a private temporary directory and invokes the package-owned
   atomic stager as the gaming user.
6. Does not start or restart Sigil, overwrite identity or host configuration,
   modify `/etc`, or guess ambiguous hardware.

`scripts/verify-sigil-bootstrap.py` keeps the channel fully closed while the
sentinels are present. Once opened, it requires the bootstrap's embedded key to
exactly match `release/sigil-minisign.pub` and rejects partial configuration or
a malformed immutable release tag before the website can deploy.

A clean Bazzite image may not include Minisign. The public-alpha gate must
either provision a pinned verifier with its own hard-coded digest or make the
verifier prerequisite explicit; it must never silently downgrade to SHA-only
verification.

## Sigil candidate and offline signing ceremony

The repository stores only the reviewed public key in
`release/sigil-minisign.pub`. Its committed `unconfigured` sentinel keeps every
promotion fail-closed until the offline publisher key exists. Never replace the
sentinel with a temporary or CI-generated key.

For an immutable tag that exactly matches `crates/sigil-host/Cargo.toml`, run
the **Sigil Release** workflow with `build-candidate`. The protected `main`
environment builds clean tagged source, runs the complete repository gate,
creates exactly these unsigned draft assets, and retains the same bytes as a
short-lived Actions artifact:

- `sigil-$tag-bazzite-x86_64.tar.gz`
- `sigil-$tag-bazzite-x86_64.tar.gz.sha256`

Move those bytes to the offline publisher machine. After independently
confirming the tag commit and reviewed public key, sign without exporting the
secret key or its passphrase:

```bash
scripts/sign-bazzite-release.sh \
  --tag "$tag" \
  --archive "/absolute/offline/path/sigil-$tag-bazzite-x86_64.tar.gz" \
  --source-commit "$tag_commit" \
  --minisign-key /absolute/offline/path/sigil-release.key \
  --public-key-file /absolute/offline/path/sigil-minisign.pub
```

Before promotion, run the **Portal release** workflow for the same tag. It
requires the untouched two-asset Sigil draft, attaches exactly the signed and
notarized Portal DMG, checksum, and manifest, verifies the five-asset combined
draft, and leaves it unpublished. Transfer only the resulting Sigil `.minisig`
back to the online release operator and attach it to that draft. Do not use
`--clobber` for any asset.

Run **Sigil Release** again with `promote-signed-draft`. It requires the exact
six-asset public-alpha set, verifies the detached Sigil signature with the
committed public key, checks the outer digest, inspects the archive without
extraction, binds both products to the same tag and commit, re-verifies the
Portal digest and release manifest, then mounts the downloaded DMG on a native
arm64 macOS runner and repeats Developer ID, hardened-runtime, Gatekeeper,
stapling, architecture, identifier, and payload checks. Only then does it
replace the incomplete draft notes and publish the prerelease. The workflow has
no input, environment, or secret capable of
receiving the offline secret key.

`scripts/package-bazzite-release.sh` intentionally cannot sign. Its product
mode requires a clean worktree, an existing tag resolving exactly to `HEAD`, a
tag equal to Sigil's Cargo version, and the stable public asset name. The two
development flags are an inseparable escape hatch and cannot claim a release
tag. `scripts/verify-sigil-release.sh` is the common candidate, offline, and
promotion verifier.

## Public Bazzite acceptance record

Repository tests exercise a complete package fixture through fresh install,
same-release rerun, upgrade, rollback, identity/config preservation, hostile
payload rejection, and proof that no `systemctl start`, `restart`, or `enable`
was attempted. That is not a substitute for the public-command gate.

Closing issue #4 still requires operator evidence from clean Bazzite using two
distinct signed candidates. Record, without node IDs or credentials:

1. The Bazzite image/version, x86_64 architecture, public bootstrap SHA-256,
   release asset SHA-256, and release tag/commit.
2. A clean `curl -fsSL https://goq.sh/install-sigil | bash` installation with
   service active/enabled state captured before and after.
3. An idempotent rerun with unchanged `current`, `previous`, identity, and host
   configuration.
4. Promotion to the second signed candidate followed by the same public command,
   proving new `current`, old `previous`, and no implicit service interruption.
5. `sigil-spark-host-rollback` without `--restart`, proving the links reverse,
   all release checksums revalidate, and service state remains unchanged.

## Portal signing boundary

The existing macOS package gate requires a Developer ID Application identity,
hardened runtime, notarization, stapling, strict Gatekeeper assessment, and a
release build without `demo-direct-node`. Store the certificate and
notarization credentials only in the protected GitHub `main` environment with
required reviewers. The release workflow uses those secrets only in its macOS
arm64 publication job.

Pin the public Apple TeamIdentifier in `release/portal-apple-team-id.txt` before
the first tag. The protected secret, produced app signature, promotion verifier,
and UAT verifier must all equal that committed value. The Portal tag-ref build
also emits GitHub artifact attestations; promotion requires each of the three
Portal assets to verify against the exact workflow, tag ref, and source commit.

The first release automation must verify that the git tag, every Cargo package
version, Tauri version, artifact filenames, and manifests agree before it
publishes the GitHub Release. Upload every required asset to a draft release,
verify the complete set, and only then mark it as a prerelease.

goq.sh reads `website/portal-release.json`, which is deliberately committed as
unavailable until an operator verifies the promoted prerelease and submits a
reviewed manifest change. Invalid, missing, or unavailable manifest data never
falls back to a development build. See the
[Portal release runbook](portal-release.md) for the issue #5 procedure.

## Tracked gates

- [Capability tickets and Portal onboarding](https://github.com/FelineStateMachine/goq/issues/3)
- [Signed Sigil package and install command](https://github.com/FelineStateMachine/goq/issues/4)
- [Signed and notarized Portal downloads](https://github.com/FelineStateMachine/goq/issues/5)
- [Public-alpha hardware and network acceptance](https://github.com/FelineStateMachine/goq/issues/6)
