#!/usr/bin/env bash

set -euo pipefail

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_dir="$(CDPATH='' cd -- "$script_dir/../.." && pwd -P)"

require_exact_line() {
  local relative_path="$1"
  local expected="$2"
  if ! grep -Fqx -- "$expected" "$repo_dir/$relative_path"; then
    printf 'compatibility identifier drifted in %s: %s\n' \
      "$relative_path" "$expected" >&2
    exit 1
  fi
}

# Portal enrollment identity. These source assertions complement the Rust
# derivation vector by preventing a second call-site literal from drifting.
require_exact_line src-tauri/src/commands/auth.rs \
  'const FIDO_RP_ID: &str = "sigil";'
require_exact_line src-tauri/src/commands/auth.rs \
  'const FIDO_HMAC_SALT_MESSAGE: &str = "sigil-iroh-identity-v1";'
require_exact_line src-tauri/src/commands/auth.rs \
  'const FIDO_RESIDENT_USER_ID: &[u8] = b"sigil-user";'
require_exact_line src-tauri/src/commands/auth.rs \
  'const PORTAL_CLIENT_IDENTITY_DOMAIN: &[u8] = b"goq/portal-client-identity/v1\0";'

# Canonical host install, configuration, persistent data, and volatile runtime
# namespaces. Decky and the installed service are independent consumers.
require_exact_line crates/sigil-host/src/appliance.rs \
  'const RUNTIME_DIRECTORY_COMPONENT: &str = "sigil-spark";'
require_exact_line decky/py_modules/goq_sigil/runner.py \
  '        self.sigil = self.home / ".local/libexec/sigil-spark/current/sigil"'
require_exact_line decky/py_modules/goq_sigil/runner.py \
  '        self.config = self.home / ".config/sigil-spark/host.toml"'
require_exact_line scripts/install-bazzite-package.sh \
  "install_root=\"\$HOME/.local/libexec/sigil-spark\""
require_exact_line scripts/sigil-host.service \
  'ConditionPathExists=%h/.config/sigil-spark/host.toml'
require_exact_line scripts/sigil-host.service \
  'ExecStart=%h/.local/libexec/sigil-spark/current/sigil-host serve --config %h/.config/sigil-spark/host.toml'
require_exact_line docs/sigil-host-activation.md \
  "identity=\"\$HOME/.local/share/sigil-spark/identity/host.key\""
require_exact_line docs/sigil-host-activation.md \
  "state_path = \"\$HOME/.local/state/sigil-spark/runtime\""
require_exact_line scripts/stage-bazzite-release.sh \
  "    \"\$HOME/.local/share/sigil-spark/package-assets/70-sigil-remote-input.rules\""

# Linux virtual-input producer and the packaged udev consumer must remain
# byte-for-byte synchronized around the names that udev matches.
require_exact_line crates/sigil-host/src/input/mod.rs \
  'const UINPUT_BUS_TYPE: u16 = 0x06; // Linux BUS_VIRTUAL'
require_exact_line crates/sigil-host/src/input/mod.rs \
  'const UINPUT_VENDOR_ID: u16 = 0x5347;'
require_exact_line crates/sigil-host/src/input/mod.rs \
  'const UINPUT_DEVICE_VERSION: u16 = 1;'
require_exact_line crates/sigil-host/src/input/mod.rs \
  'const POINTER_DEVICE_NAME: &[u8] = b"Sigil Spark Virtual Pointer";'
require_exact_line crates/sigil-host/src/input/mod.rs \
  'const KEYBOARD_DEVICE_NAME: &[u8] = b"Sigil Spark Virtual Keyboard";'
require_exact_line crates/sigil-host/src/input/mod.rs \
  'const GAMEPAD_DEVICE_NAME: &[u8] = b"Sigil Spark Virtual Gamepad";'
require_exact_line scripts/70-sigil-remote-input.rules \
  'ACTION=="add|change", SUBSYSTEM=="input", KERNEL=="event*", ATTRS{name}=="Sigil Spark Virtual Pointer", GROUP="sigil-uinput", MODE="0660", TAG+="uaccess"'
require_exact_line scripts/70-sigil-remote-input.rules \
  'ACTION=="add|change", SUBSYSTEM=="input", KERNEL=="event*", ATTRS{name}=="Sigil Spark Virtual Keyboard", GROUP="sigil-uinput", MODE="0660", TAG+="uaccess"'

printf 'compatibility_identifier_tests=ok\n'
