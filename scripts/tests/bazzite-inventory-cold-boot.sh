#!/usr/bin/env bash
# The fixture overrides commands that are reached indirectly through sourced
# inventory functions. ShellCheck reports that pattern as SC2317 before 0.10
# and SC2329 in newer releases.
# shellcheck disable=SC2317,SC2329

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
session_fixture_user="$session_user_id"
session_fixture_remote=no
session_fixture_state=active
session_fixture_services='Service=sddm-autologin'
loginctl() {
  if [[ "$1" == list-sessions ]]; then
    printf '2 %s test seat0 tty1\n' "$session_user_id"
  else
    printf '%s\n' \
      'Id=2' \
      "User=$session_fixture_user" \
      "Remote=$session_fixture_remote"
    [[ -z "$session_fixture_services" ]] || \
      printf '%s\n' "$session_fixture_services"
    printf '%s\n' \
      'Type=wayland' \
      'Class=user' \
      "State=$session_fixture_state"
  fi
}

assert_session_fixture() {
  local label="$1"
  local expected="$2"
  local output

  output="$(cold_boot_session_evidence)"
  if [[ "$expected" == pass ]]; then
    assert_contains "$output" 'gaming_autologin_session=ok'
    assert_not_contains "$output" 'cold_boot_failure='
  else
    assert_contains "$output" \
      'cold_boot_failure=no_active_local_gaming_autologin_session'
    assert_not_contains "$output" 'gaming_autologin_session=ok'
  fi
  printf 'session_fixture=%s result=%s\n' "$label" "$expected"
}

assert_session_fixture sddm pass
session_fixture_services='Service=gdm-autologin'
assert_session_fixture gdm pass
session_fixture_user=65534
assert_session_fixture wrong-user fail
session_fixture_user="$session_user_id"
session_fixture_remote=yes
assert_session_fixture remote fail
session_fixture_remote=no
session_fixture_state=closing
assert_session_fixture inactive fail
session_fixture_state=active
session_fixture_services='Service=gdm-password'
assert_session_fixture manual-gdm fail
session_fixture_services=''
assert_session_fixture missing-service fail
session_fixture_services='Service='
assert_session_fixture empty-service fail
session_fixture_services='Service=sddm-autologin-extra'
assert_session_fixture near-miss-service fail
session_fixture_services='Service=-autologin'
assert_session_fixture empty-service-prefix fail
session_fixture_services=$'Service=sddm-autologin\nService=gdm-autologin'
assert_session_fixture duplicate-service fail

loginctl() {
  if [[ "$1" == list-sessions ]]; then
    printf '%s\n' \
      "2 $session_user_id test - pts/0" \
      "3 $session_user_id test - -" \
      "4 $session_user_id test seat0 tty1"
    return
  fi

  case "$2" in
    2)
      printf '%s\n' \
        'Id=2' "User=$session_user_id" 'Remote=yes' 'Service=sshd' \
        'Type=tty' 'Class=user' 'State=active'
      ;;
    3)
      printf '%s\n' \
        'Id=3' "User=$session_user_id" 'Remote=no' 'Service=systemd-user' \
        'Type=unspecified' 'Class=manager' 'State=active'
      ;;
    4)
      printf '%s\n' \
        'Id=4' "User=$session_user_id" 'Remote=no' 'Service=gdm-autologin' \
        'Type=wayland' 'Class=user' 'State=active'
      ;;
    *)
      return 1
      ;;
  esac
}
multiple_session_output="$(cold_boot_session_evidence)"
assert_contains "$multiple_session_output" 'gaming_autologin_session=ok'
assert_not_contains "$multiple_session_output" 'cold_boot_failure='

pass_journal='[0.500000] host systemd[1]: Reached target Basic System
[1.000000] host gamescope-session-plus[100]: starting Gamescope
[2.000000] host systemd[1000]: Started sigil-host.service - Sigil streaming host
[3.000000] host sigil[200]: INFO sigil host ready
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
[302.000000] host systemd[1000]: Started sigil-host.service - Sigil streaming host
[303.000000] host sigil[200]: INFO sigil host ready
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
[3.000000] host systemd[1000]: Started sigil-host.service - Sigil streaming host
[4.000000] host sigil[200]: INFO sigil host ready'
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
[2.000000] host systemd[1000]: Started sigil-host.service - Sigil streaming host
[3.000000] host sigil[200]: INFO sigil host ready'
journalctl() {
  printf '%s\n' "$missing_ssh_journal"
}
missing_ssh_output="$(cold_boot_journal_evidence)"
assert_contains "$missing_ssh_output" \
  'cold_boot_evidence_insufficient=first_ssh_acceptance_not_observable'

echo 'bazzite_inventory_cold_boot_tests=ok'
