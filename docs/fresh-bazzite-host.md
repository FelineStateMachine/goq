# Fresh Bazzite host engineering setup

This document is the maintainer lab log and hardware-acceptance source. It
intentionally contains historical machine names, checkout paths, evidence, and
development workflows. A user activating an installed package should follow
the portable [Sigil host activation guide](sigil-host-activation.md) shipped in
that package instead.

This runbook takes a dedicated x86_64 machine with VA-API H.264 encoding from a
fresh Bazzite install to a remotely managed Sigil host. The proven reference
machine uses AMD graphics, but host admission is based on encoder capability,
not a kernel-driver name. It also defines how `slate` is used as
the temporary Linux stand-in while Steps 0–2 are being built.

The dedicated Bazzite machine is the only target for Gamescope, PipeWire,
hardware encoding, `uinput`, or appliance-level changes. Do not make those
changes on `slate`.

## Scope and machine roles

| Machine | Role | Allowed work |
| --- | --- | --- |
| Development Mac | Portal client and primary source tree | Client development, tests, evidence collection |
| `tank@slate` | Temporary Linux stand-in | Protocol tests, pure Rust host, synthetic H.264, iroh transport, reconnect tests |
| Dedicated Bazzite host | Final appliance | Everything above plus Gamescope, PipeWire, hardware H.264, audio, and `uinput` |

Steps 0–2 do not require Gamescope. They use a synthetic encoded source so the
host boundary can be proven independently. The cutover to Gamescope starts at
Step 3.

## Implemented host command contract

The repository provides these interfaces:

```text
sigil identity init --output <path>
sigil identity show --identity <path>
sigil config check --config <path>
sigil capture probe --source test-pattern --frames <count> --expect-size 1280x800
sigil capture probe --source gamescope-pipewire --config <path> --frames <count>
sigil serve --identity <path> --source test-pattern
sigil serve --config <path>
sigil-probe --node-id <host-node-id> --frames <count>
sigil-probe --node-id <host-node-id> --identity <probe.key> \
  [--invitation <probe.goq-invite>] --frames <count>
portal --dev-connect <host-node-id>
```

The Gamescope source uses GStreamer's `pipewiresrc` and GstVA H.264 encoder,
because stock upstream FFmpeg does not expose a PipeWire video input device.
It resolves exactly one live node from configured PipeWire properties and
fails closed if the node, encoder factory, render-node binding, or required
low-latency encoder properties do not match. This implementation still needs
the attached-display and headless appliance gates below before it is proven on
the target Bazzite image.

The compatibility backend launches the video pipeline through the configured
`gst-launch-1.0` executable and remains the runtime default when
`encoder_backend` is omitted. Published Sigil packages also include the
`in-process-gstreamer` backend so a host can opt into the bounded appsink path
without replacing its package. Sigil must never silently fall back to the
external backend when that explicit configuration is unavailable.

The host identity file is part of the normal daemon design. The client-side
`--dev-connect` passkey bypass is accepted by debug builds and by an explicitly
feature-gated optimized demo build. Ordinary release clients reject that
option, and every accepting client must show a prominent development-mode
indicator while the bypass is active. Build the optimized temporary client
with `cargo tauri build --features demo-direct-node`; do not ship that feature
in a production package.

`--dev-connect` alone does not authorize its ephemeral Portal identity. For an
explicit configured-host hardware test without a security key, run Sigil with
the explicit development feature:

```bash
cargo run -p sigil-host --features demo-auth-bypass -- serve \
  --config <host.toml> --dev-allow-unauthorized --max-runtime-seconds 1800
```

An optimized test binary requires the same feature. This host flag
admits any peer that knows the node ID, grants all session capabilities, prints
a startup warning, and deliberately avoids reading or mutating enrollment
state. It requires `--max-runtime-seconds` between 1 and 3600 so the exposure
expires automatically. Stop that process immediately after the bounded test.
Ordinary release builds and shipped Sigil packages reject the flag.

### Current security boundary

Portal derives a stable client Iroh identity from the security key. Sigil signs
a short-lived invitation bound to its host identity, that exact Portal peer,
an enrollment epoch, a nonce, expiry, and the requested grants. The first media
handshake consumes the invitation atomically; later sessions authenticate the
durably enrolled peer without replaying it. View, pointer/keyboard, and gamepad
grants are intersected with operational protocol capabilities on both sides.

`--dev-connect` remains only a build-time-contained routing bypass in Portal.
It does not grant access to a configured host, and ordinary release builds
reject the option. Only the explicit direct test-pattern proof form of
`sigil serve` omits enrollment; a configured Gamescope appliance fails closed.
Replay, expiry, wrong-peer, cross-host, stale-epoch, and capability-escalation
attempts are rejected before capture starts.

For first enrollment, use Portal's **show portal id** action, then issue the
short-lived file on the host:

```bash
sigil invitation create \
  --config ~/.config/sigil-spark/host.toml \
  --peer PORTAL_PEER_ID \
  --pointer-keyboard \
  --gamepad \
  --output ~/portal.goq-invite
```

Open the file with Portal and confirm its bounded summary with the controller.
After the first accepted media handshake, verify the durable grant with
`sigil enrollment show --config ~/.config/sigil-spark/host.toml`. Revocation is
explicit and invalidates all outstanding invitations:

```bash
fingerprint="$(sigil appliance status \
  --config ~/.config/sigil-spark/host.toml --json | jq -r .identity.host_fingerprint)"
systemctl --user stop sigil-host.service
sigil appliance enrollment-reset \
  --config ~/.config/sigil-spark/host.toml \
  --expected-host-fingerprint "$fingerprint" \
  --json
systemctl --user start sigil-host.service
```

Enrollment reset needs only the durable per-state lifecycle lock when
`XDG_RUNTIME_DIR` is unavailable, so the stopped-host command also works from
an SSH or recovery shell. If a valid XDG runtime root is present, reset also
takes the per-user global lock. An unsafe or malformed XDG runtime directory is
still rejected rather than ignored.

Then disconnect Portal and use **client -> reset enrollment** before importing
the replacement invitation. Portal never silently overwrites a redeemed host
profile.

Do not put an identity seed in an environment variable or command-line
argument. Store it in a mode `0600` file and pass only its path.

For an authenticated headless hardware probe, create a dedicated persistent
probe identity on the client and bind a short-lived invitation to its printed
public ID. This enrollment is intentionally separate from the ordinary Portal
profile and therefore belongs only in an isolated temporary host state:

```bash
# On the client:
umask 077
probe_state="$HOME/.local/state/goq-probe"
install -d -m 0700 "$probe_state"
sigil identity init --output "$probe_state/probe.key"

# On the host's isolated test state/config, using the peer ID printed above:
proof_state="$HOME/.local/state/sigil-spark-hardware-proof"
sigil invitation create \
  --config "$proof_state/host.toml" \
  --peer PROBE_PEER_ID \
  --pointer-keyboard \
  --output "$proof_state/probe.goq-invite"

# From the client, copy the exact invitation over the authenticated SSH path.
scp tank@umpc:.local/state/sigil-spark-hardware-proof/probe.goq-invite \
  "$probe_state/probe.goq-invite"
chmod 0600 "$probe_state/probe.goq-invite"

# First connection redeems the one-time invitation.
sigil-probe --node-id HOST_NODE_ID \
  --identity "$probe_state/probe.key" \
  --invitation "$probe_state/probe.goq-invite" \
  --keyframe-smoke --frames 120

# Later connections use the same identity without replaying the invitation.
sigil-probe --node-id HOST_NODE_ID \
  --identity "$probe_state/probe.key" \
  --keyframe-smoke --frames 120
```

Both credential files must be regular files owned by the current user with no
group or other permissions. The probe verifies the invitation signature,
host/peer binding, expiry, and requested grants before opening a network
connection, sends it only on the initial media handshake, and never prints its
contents. Delete the exact invitation file only after enrollment is confirmed;
if a later session step fails after redemption, retry ticket-free with the same
identity. After `sigil enrollment show` confirms the temporary probe peer,
remove both exact copies:

```bash
rm -- "$HOME/.local/state/goq-probe/probe.goq-invite"
ssh tank@umpc 'rm -- "$HOME/.local/state/sigil-spark-hardware-proof/probe.goq-invite"'
```

## 1. Choose and install Bazzite

Use the Bazzite download picker with:

- Home Theater PC / Steam Gaming Mode image.
- Modern AMD GPU.
- x86_64 architecture.
- KDE desktop for recovery and maintenance.

Use a single AMD GPU where possible. Hybrid-GPU configurations add device
selection ambiguity before the media path is proven.

During installation:

1. Use UEFI boot.
2. Connect wired Ethernet.
3. Set the hostname to `sigil-host` or another stable local DNS name.
4. Create the gaming account. This runbook uses `sigil` in examples.
5. Use the entire dedicated disk unless the machine has another explicit role.
6. Boot into Steam Gaming Mode once, then switch to Desktop Mode for setup.

Choose the disk-unlock model before installation. Passphrase-only LUKS blocks
an unattended headless cold boot. TPM-backed unlock can satisfy the appliance
boot requirement, but only when an offline recovery key is stored and tested.
Record the decision and prove one recovery boot before relying on the host.

Follow the current [Bazzite installation guide](https://docs.bazzite.gg/General/Installation_Guide/install-guide/)
for image verification, Secure Boot, and installer details.

## 2. Update once and pin the known-good deployment

From a host terminal in Desktop Mode:

```bash
set -euo pipefail
rpm-ostree status -v | tee "$HOME/bazzite-pre-update.txt"
df -h / /var "$HOME"
df --output=pcent /var | tail -1 | tr -dc '0-9' | {
  read -r used
  test "$used" -le 97
}
ujust update
sudo systemctl reboot
```

After reboot, record and pin the deployment:

```bash
rpm-ostree status -v
rpm-ostree status -v | grep -q 'ostree-image-signed'
sudo ostree admin pin 0
```

Bazzite-Deck/HTPC updates are normally applied manually through Steam Gaming
Mode or with `ujust update`. Pinning gives the appliance a known-good bootable
deployment before Sigil-specific work begins.

Do not layer compilers, Rust, FFmpeg development packages, or build tools with
`rpm-ostree`. Bazzite recommends containers for development workflows and warns
that package layering can interfere with upgrades.

References:

- [Bazzite update guide](https://docs.bazzite.gg/Installing_and_Managing_Software/Updates_Rollbacks_and_Rebasing/updating_guide/)
- [Bazzite rollback guide](https://docs.bazzite.gg/Installing_and_Managing_Software/Updates_Rollbacks_and_Rebasing/rolling_back_system_updates/)
- [Bazzite software installation guidance](https://docs.bazzite.gg/Installing_and_Managing_Software/software-intro/)

## 3. Establish SSH access

On the Bazzite host, confirm that OpenSSH is present and enable it:

```bash
sudo systemctl enable --now sshd.service
systemctl status sshd.service --no-pager
```

If `sshd.service` does not exist, stop and verify the selected Bazzite image
before layering packages. OpenSSH is expected on the chosen host image.

If `firewalld` is active, inspect its active zones and allow SSH only in the
management interface's zone:

```bash
management_interface="<wired-management-interface>"
if sudo systemctl is-active --quiet firewalld; then
  management_zone="$(sudo firewall-cmd --get-zone-of-interface="$management_interface")"
  test -n "$management_zone" && test "$management_zone" != no
  sudo firewall-cmd --zone="$management_zone" --permanent --add-service=ssh
  sudo firewall-cmd --reload
fi
```

At the host console, record its ED25519 fingerprint:

```bash
sudo ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub
ip -brief address show dev "$management_interface"
```

Compare that fingerprint when the Mac first connects. If `.local` name
resolution is not available yet, use the displayed management IP. Create a
dedicated management key and install its public half:

If the host uses Tailscale SSH, its ACL may present a browser check before
OpenSSH authentication. A local or temporary password is not consulted until
that tailnet check succeeds. Complete the displayed URL using the authorized
tailnet identity, then continue with the dedicated key below. Do not weaken
`sshd` or the tailnet ACL merely to bypass the check. For recovery, retain a
separately tested LAN management path or document the tailnet check policy and
its operator account; a physically headless appliance must not depend on an
unknown interactive account flow.

```bash
test -f "$HOME/.ssh/sigil-bazzite_ed25519" || \
  ssh-keygen -t ed25519 -a 64 -f "$HOME/.ssh/sigil-bazzite_ed25519"
management_key="$(cat "$HOME/.ssh/sigil-bazzite_ed25519.pub")"
printf '%s\n' "$management_key" | ssh sigil@sigil-host.local \
  'umask 077; IFS= read -r management_key; install -d -m 0700 "$HOME/.ssh"; touch "$HOME/.ssh/authorized_keys"; chmod 0600 "$HOME/.ssh/authorized_keys"; grep -qxF "$management_key" "$HOME/.ssh/authorized_keys" || printf "%s\n" "$management_key" >> "$HOME/.ssh/authorized_keys"'
ssh -o IdentitiesOnly=yes \
  -o PasswordAuthentication=no \
  -i "$HOME/.ssh/sigil-bazzite_ed25519" \
  sigil@sigil-host.local hostname
```

Only after key-only login succeeds, add an SSH hardening drop-in on the host:

```bash
sudo install -d -m 0755 /etc/ssh/sshd_config.d
sudo tee /etc/ssh/sshd_config.d/10-sigil.conf >/dev/null <<'EOF'
PubkeyAuthentication yes
AuthenticationMethods publickey
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
AllowUsers sigil
AllowAgentForwarding no
AllowTcpForwarding no
PermitTunnel no
X11Forwarding no
EOF
sudo sshd -t
sudo sshd -T | grep -E '^(pubkeyauthentication|authenticationmethods|passwordauthentication|kbdinteractiveauthentication|permitrootlogin|allowusers|allowagentforwarding|allowtcpforwarding|permittunnel|x11forwarding) '
sudo systemctl reload sshd.service
```

Keep the original session and a local keyboard available until a second
key-only SSH login succeeds.

## 4. Verify the fresh host before adding Sigil

Run this while a display is still attached:

```bash
./scripts/bazzite-inventory.sh | tee "$HOME/bazzite-inventory.txt"
./scripts/bazzite-inventory.sh --smoke | tee "$HOME/bazzite-smoke.txt"
```

The script selects the first accessible render node that completes a real
one-frame VA-API H.264 encode; it does not assume a driver name or
`renderD128`. The equivalent individual checks are shown below for recovery
and review:

```bash
hostnamectl
timedatectl
uname -a
rpm-ostree status -v
getenforce
ss -lntup
systemctl list-unit-files --state=enabled
mokutil --sb-state || true
gamescope --version
pipewire --version
ls -l /dev/dri
id
vulkaninfo --summary
render_node="$({
  for node in /dev/dri/renderD*; do
    test -c "$node" && test -r "$node" && test -w "$node" || continue
    ffmpeg -hide_banner -loglevel error \
      -vaapi_device "$node" \
      -f lavfi -i color=size=128x128:rate=1 \
      -vf 'format=nv12,hwupload' \
      -c:v h264_vaapi -frames:v 1 -f null - >/dev/null 2>&1 || continue
    printf '%s\n' "$node"
  done
} | head -n 1)"
test -n "$render_node"
udevadm info --query=property --name="$render_node" | grep -E '^(DEVNAME|ID_PATH|ID_PATH_TAG)='
readlink -f "/sys/class/drm/$(basename "$render_node")/device/driver"
test -r "$render_node" && test -w "$render_node"
vainfo --display drm --device "$render_node"
ffmpeg -hide_banner -encoders 2>/dev/null | grep -E 'h264_(vaapi|amf)'
ffmpeg -hide_banner -encoders 2>/dev/null | grep -q 'libx264'
ffmpeg -hide_banner -loglevel warning \
  -vaapi_device "$render_node" \
  -f lavfi -i testsrc2=size=1280x800:rate=60 \
  -vf 'format=nv12,hwupload' \
  -c:v h264_vaapi \
  -frames:v 600 \
  -f null -
```

Required observations:

- The gaming user can open a `/dev/dri/renderD*` render node.
- Vulkan selects the intended gaming GPU.
- VA-API exposes H.264 encoding and completes the 600-frame smoke encode.
- FFmpeg exposes `libx264` for the bounded synthetic Steps 0–2 source.
- PipeWire and Gamescope are installed.
- The clock reports NTP synchronization. NTP is suitable for correlating logs;
  one-way latency measurements still require a same-clock monotonic measurement
  or explicit clock-offset estimation.

Record the output before continuing. Missing hardware encoding is a host-image
or driver problem, not an application fallback opportunity.

## 5. Create persistent user-space locations

As the gaming user:

```bash
install -d -m 0700 \
  "$HOME/.config/sigil-spark" \
  "$HOME/.local/libexec/sigil-spark" \
  "$HOME/.local/libexec/sigil-spark/releases" \
  "$HOME/.local/share/sigil-spark/identity" \
  "$HOME/.local/state/sigil-spark" \
  "$HOME/.local/state/sigil-spark/runtime" \
  "$HOME/.config/systemd/user"
```

Use these locations consistently:

| Content | Location |
| --- | --- |
| Host releases | `~/.local/libexec/sigil-spark/releases/<commit>/sigil` (`sigil-host` is a compatibility copy) |
| Current binary | `~/.local/libexec/sigil-spark/current/sigil` |
| Configuration | `~/.config/sigil-spark/host.toml` |
| Read-only host identity | `~/.local/share/sigil-spark/identity/host.key` |
| Writable runtime state | `~/.local/state/sigil-spark/runtime/` |
| User service | `~/.config/systemd/user/sigil-host.service` |

This avoids modifying Bazzite's immutable base image.

Sigil requires the configuration file and its immediate directory to be owned
by the service user and not writable by group or other users. This appliance
layout deliberately uses mode `0700`; a conventional owner-controlled mode
`0755` configuration directory is also valid, while group- or world-writable
directories fail during `config check` and daemon startup before any managed
configuration transaction can begin.

## 6. Prepare a containerized build environment

Bazzite recommends Distrobox for development packages. The repository must
first contain a committed `rust-toolchain.toml`, `Cargo.lock`, and a clean
source revision. Determine the Fedora base version, pull the matching toolbox
image, and record its immutable digest:

```bash
. /etc/os-release
image_tag="registry.fedoraproject.org/fedora-toolbox:${VERSION_ID}"
podman pull "$image_tag"
image_digest="$(podman image inspect --format '{{index .RepoDigests 0}}' "$image_tag")"
printf '%s\n' "$image_digest"
distrobox create \
  --name sigil-dev \
  --image "$image_digest"
distrobox enter sigil-dev
```

Inside the container:

```bash
sudo dnf install -y --setopt=install_weak_deps=False \
  clang cmake curl gcc gcc-c++ git make \
  gstreamer1-devel gstreamer1-plugins-base-devel \
  openssl-devel pipewire-devel pkgconf-pkg-config systemd-devel

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
  sh -s -- -y --profile minimal --default-toolchain none
source "$HOME/.cargo/env"
rpm -qa | sort > "$HOME/sigil-dev-rpms.txt"
```

`gstreamer1-devel` supplies the core API and
`gstreamer1-plugins-base-devel` supplies the app/video APIs linked by the
optional in-process backend. They are build dependencies only; do not copy
headers or development libraries into the Sigil runtime archive.

The toolchain pinned by the repository must be Rust 1.91 or newer. Save the
container digest and exact RPM NEVRAs with the evidence. Distrobox is
host-integrated and is not a security sandbox. Build only the protocol and pure
host crates on the appliance; the Tauri client remains a Mac build. Remove the
`sigil-dev` box after installing and validating the release binary.

## 7. Transfer the working tree

Proof builds use committed, revision-addressed source. They must never copy a
dirty tree, `.env` file, identity, or evidence directory. On the Mac, create a
bundle for the intentional checkpoint:

```bash
cd /Users/dami/Developer/sigil-spark
test -z "$(git status --porcelain)" || {
  echo "Commit or intentionally discard all changes before a proof build." >&2
  exit 1
}
sigil_rev="$(git rev-parse HEAD)"
sigil_short="$(git rev-parse --short=12 HEAD)"
sigil_bundle="/tmp/sigil-spark-${sigil_short}.bundle"
git bundle create "$sigil_bundle" HEAD
git bundle verify "$sigil_bundle"
bundle_sha="$(shasum -a 256 "$sigil_bundle" | awk '{print $1}')"
printf '%s  %s\n' "$bundle_sha" "$(basename "$sigil_bundle")" >"$sigil_bundle.sha256"
ssh -i "$HOME/.ssh/sigil-bazzite_ed25519" sigil@sigil-host.local \
  'install -d -m 0700 "$HOME/Developer"'
scp -i "$HOME/.ssh/sigil-bazzite_ed25519" \
  "$sigil_bundle" "$sigil_bundle.sha256" sigil@sigil-host.local:Developer/
ssh -i "$HOME/.ssh/sigil-bazzite_ed25519" sigil@sigil-host.local \
  "cd \"\$HOME/Developer\" && \
   sha256sum -c \"sigil-spark-${sigil_short}.bundle.sha256\" && \
   git bundle list-heads \"sigil-spark-${sigil_short}.bundle\" && \
   mkdir -p \"\$HOME/Developer/sigil-spark-revisions\" && \
   git clone \"\$HOME/Developer/sigil-spark-${sigil_short}.bundle\" \
     \"\$HOME/Developer/sigil-spark-revisions/${sigil_short}\" && \
   git -C \"\$HOME/Developer/sigil-spark-revisions/${sigil_short}\" \
     checkout --detach ${sigil_rev} && \
   test \"\$(git -C \"\$HOME/Developer/sigil-spark-revisions/${sigil_short}\" rev-parse HEAD)\" = \"${sigil_rev}\""
```

Once a shared Git remote exists, replace the bundle with a normal fetch and
detached checkout of the same exact commit.

Build inside the Distrobox:

```bash
distrobox enter sigil-dev
sigil_rev="<full-commit>"
sigil_short="${sigil_rev:0:12}"
cd "$HOME/Developer/sigil-spark-revisions/$sigil_short"
source "$HOME/.cargo/env"
toolchain="$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml)"
test -n "$toolchain"
rustup toolchain install "$toolchain"
rustup show active-toolchain
rustc -vV
cargo --version
cargo test --locked -p sigil-protocol
cargo build --locked -p sigil-host --release
exit
```

Install the resulting binary as the gaming user:

```bash
sigil_rev="<full-commit>"
sigil_short="${sigil_rev:0:12}"
sigil_root="$HOME/.local/libexec/sigil-spark"
sigil_release="$sigil_root/releases/$sigil_rev"
install -d -m 0755 "$sigil_release"
install -m 0755 \
  "$HOME/Developer/sigil-spark-revisions/$sigil_short/target/release/sigil" \
  "$sigil_release/sigil"
install -m 0755 "$sigil_release/sigil" "$sigil_release/sigil-host"
sha256sum "$sigil_release/sigil" "$sigil_release/sigil-host"
ldd "$sigil_release/sigil" | tee "$HOME/sigil-${sigil_short}.ldd"
if grep -q 'not found' "$HOME/sigil-${sigil_short}.ldd"; then
  echo "Host runtime dependency missing." >&2
  exit 1
fi
"$sigil_release/sigil" --version
cd "$sigil_root"
test ! -e current || test -L current
ln -sfnT "releases/$sigil_rev" current
sha256sum current/sigil current/sigil-host
current/sigil --version
```

For a demo deployment built and hashed on the development Mac, transfer both
`sigil` and `sigil-probe` plus `scripts/stage-bazzite-release.sh`, then use
the stager instead of the manual install block above:

The thin stager is restricted to an unmanaged development layout. If the
service, audio, rollback, or udev assets are package-managed links that follow
`current`, it fails before staging and instructs you to build the complete
runtime package and run `payload/stage-this-release.sh` instead.

```bash
scripts/stage-bazzite-release.sh \
  --release-id <commit-or-source-snapshot-sha256> \
  --host-binary <absolute-path> --host-sha256 <sha256> \
  --probe-binary <absolute-path> --probe-sha256 <sha256>
```

The expected hashes must come from the trusted development machine, not from
an unverified copy on the host. The stager refuses unsafe inputs and install
directories, validates runtime linking and bounded startup, and atomically
updates `current`. It deliberately does not create an identity, install a
hardware configuration, or start/enable the service; those remain the next
separate gates.

The preferred host artifact is the deterministic, allowlisted runtime package.
It contains the generic Linux host/probe binaries, installer and rollback tool,
systemd/PipeWire/udev assets, license, complete checksums, and build provenance.
It cannot include the worktree, credentials, identity, hardware configuration,
or evidence. Product packaging exports clean tagged `HEAD`, builds both
binaries with locked `cargo-zigbuild` in an isolated target directory, and
never accepts caller-supplied binaries. It fails closed when the worktree is
dirty, the tag does not resolve to `HEAD`, the tag differs from Sigil's Cargo
version, or the output does not use the stable public asset name. The builder
never receives the offline Minisign secret:

```bash
cd /Users/dami/Developer/sigil-spark
source ~/.cargo/env
scripts/package-bazzite-release.sh \
  --release-tag v0.1.0 \
  --output /tmp/sigil-v0.1.0-bazzite-x86_64.tar.gz

# Verify the candidate before it crosses the offline signing boundary.
scripts/verify-sigil-release.sh \
  --tag v0.1.0 \
  --archive /tmp/sigil-v0.1.0-bazzite-x86_64.tar.gz \
  --source-commit "$(git rev-parse HEAD)" \
  --candidate

# After the offline signer returns the detached signature, verify with the
# reviewed public key before extraction. See docs/public-release-delivery.md.
scripts/verify-sigil-release.sh \
  --tag v0.1.0 \
  --archive /tmp/sigil-v0.1.0-bazzite-x86_64.tar.gz \
  --source-commit "$(git rev-parse HEAD)" \
  --public-key-file /absolute/path/to/sigil-minisign.pub
shasum -a 256 -c /tmp/sigil-v0.1.0-bazzite-x86_64.tar.gz.sha256
scp /tmp/sigil-v0.1.0-bazzite-x86_64.tar.gz tank@umpc:/tmp/

ssh tank@umpc '
  set -eu
  incoming="$HOME/.local/share/sigil-spark/incoming"
  install -d -m 0700 "$incoming"
  run_dir="$(mktemp -d "$incoming/package.XXXXXX")"
  tar -tzf /tmp/sigil-v0.1.0-bazzite-x86_64.tar.gz
  tar -xzf /tmp/sigil-v0.1.0-bazzite-x86_64.tar.gz -C "$run_dir"
  cd "$run_dir/payload"
  sha256sum -c PACKAGE-SHA256SUMS
  bash -n stage-this-release.sh
  ./stage-this-release.sh
'
```

The public-key signature must be checked **before extraction**; checksums inside
the archive prove consistency but cannot establish publisher authenticity.
Keep the extracted incoming directory until the deployment is accepted. The
installer locks concurrent operations, verifies the exact payload allowlist,
stages and validates the entire release, then changes `current` last. It records
the former release as `previous`; `sigil-spark-host-rollback` revalidates every
target file before swapping `current`/`previous`. Neither path creates identity
or `host.toml`, changes `/etc`, restarts PipeWire, or starts/enables the service.
Before changing any package-managed user asset, the installer preflights all of
them. It can adopt an operator-owned regular file only when it is byte-identical
to the new release asset; any local modification or unsafe target rejects the
whole asset migration without changing an earlier destination.

For a temporary dirty and unsigned development package only, omit the release
tag and use both `--allow-dirty --allow-unsigned`. The manifest records that
state and the package prints `publisher_signature=absent-development`; never
publish that artifact. Externally built host/probe binaries are accepted only
as an all-or-none pair with both development flags, and their manifest records
`binary_provenance=caller-supplied-unverified`:

```bash
scripts/package-bazzite-release.sh \
  --output /tmp/sigil-spark-host-prebuilt-dev.tar.gz \
  --allow-dirty --allow-unsigned \
  --host-binary /absolute/path/to/sigil \
  --probe-binary /absolute/path/to/sigil-probe
```

## 8. Create the host identity and synthetic configuration

Create the identity file explicitly with a restrictive umask, then print only
the public node ID:

```bash
umask 077
identity="$HOME/.local/share/sigil-spark/identity/host.key"
if ! test -e "$identity"; then
  "$HOME/.local/libexec/sigil-spark/current/sigil" identity init \
    --output "$identity"
fi
test ! -L "$identity"
chmod 0600 "$identity"
stat -c '%a %U %G %n' "$identity"
"$HOME/.local/libexec/sigil-spark/current/sigil" identity show \
  --identity "$identity"
```

`identity init` must use create-new semantics, reject symlinks and existing
files, and set restrictive permissions itself. Rerunning setup must never
silently rotate the host identity.

For Steps 0–2, configure a 1280×800 synthetic H.264 source. The daemon's strict
TOML schema accepts these values:

```bash
config="$HOME/.config/sigil-spark/host.toml"
ffmpeg_path="$(readlink -f "$(command -v ffmpeg)")"
test -x "$ffmpeg_path"
ffmpeg -hide_banner -encoders 2>/dev/null | grep -q libx264
cat >"$config" <<EOF
identity_path = "$HOME/.local/share/sigil-spark/identity/host.key"
state_path = "$HOME/.local/state/sigil-spark/runtime"
source = "test-pattern"
width = 1280
height = 800
framerate = 60
codec = "h264"
input_mode = "log"
ffmpeg_path = "$ffmpeg_path"
EOF
chmod 0600 "$config"
stat -c '%a %U %G %n' "$config"
"$HOME/.local/libexec/sigil-spark/current/sigil" config check \
  --config "$config"
install -m 0600 "$config" \
  "$HOME/.local/libexec/sigil-spark/current/host.toml"
"$HOME/.local/libexec/sigil-spark/current/sigil" serve \
  --config "$config" --max-runtime-seconds 10
```

The daemon must reject malformed configuration, unknown fields, and identity
files writable by group or other users.

Use `input_mode = "log"` only for the bounded proof so receipt can be observed
without injecting anything. Return it to `disabled` before leaving the host
unattended.

The identity path is host-specific. Generate a new identity on the Bazzite
appliance rather than copying the temporary identity from `slate`.

## 9. Install the user service

Create `~/.config/systemd/user/sigil-host.service`:

```ini
[Unit]
Description=Sigil streaming host
Wants=pipewire.socket
After=pipewire.socket
ConditionPathExists=%h/.config/sigil-spark/host.toml
StartLimitIntervalSec=0

[Service]
Type=simple
Environment=DISPLAY=:0
# Compatibility window: the service uses the byte-identical legacy filename
# so an older rollback helper can still validate and reactivate this release.
ExecStart=%h/.local/libexec/sigil-spark/current/sigil-host serve --config %h/.config/sigil-spark/host.toml
Restart=on-failure
RestartSec=1
TimeoutStopSec=10
KillSignal=SIGINT
UMask=0077
CPUQuota=400%
MemoryHigh=1536M
MemoryMax=2G
TasksMax=256
LimitNOFILE=8192
NoNewPrivileges=true
RestrictSUIDSGID=true

[Install]
WantedBy=default.target
```

Verify, load, and start it for the current proof session. Do not enable it yet:

```bash
systemd-analyze --user verify "$HOME/.config/systemd/user/sigil-host.service"
systemctl --user daemon-reload
systemctl --user start sigil-host.service
systemctl --user status sigil-host.service --no-pager
journalctl --user -u sigil-host.service -n 100 --no-pager
```

`serve` performs the same strict configuration and live capture/input preflight
before binding its network endpoint, so a second `ExecStartPre=config check`
would only duplicate work. The unlimited start interval and one-second delay
are deliberate: after a cold boot the user manager may be ready before udev
has replaced Bazzite's static `/dev/uinput` node permissions or before
Gamescope publishes its PipeWire node. The unit retries safely without ever
exposing a partially initialized host. Always run the manual `config check`
shown above before the first start and again immediately before cutover.

Do not add `PrivateTmp`, `ProtectSystem`, or `ProtectHome` to this **user**
unit. On current Bazzite/systemd those mount-namespace directives run the user
service in an unprivileged user namespace where root-owned device nodes appear
owned by the overflow UID/GID. That defeats Sigil's exact `/dev/uinput`
owner/group validation and makes the service fail closed. `NoNewPrivileges`,
`RestrictSUIDSGID`, the resource limits, mode-0600 configuration and identity,
and the dedicated `sigil-uinput` group remain active. A future system-level
unit may reintroduce mount namespacing while running the process as `tank`.

Do not enable user lingering initially. The Steam Gaming Mode login owns the
PipeWire session. Revisit lingering only if the final service must start before
the gaming account is logged in. Iroh must tolerate initial offline state and
network changes rather than depending on a user-manager `network-online.target`.

## 10. Use `slate` until the Bazzite cutover

`slate` is allowed to host only the pure Rust and synthetic-source portions of
Steps 0–2. The repository must provide a pinned `flake.nix` and `flake.lock`
before this proof begins. Nix may add garbage-collectable paths to the shared
Nix store, but Sigil files stay in run-specific directories under
`~/Developer/sigil-spark-slate-probe` and
`~/.local/state/sigil-spark-slate-probe`.

Do not use `sudo`, change the firewall, change users or groups, install a unit,
or leave an unattended background process on `slate`.

From the Mac:

```bash
cd /Users/dami/Developer/sigil-spark
test -z "$(git status --porcelain)"
sigil_rev="$(git rev-parse HEAD)"
sigil_short="$(git rev-parse --short=12 HEAD)"
sigil_bundle="/tmp/sigil-spark-${sigil_short}.bundle"
git bundle create "$sigil_bundle" HEAD
shasum -a 256 "$sigil_bundle"
ssh tank@slate \
  'umask 077; install -d -m 0700 "$HOME/Developer/sigil-spark-slate-probe"'
scp "$sigil_bundle" tank@slate:Developer/sigil-spark-slate-probe/
ssh tank@slate \
  "git clone \
    \"\$HOME/Developer/sigil-spark-slate-probe/sigil-spark-${sigil_short}.bundle\" \
    \"\$HOME/Developer/sigil-spark-slate-probe/${sigil_short}\" && \
   git -C \"\$HOME/Developer/sigil-spark-slate-probe/${sigil_short}\" \
     checkout --detach ${sigil_rev}"
```

Run tests with the repository's pinned Nix environment. Limit build parallelism
because this is a shared host:

```bash
ssh -t tank@slate
sigil_short="<12-character-commit>"
run_id="${sigil_short}-$(date -u +%Y%m%dT%H%M%SZ)"
probe_src="$HOME/Developer/sigil-spark-slate-probe/$sigil_short"
probe_state="$HOME/.local/state/sigil-spark-slate-probe/$run_id"
umask 077
install -d -m 0700 "$probe_state"
cd "$probe_src"
export CARGO_BUILD_JOBS=4
nix develop --option max-jobs 4 --option cores 2 --command bash -lc \
  'rustc --version && cargo --version && \
   ffmpeg -hide_banner -encoders 2>/dev/null | grep -q libx264 && \
   cargo fmt --all -- --check && \
   cargo test --locked -p sigil-protocol -p sigil-host && \
   cargo clippy --locked -p sigil-protocol -p sigil-host \
     --all-targets -- -D warnings'
nix develop --option max-jobs 4 --option cores 2 --command \
  cargo run --locked -p sigil-host -- capture probe \
    --source test-pattern --frames 600 --expect-size 1280x800
nix develop --option max-jobs 4 --option cores 2 --command \
  cargo run --locked -p sigil-host -- identity init \
  --output "$probe_state/host.key"
nix develop --option max-jobs 4 --option cores 2 --command \
  cargo run --locked -p sigil-host -- identity show \
  --identity "$probe_state/host.key"
stat -c 'identity_mode=%a' "$probe_state/host.key"
ulimit -n 8192
nix develop --option max-jobs 4 --option cores 2 --command \
  systemd-run --user --scope --collect --quiet \
    -p CPUQuota=400% \
    -p MemoryHigh=1536M \
    -p MemoryMax=2G \
    -p TasksMax=256 \
  timeout --signal=INT --kill-after=10s 2h \
  cargo run --locked -p sigil-host -- serve \
    --identity "$probe_state/host.key" \
    --state-path "$probe_state/runtime" \
    --source test-pattern
```

Keep this SSH terminal open. `tank` does not have lingering enabled, so the
foreground scope is intentionally tied to the proof session. Do not open a
fixed UDP port or change Slate's firewall; Iroh relay fallback is part of this
stand-in test.

On the Mac, connect with the node ID printed by `slate`:

```bash
cd /Users/dami/Developer/sigil-spark
source "$HOME/.cargo/env"
cargo run -p portal -- --dev-connect <slate-node-id>
```

Before opening the UI, prove the same v1 media and input session headlessly:

```bash
source "$HOME/.cargo/env"
cargo run --locked -p sigil-host --bin sigil-probe -- \
  --node-id <slate-node-id> --frames 300 --timeout-seconds 15
```

The probe fails unless its first media frame is an IDR with SPS/PPS, it receives
`dimensions=1280x800`, and `sequence_gaps=0`. Its default input event is a
content-free liveness probe that requires a bounded host acknowledgment on the
independent input stream; it does not move the pointer. Record
`input_ack_micros`; the Slate host logs only the event type because proof-mode
input is log-only.

For the one-client and reconnect gates, keep the Slate foreground host running
and use the already-built probe binary from a second Mac terminal:

```bash
node_id="<slate-node-id>"
cargo build --locked -p sigil-host --bin sigil-probe
target/debug/sigil-probe --node-id "$node_id" --frames 3600 \
  > /tmp/sigil-primary-probe.log &
primary_pid=$!
sleep 2
if target/debug/sigil-probe --node-id "$node_id" --frames 1; then
  echo "Second client was accepted unexpectedly." >&2
  kill "$primary_pid"
  exit 1
fi
wait "$primary_pid"
grep -E 'probe=ok|frames=3600|sequence_gaps=0' \
  /tmp/sigil-primary-probe.log

for cycle in $(seq 1 100); do
  target/debug/sigil-probe --node-id "$node_id" --frames 1 \
    > /dev/null || {
      echo "Reconnect failed at cycle $cycle." >&2
      exit 1
    }
done
echo "reconnect_cycles=100"
```

The second probe must fail with `host already has an active client`; the
primary must still finish with zero sequence gaps. Remove the temporary log
after saving any evidence needed for the demo.

The 2026-07-20/21 WIP proof completed 600 native Slate-to-Mac frames in 9.61
seconds with 10 keyframes, zero sequence gaps, a direct path, 7.04 ms RTT, and
a 7.81 ms input acknowledgment. A concurrent second client was rejected with
the exact one-active-client error. On 2026-07-21 the final release-profile
binaries completed 100/100 fresh media/input reconnects with deterministic
cleanup after a 600-frame zero-gap primary session. The final raw Tauri channel
then rendered more than 7,900 frames at 59.9 fps. During a clean
one-minute soak its counters remained fixed after startup: transport 0,
frontend 56 while joining mid-GOP, and decoder 2 while configuring WebCodecs.
The handoff is capped at four frames (about 67 ms at 60 fps), WebCodecs at two,
and new diagnostics report the three drop sources independently. Its `DEV
DIRECT-NODE · NOT AUTH` warning remained visible above the stream. The host
waits for a keyframe at session start and after a detected discontinuity, so a
newly connected decoder is never handed a delta frame as its first access
unit. Re-run these gates for the exact source snapshot used in a demo; do not
treat the WIP path as a release.

Before cutover, the `slate` proof must cover:

- Golden protocol vectors on macOS arm64 and Linux x86_64.
- Synthetic H.264 reception and WebCodecs decode.
- Independently acknowledged input transport, even though injection is stubbed
  on the stand-in.
- Second-client rejection.
- Clean shutdown and 100 connect/disconnect cycles.
- No unbounded queue or increasing process RSS.

Do not run Gamescope, change the display manager, configure `uinput`, or install
a persistent Sigil service on `slate`.

At cutover, confirm there is no running probe before removing only the exact
run-specific paths:

```bash
pgrep -a sigil && {
  echo "Stop the foreground Sigil probe before cleanup." >&2
  exit 1
}
probe_src="$HOME/Developer/sigil-spark-slate-probe/<12-character-commit>"
probe_state="$HOME/.local/state/sigil-spark-slate-probe/<run-id>"
case "$probe_src" in
  "$HOME/Developer/sigil-spark-slate-probe/"?*) ;;
  *) exit 1 ;;
esac
case "$probe_state" in
  "$HOME/.local/state/sigil-spark-slate-probe/"?*) ;;
  *) exit 1 ;;
esac
test "$probe_src" != "$HOME/Developer/sigil-spark-slate-probe"
test "$probe_state" != "$HOME/.local/state/sigil-spark-slate-probe"
printf 'removing source=%q\nremoving state=%q\n' "$probe_src" "$probe_state"
rm -rf -- "$probe_src" "$probe_state"
```

Review the two expanded paths before running the final command. Nix store paths
are left to the server's normal garbage-collection policy.

## 11. Validate the Bazzite Gamescope boundary

First validate the stock Bazzite Steam Gaming Mode session while a display is
attached. From SSH:

```bash
systemctl --user status pipewire.service pipewire.socket wireplumber.service --no-pager
pgrep -a gamescope
pgrep -a steam
pw-cli ls Node
pw-dump | jq '[
  .[]
  | select(.type == "PipeWire:Interface:Node")
  | .info.props
  | select(
      .["node.name"] == "gamescope"
      and .["media.class"] == "Video/Source"
    )
]'
journalctl --user -b --no-pager | grep -Ei 'gamescope|pipewire.*node'
```

Gamescope should publish a PipeWire stream with both `node.name=gamescope` and
`media.class=Video/Source`. Select by those properties, not by numeric node ID,
because node IDs change across boots. Inventory the exact executable paths,
render-node candidates, and dynamically registered GstVA H.264 factories:

```bash
pw_dump_path="$(readlink -f "$(command -v pw-dump)")"
gst_launch_path="$(readlink -f "$(command -v gst-launch-1.0)")"
gst_inspect_path="$(readlink -f "$(command -v gst-inspect-1.0)")"
ffmpeg_path="$(readlink -f "$(command -v ffmpeg)")"
test -x "$pw_dump_path"
test -x "$gst_launch_path"
test -x "$gst_inspect_path"
test -x "$ffmpeg_path"

for node in /sys/class/drm/renderD*; do
  test -e "$node/device/driver" || continue
  printf 'candidate=%s driver=%s pci_device=%s\n' \
    "/dev/dri/$(basename "$node")" \
    "$(basename "$(readlink -f "$node/device/driver")")" \
    "$(basename "$(readlink -f "$node/device")")"
done
render_node="<exact encode-capable render node chosen from the inventory>"
test -n "$render_node"
test -r "$render_node" && test -w "$render_node"

gst-inspect-1.0 | awk '
  $2 ~ /^va(renderD[0-9]+)?h264(lp)?enc:$/ {
    sub(/:$/, "", $2)
    print $2
  }
' | while read -r encoder; do
  printf '\n### %s\n' "$encoder"
  gst-inspect-1.0 "$encoder" | sed -n \
    -e '/^[[:space:]]*device-path[[:space:]]*:/,+4p' \
    -e '/^[[:space:]]*rate-control[[:space:]]*:/,+20p'
done

for element in appsink pipewiresrc queue videoconvert videoscale videorate h264parse fdsink; do
  gst-inspect-1.0 "$element" >/dev/null
done

rpm -q gstreamer1 gstreamer1-plugins-base gstreamer1-plugins-bad-free
```

Record the exact runtime package NEVRAs. `appsink` is mandatory for a binary
built with the in-process feature, and the package installer must resolve its
linked GStreamer core/app/video libraries with `ldd`. `pipewiresrc`,
conversion/parser elements, and the chosen GstVA plugin remain required for
either video backend. The external `gst-launch` path is also still used for
audio capture when audio is enabled.

### Grant the gaming user only the uinput capability

Do not add the gaming user to the broad `input` group and do not run the host
as root. Create a dedicated group that can open only the kernel's uinput misc
device:

```bash
getent group sigil-uinput >/dev/null || sudo groupadd --system sigil-uinput
sudo usermod --append --groups sigil-uinput "$USER"
sudo tee /etc/udev/rules.d/72-sigil-uinput.rules >/dev/null <<'EOF'
KERNEL=="uinput", SUBSYSTEM=="misc", TAG-="uaccess"
EOF
sudo install -o root -g root -m 0644 \
  scripts/70-sigil-remote-input.rules \
  /etc/udev/rules.d/70-sigil-remote-input.rules
sudo tee /etc/udev/rules.d/99-sigil-uinput.rules >/dev/null <<'EOF'
KERNEL=="uinput", SUBSYSTEM=="misc", OWNER="root", GROUP="sigil-uinput", MODE="0660", TAG-="uaccess", RUN+="/usr/bin/setfacl --remove-all $env{DEVNAME}"
EOF
sudo modprobe uinput
sudo udevadm control --reload-rules
sudo udevadm trigger --action=add --subsystem-match=misc --sysname-match=uinput
```

Reboot or fully sign out and back in before continuing so the service receives
the new supplementary group. Then prove the exact device identity and access:

```bash
uinput_gid="$(getent group sigil-uinput | cut -d: -f3)"
test -n "$uinput_gid"
id -G | tr ' ' '\n' | grep -Fx "$uinput_gid"
test ! -L /dev/uinput
test -c /dev/uinput
test -r /dev/uinput && test -w /dev/uinput
test "$(stat -Lc '%u' /dev/uinput)" -eq 0
test "$(stat -Lc '%g' /dev/uinput)" -eq "$uinput_gid"
test "$(stat -Lc '%a' /dev/uinput)" = 660
test "$(stat -Lc '%t:%T' /dev/uinput)" = a:df
getfacl -cp /dev/uinput
```

The last hexadecimal device pair is Linux misc major 10, uinput minor 223.
The early rule removes `uaccess` before the seat ACL pass. Current Bazzite also
ships Sunshine and early-uinput rules that may already have queued the uaccess
builtin, so the final bounded `setfacl` action removes any materialized ACL
after permissions settle. The exact kernel/subsystem match and fixed executable
path keep that action scoped only to `/dev/uinput`.
The separate remote-input rule runs before Bazzite's seat pass. It gives the
active Gamescope session access to Sigil's disjoint keyboard and relative-mouse
event nodes and assigns the same single-purpose group. Without it, correct
`BTN_LEFT` and `REL_X/Y` events can appear in `evtest` while Gamescope silently
ignores an inaccessible hot-plugged device.
The daemon independently opens the configured path three times with
`O_NOFOLLOW`, verifies that character-device identity, exact owner/group/mode,
and absence of an extended access ACL on every descriptor, then creates
separate pointer, keyboard, and gamepad devices before it binds Iroh. Any change
fails closed. Access to
uinput is equivalent to local keyboard, pointer, and gamepad control, so keep
this dedicated group single-purpose. Do not add
`PrivateDevices=true` to the user unit because that would deliberately hide the
validated device.

Set `vaapi_encoder` to one factory whose programmatically queried read-only
`device-path` exactly equals `render_node`. This is deliberate: GstVA selects a
DRM device by factory and exposes `device-path` as read-only, so Sigil verifies
the live element's reported device before starting capture. Set
`rate_control = "cbr"` only when
that factory advertises CBR; otherwise select a low-power factory that
advertises CQP and use `rate_control = "cqp"`. Do not silently substitute a
different factory, render node, or software encoder.

Create a separate probe configuration without changing the running synthetic
service configuration. The CBR example is:

```bash
vaapi_encoder="<factory verified above>"
test -n "$vaapi_encoder"
uinput_gid="$(getent group sigil-uinput | cut -d: -f3)"
test -n "$uinput_gid"
probe_config="$HOME/.config/sigil-spark/host-gamescope-probe.toml"
cat >"$probe_config" <<EOF
identity_path = "$HOME/.local/share/sigil-spark/identity/host.key"
state_path = "$HOME/.local/state/sigil-spark/runtime"
source = "gamescope-pipewire"
framerate = 60
codec = "h264"
input_mode = "uinput"
ffmpeg_path = "$ffmpeg_path"

[uinput]
device_path = "/dev/uinput"
expected_owner_uid = 0
expected_group_gid = $uinput_gid
expected_mode = 0o660

[gamescope_pipewire]
node_name = "gamescope"
media_class = "Video/Source"
xwayland_display = ":0"
pw_dump_path = "$pw_dump_path"
gst_launch_path = "$gst_launch_path"
gst_inspect_path = "$gst_inspect_path"
encoder_backend = "external-gst-launch"
vaapi_encoder = "$vaapi_encoder"
vaapi_render_node = "$render_node"
rate_control = "cbr"
bitrate_kbps = 12000
EOF
chmod 0600 "$probe_config"
```

With `width` and `height` absent, Sigil uses the single bounded native size
advertised by the selected Gamescope node. Set both fields only to request an
explicit same-aspect encoded downscale; partial or distorting overrides are
rejected. `framerate` is a maximum encoder cadence because current Gamescope
PipeWire caps advertise `0/1` rather than a trustworthy native rate.

Omitting `encoder_backend` has the same external default. After the external
baseline passes, a locally built Linux binary containing the feature can use:

```toml
encoder_backend = "in-process-gstreamer"
```

That selection is not an availability hint: a binary without the feature, a
missing `appsink`, or an unavailable encoder factory makes preflight fail. The
foundation currently requires CBR, and a bitrate property that is not mutable
while PLAYING also fails the control preflight. Runtime bitrate control is
acknowledged only after exact property readback. CQP remains on the external
compatibility backend until its in-process force-keyframe path is implemented
and proven separately.

The configured `:0` is a bootstrap connection, not a fixed input target.
Gamescope may move mouse focus between its `:0` and `:1` Xwayland servers.
Sigil reads `GAMESCOPE_MOUSE_FOCUS_DISPLAY` from the bootstrap root, reconnects
to the active local display, and samples `QueryPointer` at no more than 60 Hz.
It also reads `GAMESCOPE_CURSOR_VISIBLE_FEEDBACK` from that active root so the
client overlay disappears when Gamescope hides its cursor. Missing, malformed,
or unreachable Xwayland state during startup disables the separately
negotiated pointer feedback capability. After successful startup, losing
Gamescope publishes pointer feedback as unavailable, then reconnects the
bootstrap display with a bounded 100 ms to 2 second backoff. A complete
reconnect re-interns both Gamescope atoms, discovers the replacement native
surface, and validates one active-display sample before publishing coordinates
again. Relative uinput remains available without guessed cursor coordinates.
The service `DISPLAY=:0` line is retained as an explicit fallback for
configurations created before `xwayland_display` was added.

Audio is optional and must resolve one exact PipeWire sink, never a microphone.
The appliance owns a persistent 48 kHz stereo null sink so capture does not
depend on HDMI, speakers, or a physical sound card. Install the repository's
drop-in as the gaming user, restart only the PulseAudio-compatible PipeWire
service, and make the new sink the default target:

```bash
audio_dropin_dir="$HOME/.config/pipewire/pipewire-pulse.conf.d"
install -d -m 0700 "$audio_dropin_dir"
install -m 0600 scripts/50-sigil-spark-audio.conf \
  "$audio_dropin_dir/50-sigil-spark-audio.conf"
systemctl --user restart pipewire-pulse.service

audio_sink_id="$(wpctl status -n | awk '
  $0 ~ /sigil_spark/ {
    for (field = 1; field <= NF; field++) {
      if ($field ~ /^[0-9]+\.$/) {
        gsub("\\.", "", $field)
        print $field
        exit
      }
    }
  }
')"
test "$audio_sink_id" -gt 0
wpctl set-default "$audio_sink_id"
pactl get-default-sink | grep -qx sigil_spark
```

Restarting `pipewire-pulse.service` disconnects existing playback streams. Do
this during provisioning before launching Steam or a game; new streams then
route to `sigil_spark`. The sink deliberately has no physical playback leg.
Remote audio continues while the machine has no display or speakers, and local
speaker mirroring remains a separate opt-in policy rather than part of the
capture trust boundary.

The drop-in makes only this virtual sink an always-processing graph driver and
disables its idle suspension. Both are required because the host captures its
monitor continuously: an idle null sink otherwise emits no silent clocked
buffers and leaves a newly connected client stuck in audio priming until a
game happens to make sound.

Verify that exactly one persistent sink has the expected stable `node.name`;
do not copy the changing numeric ID or `object.serial` into host configuration:

```bash
pw-dump | jq -r '
  .[]
  | select(.type == "PipeWire:Interface:Node")
  | .info.props
  | select(."media.class" == "Audio/Sink")
  | [."object.serial", ."node.name", ."node.description"]
  | @tsv
'
test "$(pw-dump | jq '[
  .[]
  | select(.type == "PipeWire:Interface:Node")
  | .info.props
  | select(."media.class" == "Audio/Sink" and ."node.name" == "sigil_spark")
] | length')" -eq 1
audio_sink_name="sigil_spark"
cat >>"$probe_config" <<EOF

[audio]
node_name = "$audio_sink_name"
media_class = "Audio/Sink"
bitrate_bps = 96000
EOF
chmod 0600 "$probe_config"
```

This monitor capture is fixed at 48 kHz stereo, 20 ms frames, and 96 kbit/s
restricted-low-delay Opus. It uses a two-packet host queue, QUIC datagrams, a
three-packet client reorder window, a three-message binary Tauri channel, and a
60 ms AudioWorklet ring; overflow drops old audio instead of increasing
latency. Audio negotiation and failure are independent from video and input.
Only audio intentionally routed to the appliance sink is captured. This keeps
desktop notifications or other audio routed to a different device outside the
stream, while Steam and games inherit `sigil_spark` from the default selected
during provisioning.

This backend implements a conventional relative mouse with `REL_X`, `REL_Y`,
three mouse buttons, vertical/horizontal wheel events, and no `ABS_X/Y` axes.
The client coalesces displacement into one bounded slot rather than dropping
stale samples, and the host negotiates `relative_pointer` independently from
keyboard and gamepad control. A third device named `Sigil Spark Virtual
Gamepad` exposes one
Xbox-style controller: ABXY, shoulders, back/start/guide, stick clicks, d-pad,
two sticks, and two analog triggers. Stick values are normalized signed
integers in `-32767..=32767`, triggers are unsigned integers in
`0..=32767`, and d-pad axes are `-1..=1`. Each protocol message is a complete
snapshot and is emitted as one statically bounded uinput report, so stale axis
or button updates cannot grow an independent queue. The host advertises the
separate `gamepad` input capability and rejects gamepad snapshots unless that
capability was negotiated.

It releases every held keyboard/mouse transition and sends a fully neutral
gamepad snapshot when an input session ends. The client currently sends a Text
event in addition to physical printable-key events; the host explicitly treats
Text as a content-free no-op, acknowledges it when ACKs were negotiated, and
does not advertise Text support. No key, text, or gamepad payload is logged.
The Linux button/axis assignments follow the kernel's
[gamepad protocol](https://docs.kernel.org/input/gamepad.html) and Xbox `xpad`
layout.

For a factory that advertises only CQP, replace the last two settings with:

```toml
rate_control = "cqp"
quantizer = 24
```

Keep `encoder_backend = "external-gst-launch"` for CQP. The current
in-process foundation rejects CQP rather than implying runtime control that it
does not implement.

If `pw-dump` exposes an additional stable GPU identity on the Gamescope node,
add it as an exact match; do not add a changing global ID, node ID, client ID,
or `object.serial`:

```toml
[gamescope_pipewire.match_properties]
"device.bus-path" = "<exact stable value observed in pw-dump>"
```

Exact properties and fail-on-ambiguity prevent accidental selection, but they
do not authenticate a visual source against another process running as the
same gaming user: a same-UID process can publish spoofed PipeWire properties
and can already access this user's Sigil configuration. Treat the whole gaming
UID/session as one trust boundary for this demo. Stronger isolation requires a
separate service identity plus Node-to-Client PID correlation and verification
of `/proc/<pid>/exe` and the Gamescope cgroup.

`config check` performs a live, bounded preflight. It requires exactly one
matching PipeWire node, checks every configured executable and required
GStreamer element, opens the configured render node read/write, proves its
exact GstVA factory is bound to that node, and verifies all low-latency
properties used by the pipeline. When audio is configured, it also prints the
exact resolved sink target:

```bash
"$HOME/.local/libexec/sigil-spark/current/sigil" config check \
  --config "$probe_config"
# Expected with audio enabled:
# audio_pipewire_target_object=<current numeric target>
# audio_capture_preflight=ok
```

Node enumeration and preflight are not enough. Consume 300 frames and verify a
decodable H.264 keyframe, the resolved output size, and a changing sequence:

```bash
"$HOME/.local/libexec/sigil-spark/current/sigil" capture probe \
  --source gamescope-pipewire \
  --config "$probe_config" \
  --frames 300 \
  --minimum-fps 55
```

The Gamescope probe acquires the same per-user global lifecycle lock as the
daemon and holds it through encoder teardown. Stop `sigil-host.service` before
running it; a second configured Sigil daemon using the same runtime root or
another capture probe fails before opening PipeWire. The lock cannot exclude
arbitrary third-party consumers, so inventory non-Sigil PipeWire consumers
before a benchmark.
Gamescope may divide vblank delivery across simultaneous consumers, making two
otherwise healthy 60 fps pipelines appear as roughly 30 fps each. A benchmark
with an existing Gamescope consumer is invalid, not evidence that CQP or the
transport is intrinsically half-rate.

The probe prints `resolved_encoded_mode=<width>x<height>@<cap>` before capture
and the dimensions observed on encoded frames afterward. Repeat with
`--expect-size <width>x<height>` when recording a strict benchmark. The
pipeline explicitly negotiates and converts its encoder input to NV12 at that
resolved size and configured cadence ceiling. The capture pipeline bounds the
PipeWire source pool between one and four buffers and then uses an explicit
one-buffer, leaky-downstream queue; encoder-internal buffering is separate and
must be measured on the installed GstVA version. Gamescope's stream is
damage-driven and may emit no buffers while the image is static. The versioned
host/client path therefore keeps media and input connected through arbitrary
frame silence; process exit, stream closure, malformed media, and write failure
remain terminal. H.264 uses no B-frames, and SPS/PPS are repeated with each
IDR. A capture or hardware-encoder failure is an error; there is no automatic
synthetic or software fallback.

The probe reports sustained encoded FPS, frames dropped after encode before the
probe consumer, and maximum post-encode queue age. The current GStreamer stdout
bridge does not preserve PipeWire capture PTS, so these values do not prove raw
capture age, pre-encode drops, or glass-to-glass latency. Preserve GStreamer
buffer metadata through an in-process appsink before making those stronger
claims; until then, correlate the probe with Gamescope/PipeWire statistics and
the external sampling in the evidence section.

### Validate motion-sensitive resolution

This gate applies only to a Linux build with `in-process-gstreamer` and a host
configuration whose `encoder_backend` is `in-process-gstreamer`. The external
`gst-launch` compatibility backend intentionally remains fixed-resolution.

Connect Portal, enable host debug logs, and repeat at least ten cycles of a
high-motion 3D scene followed by a static high-frequency test card. For each
cycle retain the host journal and Portal console. When the resolved native
dimensions admit an exact even three-quarter tier, the host decision record
must show that reduced motion/pressure target followed by the resolved native
recovery target only after at least two seconds of stillness, three fresh clean
feedback windows, and the three-second transition cooldown. A stale feedback
window must never restore native resolution. A valid native mode without such
a tier must remain streamable with motion adaptation visibly disabled.

At each boundary verify that the first delivered target-size frame is a
keyframe with SPS/PPS and a discontinuity, no preceding-resolution delta is
presented afterward, and Portal stays connected without a WebCodecs error.
Absolute pointer tests at the canvas center and four corners must continue to
land on the native Gamescope surface at both encoded resolutions. The Portal
window must not resize merely because the native and reduced modes have the
same aspect ratio.

Record transition request-to-configured-IDR time and Portal
configured-IDR-to-first-presentation time separately. These are recovery
measurements, not glass-to-glass latency. Require ten of ten successful
downscale/restore cycles, recovery p95 no greater than 900 ms and no individual
recovery greater than two seconds, video/decode queue p95 no greater than two,
and no sustained latency growth. Measure true glass-to-glass latency with a
paired high-speed-camera run comparing fixed-native and adaptive sessions under
the same controlled bandwidth pressure; do not substitute post-encode queue
age for that result.

Then perform the actual headless gate:

1. Shut the host down.
2. Disconnect the display; do not use a dummy plug for this test.
3. Cold boot the machine.
4. Make the first SSH connection of that boot.
5. Verify Steam, Gamescope, PipeWire, and the Gamescope stream node.
6. Repeat `config check` and the 300-frame capture probe with `probe_config`.
7. Save the boot and user-session journals.

On the first SSH connection, distinguish Gaming Mode auto-login from an SSH
session that merely started the user manager:

```bash
loginctl list-sessions
sudo journalctl -b -o short-monotonic --no-pager | \
  grep -Ei 'gamescope|steam|sshd.*accepted|session.*sigil'
set -o pipefail
./scripts/bazzite-inventory.sh --cold-boot | \
  tee "$HOME/bazzite-cold-boot.txt"
```

The Gamescope session must predate the first accepted SSH login.
`--cold-boot` is read-only and makes the gate machine-checkable. It exits zero
only when no DRM connector is connected, the SDDM Gaming Mode session and
required PipeWire nodes are active, the Sigil service is enabled and ready,
and Gamescope plus Sigil both predate the first accepted SSH login. Exit status
`1` means observed evidence failed a gate. Exit status `3` means the boot
journal is unavailable, starts more than five minutes after boot, or otherwise
cannot establish the first SSH ordering; do not treat that as a pass. Run it
immediately on the first SSH login so journal rotation cannot erase the proof.

If the stock session does not start or does not publish a node without a
physical connector, stop at this gate. This is an expected discovery risk on
some AMD/KMS combinations, not permission to claim a headless pass. Record the
installed Gamescope version, its `--help` output, connector state, and DRM logs
before designing a custom virtual-output/headless-backend unit. Keep a local
console recovery path. Do not introduce an XDG ScreenCast portal as a
workaround.

Valve documents Gamescope's embedded session model and AMD/Mesa support in the
[Gamescope repository](https://github.com/ValveSoftware/gamescope). Gamescope
logs should identify its PipeWire node when the stream becomes available.

## 12. Cut over from synthetic video to Gamescope

Only after the headless gate passes:

1. Stop `sigil-host.service`.
2. Re-run the attached and headless `config check` and capture probes against
   the exact `host-gamescope-probe.toml` that will be installed.
3. Install that validated file as `host.toml`; do not reconstruct or partially
   merge its strict node, encoder-factory, and render-node selection.
4. Leave width and height absent for native resolution; retain frame rate 60 as
   the explicit encoder ceiling unless a different measured cap is under test.
5. Start the service and confirm the capture queue is bounded to one frame.
6. Connect from the Mac with the Bazzite host's new development node ID.
7. Enable the service only after the attached and headless capture probes pass.
8. Cold boot headlessly again and prove the unit started before the first SSH
   login.

```bash
systemctl --user stop sigil-host.service
install -m 0600 \
  "$HOME/.config/sigil-spark/host-gamescope-probe.toml" \
  "$HOME/.config/sigil-spark/host.toml"
"$HOME/.local/libexec/sigil-spark/current/sigil" config check \
  --config "$HOME/.config/sigil-spark/host.toml"
systemctl --user start sigil-host.service
systemctl --user status sigil-host.service --no-pager
pointer_sysfs="$(grep -lFx 'Sigil Spark Virtual Pointer' /sys/class/input/event*/device/name)"
test -n "$pointer_sysfs"
pointer_node="/dev/input/$(basename "$(dirname "$(dirname "$pointer_sysfs")")")"
udevadm info "$pointer_node" | grep -F 'ID_INPUT_MOUSE=1'
sudo libinput list-devices | sed -n \
  '/Device:[[:space:]]*Sigil Spark Virtual Pointer/,/^$/p'
sudo libinput debug-events --device "$pointer_node"
keyboard_sysfs="$(grep -lFx 'Sigil Spark Virtual Keyboard' /sys/class/input/event*/device/name)"
test -n "$keyboard_sysfs"
keyboard_node="/dev/input/$(basename "$(dirname "$(dirname "$keyboard_sysfs")")")"
udevadm info "$keyboard_node" | grep -F 'ID_INPUT_KEYBOARD=1'
gamepad_sysfs="$(grep -lFx 'Sigil Spark Virtual Gamepad' /sys/class/input/event*/device/name)"
test -n "$gamepad_sysfs"
gamepad_node="/dev/input/$(basename "$(dirname "$(dirname "$gamepad_sysfs")")")"
udevadm info "$gamepad_node" | grep -F 'ID_INPUT_JOYSTICK=1'
systemctl --user enable sigil-host.service
sudo systemctl reboot
```

The pointer and keyboard devices must each be classified only for their own
input role, and the separate gamepad device as a joystick, before the client
control toggle is tested. `libinput debug-events` must report
`POINTER_MOTION` and `POINTER_BUTTON`, never `POINTER_MOTION_ABSOLUTE`. For a
downstream proof, display an isolated Xwayland target in Gamescope and activate
it remotely:

```bash
DISPLAY=:0 xmessage -center -buttons pass:0 'Sigil pointer probe'
```

The command must exit with status 0 after the remote click; `evtest` alone only
proves kernel delivery. During the live demo, prove motion to all four frame
edges, each mouse button, both wheel axes, a modifier chord, ABXY, both
shoulders, back/start/guide, both stick clicks, every d-pad direction, extrema
and center on both sticks, and released/full values on both triggers. End with
a held keyboard key, held gamepad button, displaced stick, and pressed trigger,
then disconnect the client; `evtest` on all three event nodes must show the
pointer button and key released and every gamepad button/axis returned to
neutral.

For a deterministic protocol-to-uinput pointer smoke, leave both non-grabbing
observers running and invoke the probe with `--pointer-smoke`:

```bash
sudo evtest "$pointer_node"
sudo libinput debug-events --device "$pointer_node"

# From the client checkout:
target/debug/sigil-probe \
  --node-id <host-node-id> \
  --frames 120 \
  --pointer-smoke \
  --pointer-feedback-smoke
```

The probe must report `pointer_smoke=ok` and `pointer_feedback_smoke=ok`.
The feedback check requires an immediately available compositor position and
visibility sample; an input acknowledgment with unavailable coordinates fails
the proof. The pointer smoke uses the native pointer-surface
dimensions negotiated in the media `HostHello`, not the potentially downscaled
encoded dimensions. On a 2560x1600 Gamescope surface, before the ordinary
motion, evtest must show the synchronization sequence:
`REL_X=-32767`/`REL_Y=-32767`, a `SYN_REPORT`, then
`REL_X=1280`/`REL_Y=800` and another `SYN_REPORT`. It must then show
`REL_X=32`, `REL_Y=16`, and a complete `BTN_LEFT` press/release, while libinput
reports relative `POINTER_MOTION` and `POINTER_BUTTON` events. The probe fails
closed if the host omits the native pointer-surface dimensions. The interactive
`xmessage` activation remains the downstream Gamescope/Xwayland proof.

For compositor replacement recovery, keep the candidate Sigil daemon running,
record its PID, and restart `gamescope-session-plus@steam.service` twice. After
each restart, wait for the `gamescope` PipeWire node and the bootstrap root
property to return, then rerun the pointer and feedback smoke without replaying
the redeemed invitation. Both runs must pass, the Sigil PID must remain
unchanged, and the host log must contain one `Gamescope Xwayland pointer
feedback reconnected` event per restart. The packaged hardware UAT performs
this sequence after its fixed-mode session checks. That runner independently
enumerates accessible DRM render nodes and matches the exact GstVA H.264
factory by its programmatically queried `device-path` and required CBR/CQP
properties; it
never assumes a kernel driver, `renderD128`, or the generic `vah264enc` factory.
If the panel's resolved native
mode is the same 1280x800 mode as the fixed performance leg, the native-config
leg still runs and records that the two pixel sizes are identical instead of
rejecting the host.

For a deterministic pre-demo gamepad proof, leave `evtest "$gamepad_node"`
running on the host and run this from the client checkout:

```bash
source ~/.cargo/env
cargo build --locked -p sigil-host --bin sigil-probe
target/debug/sigil-probe \
  --node-id <host-node-id> \
  --frames 120 \
  --gamepad-smoke
```

The probe must report `gamepad_smoke=ok`. `evtest` must show `BTN_SOUTH`,
`BTN_TR`, `ABS_X`, `ABS_RY`, both trigger axes, and `ABS_HAT0X` reach their
non-neutral values, followed by an explicit release/zero for every one of
them. This proves protocol negotiation, uinput mapping, and neutralization;
it does not replace the physical-controller client test.

On the first SSH login after reboot:

```bash
systemctl --user status sigil-host.service --no-pager
sudo journalctl -b -o short-monotonic --no-pager | \
  grep -Ei 'sigil|sshd.*accepted|gamescope|steam'
```

The Sigil unit start must predate the first accepted SSH login; otherwise SSH
may have started the user manager and produced a false-positive boot test.

The synthetic source remains a diagnostic mode, not an automatic production
fallback. A Gamescope capture failure must be surfaced as an error.

## 13. Evidence to save for every host image

Create one mode `0700` evidence directory per Bazzite deployment and test date.
Sanitize journals before exporting them. Save:

- Bazzite image and OSTree deployment checksum.
- Kernel, Mesa, Gamescope, PipeWire, FFmpeg, and Rust versions.
- GPU PCI ID, Vulkan device, VA-API profiles, and `/dev/dri` permissions.
- Source commit, Git bundle hash, `Cargo.lock` hash, container/Nix revision,
  binary hash, and configuration hash with private paths redacted.
- Host service journal and Gamescope/PipeWire session journal.
- Direct versus relayed iroh path.
- Frame counts, queue depth, drops, queue age, RSS, CPU, and GPU utilization.
- Reconnect and second-client rejection results.

Never copy the host identity seed into an evidence bundle. Treat node IDs and
connection metadata as operationally sensitive until client authorization
exists.

Until the daemon exports all of those metrics, collect the missing external
samples during a bounded probe instead of leaving evidence fields blank:

```bash
pid="$(systemctl --user show -p MainPID --value sigil-host.service)"
test "$pid" -gt 1
for sample in $(seq 1 30); do
  date --iso-8601=ns
  ps -o pid,rss,%cpu,nlwp,etimes,cmd -p "$pid"
  cat "/proc/$pid/io"
  radeontop -d - -l 1 2>/dev/null || true
  sleep 1
done | tee "$evidence_dir/runtime-samples.txt"
```

## 14. Rollback and recovery

Stop and disable only the Sigil service:

```bash
systemctl --user disable --now sigil-host.service
```

Return to synthetic video by changing `source` back to `test-pattern`. Preserve
the identity unless a new host identity is explicitly required.

Application releases live independently of the OSTree deployment. To roll back
the daemon, select a previously validated release and restart the unit:

```bash
sigil_root="$HOME/.local/libexec/sigil-spark"
previous_commit="<validated-full-commit>"
previous_config="$sigil_root/releases/$previous_commit/host.toml"
cd "$sigil_root"
previous_host="releases/$previous_commit/sigil"
if [[ ! -x "$previous_host" ]]; then
  previous_host="releases/$previous_commit/sigil-host"
fi
test -x "$previous_host"
test -f "$previous_config"
"$previous_host" config check --config "$previous_config"
test ! -e current || test -L current
ln -sfnT "releases/$previous_commit" current
install -m 0600 "$previous_config" "$HOME/.config/sigil-spark/host.toml"
systemctl --user restart sigil-host.service
systemctl --user is-active --quiet sigil-host.service
```

Keep a validated backup of `host.toml` with each application release and run
`sigil config check` before restoring it. An OSTree rollback does not roll
back binaries or configuration in the user's home directory, and it does not
necessarily remove the SSH drop-in under `/etc`.

If a Bazzite update breaks the appliance, inspect deployments and roll back:

```bash
rpm-ostree status -v
sudo rpm-ostree rollback
sudo systemctl reboot
```

The previous deployment is also selectable from the boot menu. Do not reset or
reinstall the machine until its logs and failing deployment ID have been saved.

If SSH hardening itself must be rolled back, do it from the physical console:

```bash
sudo mv /etc/ssh/sshd_config.d/10-sigil.conf \
  /etc/ssh/sshd_config.d/10-sigil.conf.disabled
sudo sshd -t
sudo systemctl reload sshd.service
```

## Completion gates

### Steps 0–2 on `slate`

- Debug and explicitly feature-gated demo-client bypasses work and are visibly
  labeled; ordinary release-client parsing rejects `--dev-connect`.
- Shared protocol tests pass on both architectures.
- Pure host binary has no Tauri dependency.
- Synthetic H.264 streams from `slate` to the Mac.
- The rendered Tauri/WebCodecs path starts on a keyframe and its transport,
  frontend, and decoder drop counters do not grow after startup during the
  demo soak.
- Input transport remains responsive during video load.
- One-client enforcement and 100 reconnect cycles pass.

### Fresh Bazzite host

- Key-only SSH survives reboot.
- The base deployment is pinned and recoverable.
- AMD Vulkan and H.264 VA-API checks pass.
- Sigil builds without layering host development packages.
- The foreground synthetic service passes reconnect tests before Gamescope
  cutover.
- A cold, physically headless boot produces a Gamescope PipeWire node.
- Switching to Gamescope capture does not require a portal or a physical
  display.
