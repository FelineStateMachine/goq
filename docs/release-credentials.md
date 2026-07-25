# Release credential bring-up

Everything needed to publish goq is already built and already fails closed. The
only thing standing between this repository and a public release is that two
committed trust pins still read `unconfigured`:

| Pin | File | Becomes |
| --- | --- | --- |
| Sigil publisher key | `release/sigil-minisign.pub` | The offline Minisign public key |
| Sigil bootstrap channel | `website/install-sigil` | `publisher_key` and `release_tag` |

Both are free. There is no third credential, because Portal is deliberately not
signed with an Apple Developer ID.

This document is the operator path from those sentinels to a published
prerelease. It closes the credential half of
[issue #4](https://github.com/FelineStateMachine/goq/issues/4) and
[issue #5](https://github.com/FelineStateMachine/goq/issues/5).

Read [Public release delivery](public-release-delivery.md) for the trust model
and [the Portal runbook](portal-release.md) for the publication transaction.

## Read this ordering constraint first

`scripts/verify-sigil-bootstrap.py` accepts exactly two states: fully closed, or
fully open. A commit that configures `release/sigil-minisign.pub` while
`website/install-sigil` still says `unconfigured` is rejected as "partially
configured", and the website gate fails.

The bootstrap also pins one immutable `release_tag`. Combined with the
promotion workflow reading `release/sigil-minisign.pub` from the checked-out
tag, this means the release commit must already contain the real key and a
bootstrap pinned to the tag it is itself becoming.

Pushing either path to `main` triggers a goq.sh deployment. So the release
commit is tagged and released **before** it is merged, or the public install
command goes live pointing at a release that does not exist yet. The sequence
in "First release" below does this in the correct order. Do not shortcut it.

## Part A: the Sigil offline publisher key

This key is the root of trust for every Bazzite install. It must be generated
and must stay on a machine that is not the release runner and never a CI
environment.

```bash
minisign -G -p sigil-minisign.pub -s sigil-release.key
```

Choose a strong passphrase and store it separately from the key file. Back up
`sigil-release.key` offline, in two physically separate locations. There is no
recovery path: losing it means every future release needs a new key and a
reviewed re-pin, and disclosing it means an attacker can sign a package that
the bootstrap will install without complaint.

Prove the passphrase works before you depend on it:

```bash
echo test > /tmp/sigcheck
minisign -Sm /tmp/sigcheck -s sigil-release.key
minisign -Vm /tmp/sigcheck -P "$(tail -1 sigil-minisign.pub)"
```

`sigil-minisign.pub` is two lines, an `untrusted comment:` line and the base64
key. Both matter, in different places:

- `release/sigil-minisign.pub` takes the file verbatim. The verifier tolerates
  the comment line and reads the key from the last non-empty line.
- `website/install-sigil` takes **only the base64 key** in
  `readonly publisher_key="..."`, with no comment line.

The two must match exactly or the gate refuses to open the channel. The key
decodes to exactly 42 bytes beginning `Ed`; anything else is rejected as
malformed.

Record the key fingerprint and publish it through a channel independent of the
website, so a compromised site cannot also redefine what "the real key" is.

## Part B: no Apple credentials

Portal ships as an ad-hoc signed, un-notarized DMG. There is no Apple Developer
Program membership, no Developer ID certificate, and no notarization
credential, and none of them are required to cut a release.

The packaging gate **refuses** to build in ad-hoc mode if any of
`APPLE_SIGNING_IDENTITY`, `APPLE_CERTIFICATE`, `APPLE_API_KEY`,
`APPLE_API_ISSUER`, `APPLE_API_KEY_PATH`, `APPLE_ID`, or `APPLE_PASSWORD` is
set. Do not add them to the `main` environment "just in case".

What replaces Apple as the trust anchor:

- a GitHub/Sigstore build-provenance attestation bound to the exact workflow,
  tag ref, and source commit;
- the published SHA-256 beside the DMG;
- a release manifest that records `signing: "adhoc"` and `notarized: false`.

The cost is real and is stated on the website and in the release notes: macOS
blocks Portal's first launch, and the user must allow it under System Settings,
Privacy and Security.

If a membership is ever purchased, `--signing developer-id` still implements the
full Developer ID path, and `docs/public-release-delivery.md` records what has
to be restored. That branch is not exercised by CI; re-validate it end to end
before trusting it.

## Part C: the protected GitHub environment

Create an environment named `main` with required reviewers and restrict its
deployment branches and tags to the release policy.

It needs **no Apple secrets**. The website deployment needs
`CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`; scope the token to Workers
deployment on the goq.sh account only.

No Minisign secret belongs here, or in any other GitHub environment. The
promotion workflow has no input capable of receiving it, and that is deliberate.

## First release, in order

The tag is `v0.1.0`, matching the version already in every workspace manifest.

1. **Prepare one reviewed release commit** on a branch. It replaces
   `release/sigil-minisign.pub` with the real public key file and opens
   `website/install-sigil` with the base64 `publisher_key` and
   `readonly release_tag="v0.1.0"`. No version bump is needed.

   Verify locally before pushing:

   ```bash
   python3 scripts/verify-sigil-bootstrap.py \
     --bootstrap website/install-sigil \
     --public-key-file release/sigil-minisign.pub
   ./scripts/verify-website.sh
   ./scripts/tests/portal-release.sh
   ```

   Expect `sigil_bootstrap_channel=open`.

2. **Tag that exact commit and push the tag. Do not merge to `main` yet.**
   Merging now would deploy an install command pointing at a release that does
   not exist.

3. **Run Sigil Release with `build-candidate`** against the tag. It creates the
   draft and attaches the unsigned candidate archive and its checksum.

4. **Run Portal release** against the same tag. It builds, verifies, and
   attests the ad-hoc macOS arm64 assets, and separately builds and attests the
   unpublished preview matrix. The draft stays a draft.

   Signing comes after this, not before. The Portal job refuses to start unless
   the draft holds *exactly* the two Sigil candidate assets, and it names its
   own result the five-asset **pre-signature** contract. Attaching the
   `.minisig` first makes that precondition fail.

5. **Sign offline.** Move the candidate bytes to the publisher machine,
   independently confirm the tag commit, then:

   ```bash
   scripts/sign-bazzite-release.sh \
     --tag v0.1.0 \
     --archive /absolute/offline/path/sigil-v0.1.0-linux-glibc2.17-x86_64.tar.gz \
     --source-commit "$tag_commit" \
     --minisign-key /absolute/offline/path/sigil-release.key \
     --public-key-file /absolute/offline/path/sigil-minisign.pub
   ```

   Carry only the resulting `.minisig` back and attach it to the draft, taking
   the draft from five assets to six. Never use `--clobber`.

6. **Run Sigil Release with `promote-signed-draft`.** It requires the exact
   six-asset set, re-verifies both products against the same tag and commit,
   and publishes the prerelease.

7. **Verify as a stranger would**, on a different machine: download the DMG,
   check the digest, run `gh attestation verify`, confirm the Gatekeeper prompt
   behaves as documented, and run the public install command on clean Bazzite.

8. **Merge the release commit to `main`.** This deploys goq.sh with the install
   command open against the now-published tag.

9. **Enable the Portal download** with the follow-up reviewed change to
   `website/portal-release.json` described in
   [the Portal runbook](portal-release.md). It needs the published DMG's
   verified SHA-256, so it can only happen after step 6.

Every subsequent release repeats steps 1 to 9, except that the key is already
correct and only `release_tag` in `website/install-sigil` moves.

## Deliberately not automated

- The Minisign secret key never enters CI. Signing is a manual offline step.
- Opening or moving the public install channel is a reviewed website commit.
- Enabling the Portal download link is a separate reviewed commit made only
  after an operator independently verifies the published artifact.
- Preview matrix artifacts are never published. See
  [Portal platform support](portal-platform-support.md).

If a step fails, the release stays a draft. Fix the source, discard the
unpublished draft after review, and use a new version and tag; do not replace
assets on a tag that anyone may already have fetched.
