#!/usr/bin/env bash
# shellcheck disable=SC2329

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export SIGIL_INVENTORY_SOURCE_ONLY=1
# shellcheck source=../bazzite-inventory.sh
# shellcheck disable=SC1091
source "$script_dir/../bazzite-inventory.sh"

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

assert_contains() {
  local output="$1"
  local expected="$2"
  grep -qxF "$expected" <<<"$output" || \
    fail "expected output line: $expected"
}

assert_not_contains() {
  local output="$1"
  local unexpected="$2"
  if grep -qF "$unexpected" <<<"$output"; then
    fail "unexpected output text: $unexpected"
  fi
}

session_user_id="$(id -u)"
loginctl() {
  if [[ "$1" == list-sessions ]]; then
    printf '2 %s test seat0 tty1\n' "$session_user_id"
  else
    printf '%s\n' \
      'Id=2' \
      "User=$session_user_id" \
      'Remote=no' \
      'Service=sddm-autologin' \
      'Type=wayland' \
      'Class=user' \
      'State=active'
  fi
}
session_output="$(cold_boot_session_evidence)"
assert_contains "$session_output" 'gaming_autologin_session=ok'
assert_not_contains "$session_output" 'cold_boot_failure='

loginctl() {
  if [[ "$1" == list-sessions ]]; then
    printf '2 65534 somebody seat0 tty1\n'
  else
    printf '%s\n' \
      'Id=2' \
      'User=65534' \
      'Remote=no' \
      'Service=sddm-autologin' \
      'Type=wayland' \
      'Class=user' \
      'State=active'
  fi
}
wrong_user_session_output="$(cold_boot_session_evidence)"
assert_contains "$wrong_user_session_output" \
  'cold_boot_failure=no_active_local_sddm_autologin_session'

pass_journal='[0.500000] host systemd[1]: Reached target Basic System
[1.000000] host gamescope-session-plus[100]: starting Gamescope
[2.000000] host systemd[1000]: Started sigil-host.service - Sigil Spark streaming host
[3.000000] host sigil-host[200]: INFO sigil host ready
[4.000000] host sshd-session[300]: Accepted publickey for tank'
journalctl() {
  printf '%s\n' "$pass_journal"
}
pass_output="$(cold_boot_journal_evidence)"
assert_contains "$pass_output" 'gamescope_before_first_ssh=ok'
assert_contains "$pass_output" 'sigil_unit_before_first_ssh=ok'
assert_contains "$pass_output" 'sigil_ready_before_first_ssh=ok'
assert_not_contains "$pass_output" 'cold_boot_failure='
assert_not_contains "$pass_output" 'cold_boot_evidence_insufficient='

late_journal='[301.000000] host gamescope-session-plus[100]: starting Gamescope
[302.000000] host systemd[1000]: Started sigil-host.service - Sigil Spark streaming host
[303.000000] host sigil-host[200]: INFO sigil host ready
[304.000000] host sshd-session[300]: Accepted publickey for tank'
journalctl() {
  printf '%s\n' "$late_journal"
}
late_output="$(cold_boot_journal_evidence)"
assert_contains "$late_output" \
  'cold_boot_evidence_insufficient=boot_journal_does_not_reach_startup'

bad_order_journal='[0.500000] host systemd[1]: Reached target Basic System
[1.000000] host sshd-session[300]: Accepted publickey for tank
[2.000000] host gamescope-session-plus[100]: starting Gamescope
[3.000000] host systemd[1000]: Started sigil-host.service - Sigil Spark streaming host
[4.000000] host sigil-host[200]: INFO sigil host ready'
journalctl() {
  printf '%s\n' "$bad_order_journal"
}
bad_order_output="$(cold_boot_journal_evidence)"
assert_contains "$bad_order_output" \
  'cold_boot_failure=gamescope_did_not_start_before_first_ssh'
assert_contains "$bad_order_output" \
  'cold_boot_failure=sigil_unit_did_not_start_before_first_ssh'
assert_contains "$bad_order_output" \
  'cold_boot_failure=sigil_was_not_ready_before_first_ssh'

missing_ssh_journal='[0.500000] host systemd[1]: Reached target Basic System
[1.000000] host gamescope-session-plus[100]: starting Gamescope
[2.000000] host systemd[1000]: Started sigil-host.service - Sigil Spark streaming host
[3.000000] host sigil-host[200]: INFO sigil host ready'
journalctl() {
  printf '%s\n' "$missing_ssh_journal"
}
missing_ssh_output="$(cold_boot_journal_evidence)"
assert_contains "$missing_ssh_output" \
  'cold_boot_evidence_insufficient=first_ssh_acceptance_not_observable'

echo 'bazzite_inventory_cold_boot_tests=ok'
