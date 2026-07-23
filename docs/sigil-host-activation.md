# Activate an installed Sigil host

This is the portable post-install path for a dedicated Bazzite or SteamOS-like
AMD gaming host. Run it as the gaming user that owns the Gamescope session, not
as root. The commands use only `$HOME` and `$USER`; no hostname or account name
is assumed.

The package installer deliberately stops before these steps. Identity creation,
system input permissions, PipeWire restart, hardware configuration, and service
enablement each remain visible operator decisions. Re-running a package install
or rollback preserves the identity and `host.toml` created here.

## 1. Grant the gaming user access to only uinput

The package stages its virtual-input udev rule but does not write `/etc` or
change group membership. Install that exact package asset together with the two
rules that bound `/dev/uinput` to a dedicated group:

```bash
set -euo pipefail

current="$HOME/.local/libexec/sigil-spark/current"
remote_input_rule="$current/assets/70-sigil-remote-input.rules"
early_uinput_rule="$current/assets/72-sigil-uinput.rules"
final_uinput_rule="$current/assets/99-sigil-uinput.rules"
test -x "$current/sigil"
for rule in "$remote_input_rule" "$early_uinput_rule" "$final_uinput_rule"; do
  test -f "$rule" && test ! -L "$rule"
done

getent group sigil-uinput >/dev/null || sudo groupadd --system sigil-uinput
sudo usermod --append --groups sigil-uinput "$USER"
sudo install -o root -g root -m 0644 \
  "$remote_input_rule" /etc/udev/rules.d/70-sigil-remote-input.rules
sudo install -o root -g root -m 0644 \
  "$early_uinput_rule" /etc/udev/rules.d/72-sigil-uinput.rules
sudo install -o root -g root -m 0644 \
  "$final_uinput_rule" /etc/udev/rules.d/99-sigil-uinput.rules
sudo modprobe uinput
sudo udevadm control --reload-rules
sudo udevadm trigger --action=add --subsystem-match=misc --sysname-match=uinput
```

Reboot, or fully sign out and back in, before continuing. A new login is
required for the user service to inherit the `sigil-uinput` group.

After that login, fail closed unless the device has the expected Linux uinput
identity, ownership, mode, and no extended ACL:

```bash
set -euo pipefail

uinput_gid="$(getent group sigil-uinput | cut -d: -f3)"
test -n "$uinput_gid"
id -G | tr ' ' '\n' | grep -Fx "$uinput_gid"
test ! -L /dev/uinput
test -c /dev/uinput && test -r /dev/uinput && test -w /dev/uinput
test "$(stat -Lc '%u:%g:%a:%t:%T' /dev/uinput)" = "0:$uinput_gid:660:a:df"
if getfacl -cp /dev/uinput | grep -Eq '^(user|group):[^:]|^mask:'; then
  echo '/dev/uinput has an extended access ACL' >&2
  exit 1
fi
```

Do not add this account to the broad `input` group and do not run Sigil as
root. Access to uinput is equivalent to local controller, keyboard, and pointer
control.

## 2. Create the host identity once

```bash
set -euo pipefail
umask 077

sigil="$HOME/.local/libexec/sigil-spark/current/sigil"
identity="$HOME/.local/share/sigil-spark/identity/host.key"
install -d -m 0700 "$(dirname "$identity")" \
  "$HOME/.config/sigil-spark" "$HOME/.local/state/sigil-spark"
if ! test -e "$identity"; then
  "$sigil" identity init --output "$identity"
fi
test -f "$identity" && test ! -L "$identity"
chmod 0600 "$identity"
"$sigil" identity show --identity "$identity"
```

Never copy this seed to a client or rotate it during an upgrade. Portal pairs
to the host identity represented by this file.

## 3. Select the exact AMD render node and GstVA H.264 factory

Sigil binds the encoder factory to its inspected read-only `device-path`; a
factory name alone is not enough on a multi-GPU machine. The following chooses
automatically only when there is one usable AMD node/factory pair. Set
`RENDER_NODE` and `VA_ENCODER` explicitly before running it when the printed
inventory is intentionally ambiguous.

```bash
set -euo pipefail

mapfile -t amd_nodes < <(
  for sysfs_node in /sys/class/drm/renderD*; do
    test -e "$sysfs_node/device/driver" || continue
    test "$(basename "$(readlink -f "$sysfs_node/device/driver")")" = amdgpu \
      || continue
    node="/dev/dri/$(basename "$sysfs_node")"
    test -c "$node" && test -r "$node" && test -w "$node" && printf '%s\n' "$node"
  done
)
printf 'AMD render node: %s\n' "${amd_nodes[@]}"
if test -n "${RENDER_NODE:-}"; then
  render_node="$RENDER_NODE"
else
  test "${#amd_nodes[@]}" -eq 1
  render_node="${amd_nodes[0]}"
fi
printf '%s\n' "${amd_nodes[@]}" | grep -Fx "$render_node"

mapfile -t va_factories < <(
  gst-inspect-1.0 | awk '$2 ~ /^va(renderD[0-9]+)?h264(lp)?enc:$/ {
    sub(/:$/, "", $2); print $2
  }' | sort -u
)
matching_factories=()
for factory in "${va_factories[@]}"; do
  inspection="$(gst-inspect-1.0 "$factory")" || continue
  grep -Fq "Default: \"$render_node\"" <<<"$inspection" || continue
  grep -Eq '\([0-9]+\): cbr([[:space:]]|$)' <<<"$inspection" || continue
  matching_factories+=("$factory")
done
printf 'matching CBR GstVA factory: %s\n' "${matching_factories[@]}"
if test -n "${VA_ENCODER:-}"; then
  va_encoder="$VA_ENCODER"
else
  test "${#matching_factories[@]}" -eq 1
  va_encoder="${matching_factories[0]}"
fi
printf '%s\n' "${matching_factories[@]}" | grep -Fx "$va_encoder"

printf 'export SIGIL_RENDER_NODE=%q\n' "$render_node"
printf 'export SIGIL_VA_ENCODER=%q\n' "$va_encoder"
```

Keep the two printed export commands for the next step. If automatic selection
fails, inspect every printed node and factory and rerun with an exact pair, for
example `RENDER_NODE=/dev/dri/renderD129 VA_ENCODER=varenderD129h264enc`.
Never substitute a software encoder just to make preflight pass.

## 4. Restart the package-owned audio sink

The package already linked its PipeWire drop-in into the gaming user's config.
Restart only the PulseAudio-compatible PipeWire service during provisioning,
then select the persistent headless sink:

```bash
set -euo pipefail

test -f "$HOME/.config/pipewire/pipewire-pulse.conf.d/50-sigil-spark-audio.conf"
systemctl --user restart pipewire-pulse.service
audio_sink_id="$(wpctl status -n | awk '
  $0 ~ /sigil_spark/ {
    for (field = 1; field <= NF; field++) {
      if ($field ~ /^[0-9]+\.$/) {
        gsub("\\.", "", $field); print $field; exit
      }
    }
  }
')"
test "$audio_sink_id" -gt 0
wpctl set-default "$audio_sink_id"
pactl get-default-sink | grep -Fx sigil_spark
```

This restart can interrupt existing local playback. Perform it before starting
a game.

## 5. Write and validate the hardware configuration

First run the two `export` commands printed by step 3 in this shell. Width and
height are intentionally omitted so Sigil follows the bounded native Gamescope
mode instead of assuming one panel resolution.

```bash
set -euo pipefail
: "${SIGIL_RENDER_NODE:?run the render-node exports from step 3}"
: "${SIGIL_VA_ENCODER:?run the encoder exports from step 3}"

sigil="$HOME/.local/libexec/sigil-spark/current/sigil"
uinput_gid="$(getent group sigil-uinput | cut -d: -f3)"
config="$HOME/.config/sigil-spark/host.toml"
ffmpeg_path="$(command -v ffmpeg)"
pw_dump_path="$(command -v pw-dump)"
gst_launch_path="$(command -v gst-launch-1.0)"
gst_inspect_path="$(command -v gst-inspect-1.0)"
test -n "$uinput_gid"

umask 077
if test -e "$config" || test -L "$config"; then
  echo "refusing to replace existing host configuration: $config" >&2
  exit 1
fi
cat >"$config" <<EOF
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
vaapi_encoder = "$SIGIL_VA_ENCODER"
vaapi_render_node = "$SIGIL_RENDER_NODE"
rate_control = "cbr"
bitrate_kbps = 12000

[audio]
node_name = "sigil_spark"
media_class = "Audio/Sink"
bitrate_bps = 96000
EOF
chmod 0600 "$config"

"$sigil" config check --config "$config"
"$sigil" capture probe \
  --source gamescope-pipewire --config "$config" --frames 300
```

Both commands must exit zero. The capture probe must report 300 encoded frames,
zero post-encode drops, and the actual resolved dimensions. Do not enable the
service after a failed preflight or by weakening the device/factory checks.

## 6. Start, inspect, and only then enable Sigil

```bash
set -euo pipefail

systemctl --user daemon-reload
systemctl --user start sigil-host.service
systemctl --user is-active --quiet sigil-host.service
invocation="$(systemctl --user show -p InvocationID --value sigil-host.service)"
test -n "$invocation"
ready=false
for _attempt in $(seq 1 60); do
  if journalctl --user _SYSTEMD_INVOCATION_ID="$invocation" \
    _SYSTEMD_USER_UNIT=sigil-host.service --no-pager -o cat \
    | grep -Fxq status=ready
  then
    ready=true
    break
  fi
  sleep 1
done
test "$ready" = true
systemctl --user status sigil-host.service --no-pager
journalctl --user _SYSTEMD_INVOCATION_ID="$invocation" --no-pager -o cat

sigil="$HOME/.local/libexec/sigil-spark/current/sigil"
"$sigil" appliance status \
  --config "$HOME/.config/sigil-spark/host.toml" --json

systemctl --user enable sigil-host.service
```

The exact service invocation must report `status=ready` before enablement. The
unit runs in the gaming user's ordinary user manager and retries safely if its
configured Gamescope or PipeWire source is not ready yet; no root daemon or
interactive desktop capture portal is involved.

Create a one-time, short-lived invitation only after Portal shows its peer ID:

```bash
"$HOME/.local/libexec/sigil-spark/current/sigil" invitation create \
  --config "$HOME/.config/sigil-spark/host.toml" \
  --peer '<PORTAL_IROH_PEER_ID>' \
  --expires-in-seconds 900 --pointer-keyboard --gamepad \
  --output "$HOME/sigil.goq-invite"
```

Move that owner-only file to Portal and confirm the displayed host and grants.
After the one-time enrollment, ordinary startup remains **PIN -> tap -> play**.
