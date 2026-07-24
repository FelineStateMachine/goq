# Multi-viewer relay design and issue #81 decision

## Decision

Production viewer-to-viewer media routing is **deferred**. The Phase 2 wire shape is adopted now: every control-v2 media generation has a host-certified ephemeral Ed25519 key, every media object is signed over canonical coordinates and a SHA-256 payload digest, and every subscriber receives a short-lived host-signed capability bound to its exact Iroh endpoint. Direct-host v2 media uses this contract immediately, so later relay delivery does not require replacing the media trust model.

Deferral is deliberate rather than an undocumented follow-up. A production relay scheduler, alternate-publisher discovery, automatic failover, and relay UI are not enabled by this phase. Input, focus, authorization mutation, and host administration always terminate at Sigil.

## Spike result

`scripts/relay-spike-proof.sh` models two Portal roles over a bounded virtual five-minute media horizon. Portal A receives each host-authored object and re-serves it through a four-object-capacity relay queue. Portal B verifies the host certificate, final MoQ group/object coordinates, object signature, and payload digest before accepting the existing media envelope. The spike does not sleep, so CPU percentages are normalized against the represented 300 seconds rather than wall-clock runtime.

Release-mode result on the development macOS arm64 machine on 2026-07-23:

| Measurement | Result |
|---|---:|
| Video target | 1280x800 at 60 fps |
| Audio target | Opus, 48 kHz stereo, 50 objects/s |
| Virtual duration | 300 s |
| Signed video objects | 18,000 |
| Signed audio objects | 15,000 |
| Relay queue high-water mark | 1 of 4 |
| Host upload through Portal A | 1,187,934,000 bytes |
| Direct host upload to two viewers | 2,375,868,000 bytes |
| Modeled host-upload saving | 50% |
| Ed25519 signing CPU / virtual realtime | 0.72% of one core |
| Ed25519 verification CPU / virtual realtime | 1.14% of one core |
| Local verification p95 | 160 us |
| Added network latency | Not measured by the virtual spike |

The automated spike proves canonical re-serving, bounded memory, upload accounting, and cryptographic behavior. It is not evidence for Iroh direct-path or relay-fallback network latency. The manual Phase 2 gate must record two physical Portal paths before production mesh routing can be reconsidered.

## Resource threshold and authentication mode

Ed25519 is the only accepted v2 catalog authentication mode. It remains the default while exact-target release-mode evidence stays at or below all of these thresholds:

- Host signing uses at most 5% of one core over the represented 1280x800/60 plus Opus workload.
- Portal verification uses at most 5% of one core over the same workload.
- Local authentication adds at most 1 ms p95 per object before codec validation.
- Signing overhead does not cause a source, MoQ, frontend, audio, or input queue to exceed its existing bound.

A shared-MAC contingency may be evaluated only if an exact Bazzite host or supported Portal target crosses one of those thresholds. It must use a separately negotiated and diagnosed catalog mode. It must never be a silent downgrade, and its security consequence must be explicit: any viewer holding the shared key could forge relayed media. No shared-MAC mode is implemented or accepted now.

## Connectivity model

Sigil and every Portal retain normal Iroh endpoints. Iroh owns endpoint identity, encryption, direct-path discovery, NAT traversal, and relay fallback. A media relay is another authenticated Iroh delivery hop; it does not become Sigil and does not receive input or administration authority.

The future attachment sequence is:

1. Sigil admits Portal B over control v2 and issues a short-lived subscription capability for B's exact Iroh endpoint, media generation, authorization revision, tracks, expiry, nonce, and one relay hop.
2. Sigil's catalog certifies the generation key and media authentication format.
3. Sigil or a selected Portal A advertises an alternate publisher for a bounded group range. A production descriptor must identify the publisher endpoint, generation, track, group range, expiry, and host signature; raw peer identities must not enter the shared roster UI.
4. Portal B connects to Portal A through Iroh and presents the capability. Portal A verifies the host signature and compares the authenticated downstream `remote_id()` with the capability subscriber.
5. Portal B independently verifies the generation certificate and every object. Trust remains rooted in the enrolled Sigil host, not Portal A or the delivery path.

Alternate-publisher descriptors are intentionally not emitted in Phase 2 because no production relay accepts them. The v2 catalog has an explicit version and authentication block, so a later strict extension can add a bounded alternate-publisher list without changing object bytes or treating an unknown mode as valid.

## Trust boundaries

- The persistent Sigil Iroh key signs only the domain-separated generation certificate and subscription capability.
- A fresh ephemeral generation key signs canonical media-object headers. The header binds host-certified generation, track, MoQ group, object index, media flags, payload length, and SHA-256 digest.
- The existing H.264 or Opus envelope remains the signed payload and is validated only after object authentication succeeds.
- Subscription capabilities authorize tracks, not focus or input. They are endpoint-bound, revision-bound, expiring, nonce-bearing, and permit exactly one relay hop.
- Capability tokens, host secrets, generation secrets, peer keys, and object payloads are excluded from diagnostics and logs.

## Failure, failover, and abuse

| Failure or abuse | Required behavior |
|---|---|
| Relay changes payload, coordinates, flags, or signature | Portal rejects the object before codec parsing and increments a local verification-failure counter. |
| Relay withholds a group or stalls an object | Existing bounded MoQ deadlines enter recovery; withheld bytes are never counted as trusted media. |
| Relay disappears | Portal discards relay-owned state and reconnects directly to Sigil using the same host-rooted object trust contract. |
| Wrong or expired subscriber capability | Relay or host rejects attachment before publishing media. |
| Wrong host or generation certificate | Portal rejects the catalog and terminates v2 media setup without falling back to unauthenticated v2 objects. |
| Replay from another track/group/object | Canonical coordinate comparison rejects it even when the payload and signature were once valid. |
| Slow or malicious relay | Its bounded queue may drop or disconnect that path; it cannot increase Sigil's producer history or another viewer's queue. |
| Alternate publisher lies about availability | Timeout leads to direct-host recovery; advertisements never make absent media trusted. |

Direct fallback is a transport fallback, not an authentication fallback. An unknown catalog mode, absent v2 certificate, malformed capability, or failed object signature is terminal for that path.

## Adoption triggers

Re-open production mesh routing only when all of these are available:

- Exact-target measurements show direct Sigil upload is sustained above 70% of available uplink with at least three viewers, or product UAT establishes a lower measured saturation threshold.
- Two physical Portals prove direct Iroh and relay-fallback paths, one-hop capability presentation, relay loss, and direct-host recovery with added end-to-end p95 below one 60 Hz frame period.
- The relay queue remains bounded under a slow downstream and cannot retain a playable history.
- Abuse tests cover tampering, withholding, wrong host/subscriber/generation/revision, expiry, replay, and relay churn.
- Portal UX can truthfully disclose `direct_host` versus `viewer_relay` without exposing endpoint keys or topology details.

Until those triggers pass, the adopted authentication format keeps fan-out relay-ready while all production viewers subscribe directly to Sigil.

## Reproduction

```bash
./scripts/relay-spike-proof.sh \
  --video 1280x800@60 \
  --audio opus-48k-stereo \
  --duration-seconds 300
```

The command must report Ed25519 mode, 50% modeled two-viewer host-upload savings, bounded relay queue depth, tamper/wrong-subscriber/expiry rejection, zero trusted withheld media, and successful direct-host verification after simulated relay loss.
