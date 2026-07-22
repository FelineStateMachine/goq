# Hardware UAT: `7920c5d21434`

- Exact commit: `7920c5d214344f859e0cd5987433a3b303eb85f3`
- GitHub Actions run: [29919920109](https://github.com/FelineStateMachine/goq/actions/runs/29919920109)
- Host: GPD Pocket 4, Bazzite 43, AMD VA-API H.264, Gamescope PipeWire
- Candidate: unsigned development build with `in-process-gstreamer`
- Result: pass

The fixed performance contract sustained 58.701 fps at 1280x800 with zero
post-encode drops. Dynamic native capture resolved to 2560x1600 and sustained
29.759 fps in the final run, also with zero post-encode drops. Native cadence is
therefore below the 55 fps fixed-mode target and remains explicit follow-up
work; native resolution compatibility itself passed.

Forty authenticated sessions completed across the two daemon invocations and
two transports. Every group contained ten unique daemon-local sessions with
zero sequence gaps. Forced-IDR recovery p95 was 34.247 ms for fixed Iroh/MoQ,
70.724 ms for fixed grouped-v3, 63.983 ms for native Iroh/MoQ, and 66.295 ms for
native grouped-v3. The one-time invitation enrolled the persistent probe
identity, replay was rejected, and input liveness was acknowledged in every
session.

The runner restored and independently verified the original service, host
configuration, identity, and installed release. The committed probe evidence is
sanitized; raw host logs and identities remain private on the test host.
