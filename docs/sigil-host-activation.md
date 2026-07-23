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
  if test ! -f "$rule" || test -L "$rule"; then
    echo "unsafe or missing package rule: $rule" >&2
    exit 1
  fi
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
if test ! -e "$identity" && test ! -L "$identity"; then
  "$sigil" identity init --output "$identity"
fi
if test ! -f "$identity" || test -L "$identity"; then
  echo "unsafe host identity: $identity" >&2
  exit 1
fi
chmod 0600 "$identity"
"$sigil" identity show --identity "$identity"
```

Never copy this seed to a client or rotate it during an upgrade. Portal pairs
to the host identity represented by this file.

## 3. Select the exact VA-API render node and GstVA H.264 factory

Sigil binds the encoder factory to its programmatically queried read-only
`device-path`; a
factory name alone is not enough on a multi-GPU machine. The following chooses
automatically only when there is one usable capability-backed node/factory
pair, independent of its kernel driver. Set
`RENDER_NODE` and `VA_ENCODER` explicitly before running it when the printed
inventory is intentionally ambiguous.

```bash
set -euo pipefail

gst_inspect_path="$(command -v gst-inspect-1.0)"
timeout_path="$(command -v timeout)"
inspect_max_bytes=1048576
inspection_dir="$(mktemp -d "${TMPDIR:-/tmp}/sigil-activation-inspect.XXXXXX")"
chmod 0700 "$inspection_dir"
trap 'rm -rf -- "$inspection_dir"' EXIT INT TERM HUP

bounded_gst_inspect() {
  local output_path
  local output_size
  output_path="$(mktemp "$inspection_dir/gst-inspect.XXXXXX")"
  if ! "$timeout_path" --signal=TERM --kill-after=2s 5s \
    "$gst_inspect_path" "$@" 2>/dev/null \
    | head -c "$((inspect_max_bytes + 1))" >"$output_path"
  then
    rm -f -- "$output_path"
    return 1
  fi
  output_size="$(wc -c <"$output_path" | tr -d '[:space:]')"
  if test "$output_size" -gt "$inspect_max_bytes"; then
    rm -f -- "$output_path"
    return 1
  fi
  cat "$output_path"
  rm -f -- "$output_path"
}

bounded_encoder_preflight() {
  local node="$1"
  local factory="$2"
  local output_path
  local output_size
  output_path="$(mktemp "$inspection_dir/encoder-preflight.XXXXXX")"
  if ! "$timeout_path" --signal=TERM --kill-after=2s 5s \
    "$sigil" capture encoder-preflight \
      --vaapi-encoder "$factory" --vaapi-render-node "$node" \
      --rate-control cbr --rate-control cqp 2>&1 \
    | head -c "$((inspect_max_bytes + 1))" >"$output_path"
  then
    rm -f -- "$output_path"
    return 1
  fi
  output_size="$(wc -c <"$output_path" | tr -d '[:space:]')"
  rm -f -- "$output_path"
  test "$output_size" -le "$inspect_max_bytes"
}

mapfile -t render_nodes < <(
  for node in /dev/dri/renderD*; do
    test -c "$node" && test -r "$node" && test -w "$node" && printf '%s\n' "$node"
  done
)
printf 'accessible render node: %s\n' "${render_nodes[@]}"

registry="$(bounded_gst_inspect)"
mapfile -t va_factories < <(
  awk '$2 ~ /^va(renderD[0-9]+)?h264(lp)?enc:$/ {
    sub(/:$/, "", $2); print $2
  }' <<<"$registry" | sort -u
)
eligible_pairs=()
for node in "${render_nodes[@]}"; do
  for factory in "${va_factories[@]}"; do
    bounded_encoder_preflight "$node" "$factory" || continue
    eligible_pairs+=("$node $factory")
  done
done
printf 'eligible CBR GstVA pair: %s\n' "${eligible_pairs[@]}"
if test -n "${RENDER_NODE:-}" || test -n "${VA_ENCODER:-}"; then
  test -n "${RENDER_NODE:-}" && test -n "${VA_ENCODER:-}"
  selected_pair="$RENDER_NODE $VA_ENCODER"
  printf '%s\n' "${eligible_pairs[@]}" | grep -Fx "$selected_pair"
else
  test "${#eligible_pairs[@]}" -eq 1
  selected_pair="${eligible_pairs[0]}"
fi
read -r render_node va_encoder <<<"$selected_pair"

printf 'export SIGIL_RENDER_NODE=%q\n' "$render_node"
printf 'export SIGIL_VA_ENCODER=%q\n' "$va_encoder"

rm -rf -- "$inspection_dir"
trap - EXIT INT TERM HUP
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
set +e
set -o noclobber
exec 3>"$config"
create_status=$?
set +o noclobber
set -e
if test "$create_status" -ne 0; then
  echo "refusing to replace existing host configuration: $config" >&2
  exit 1
fi
cat >&3 <<EOF
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
exec 3>&-
test "$(stat -Lc '%a:%U' "$config")" = "600:$(id -un)"

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
