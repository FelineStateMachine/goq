# Portal platform and relative-pointer support

Portal's first supported download is macOS arm64. Its relative-pointer path
uses CoreGraphics cursor disassociation and has live Mac-to-Gamescope hardware
evidence. Linux, Windows, and other Portal builds are not release targets yet.
All platform claims also inherit the multi-viewer acceptance boundary: native
MoQ control v2 supports three viewers by default (hard ceiling eight), while
control v1/grouped-v3 remains exclusive. Only one viewer may hold slot-0 input
focus, regardless of client platform.

## Build policy

| Client build | Relative pointer | Other negotiated input |
| --- | --- | --- |
| macOS | Enabled through the native CoreGraphics path | Absolute pointer, keyboard/text, and gamepad remain available when granted |
| Non-macOS, default features | Not offered; any unexpected relative-pointer or pointer-feedback response is masked locally | Absolute pointer, keyboard/text, gamepad, and input acknowledgements remain available when granted |
| Non-macOS with `experimental-non-macos-pointer-capture` | Enabled through Tao cursor grab plus browser Pointer Lock | Same independently granted input capabilities |

The experimental flag makes the non-macOS code available for testing; it does
not make that platform supported and must not appear in a published build.
Compile both policies with:

```bash
cargo check --locked -p portal --all-targets
cargo check --locked -p portal --all-targets \
  --features experimental-non-macos-pointer-capture
```

The complete repository gate runs the default test suite and separately
compiles the experimental branch. On Linux CI this proves both non-macOS
configurations compile. Portal's macOS release job continues to build without
feature flags.

## Non-macOS acceptance checklist

Complete this checklist independently for each OS/webview combination before
proposing default or release support. Preserve the exact Portal commit,
operating-system version, webview version, and window-system/session type with
the evidence.

1. Build and launch Portal without the experimental feature. Connect to a host
   offering every input capability and confirm diagnostics do not report
   relative pointer or pointer-position feedback. Confirm absolute motion and
   buttons, keyboard/text, and a physical gamepad still work when their grants
   are present.
2. Build the exact same commit with
   `experimental-non-macos-pointer-capture`. Confirm the embedded webview
   implements `requestPointerLock`, reports ownership changes, and supplies
   bounded `movementX`/`movementY` deltas.
3. Against the UMPC Gamescope session, enter and exit control at least ten
   times. Exercise motion, left/right click, both scroll axes, keyboard, and a
   physical controller in an actual game. Confirm the remote pointer maps to
   the native host surface rather than the encoded size.
4. While control is active, test window focus loss/regain, Pointer Lock loss,
   disconnect, reconnect, and application exit. Portal must visibly leave
   control when ownership is lost, restore the local cursor, bound release
   retries, and leave no held host input.
5. Repeat under the platform's materially different window systems or
   webviews. Linux requires separate Wayland/WebKitGTK and X11/WebKitGTK
   evidence when both are claimed; Windows requires the shipped WebView2
   runtime. Record unavailable APIs as an explicit unsupported result rather
   than bypassing the gate.
6. Run `./scripts/verify-demo-build.sh` and retain the exact Portal-to-UMPC
   session evidence required by `AGENTS.md`. A config check, unit test, or
   compile-only result is not hardware acceptance.
7. Join the exact candidate as one of three simultaneous control-v2 viewers.
   Verify the roster and local opaque handle, request and transfer slot-0 focus
   by controller, and confirm focus loss immediately tears down local pointer,
   keyboard, and gamepad routing before another viewer injects.
8. While this platform is the holder and while it is a spectator, exercise
   same-peer replacement, live input and view revocation, and one constrained
   viewer. Survivor video/audio and diagnostics must continue on the same media
   generation with bounded queues. Repeat legacy mode separately and confirm a
   second client receives the explicit exclusive-mode rejection.

Removing the feature gate requires reviewed evidence for every platform being
enabled and an explicit release-policy change. Do not infer support from Tao
or webview API availability alone.
