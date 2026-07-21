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

A clean Bazzite image may not include Minisign. The public-alpha gate must
either provision a pinned verifier with its own hard-coded digest or make the
verifier prerequisite explicit; it must never silently downgrade to SHA-only
verification.

## Portal signing boundary

The existing macOS package gate requires a Developer ID Application identity,
hardened runtime, notarization, stapling, strict Gatekeeper assessment, and a
release build without `demo-direct-node`. Store the certificate and App Store
Connect credentials only in a separately protected GitHub `release`
environment with required reviewers.

The first release automation must verify that the git tag, every Cargo package
version, Tauri version, artifact filenames, and manifests agree before it
publishes the GitHub Release. Upload every required asset to a draft release,
verify the complete set, and only then mark it as a prerelease.

## Tracked gates

- [Capability tickets and Portal onboarding](https://github.com/FelineStateMachine/goq/issues/3)
- [Signed Sigil package and install command](https://github.com/FelineStateMachine/goq/issues/4)
- [Signed and notarized Portal downloads](https://github.com/FelineStateMachine/goq/issues/5)
- [Public-alpha hardware and network acceptance](https://github.com/FelineStateMachine/goq/issues/6)
