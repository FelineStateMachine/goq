# Tasks

## 1. Research and spike setup

- [x] 1.1 Compare Rust YubiKey/CTAP/PIV crates and document candidate APIs.
- [ ] 1.2 Scaffold minimal Tauri app with backend commands and a diagnostics UI.
- [x] 1.3 Add an evidence log template under `docs/evidence/`.

## 2. Hardware authentication viability

- [x] 2.1 Implement token detection spike.
- [x] 2.2 Implement one verification path requiring touch or PIN.
- [x] 2.3 Prove pairing material can be read, derived, or unlocked after verification.
- [x] 2.4 Document failed token modes with exact errors and OS requirements.

## 3. Iroh viability

- [x] 3.1 Implement native Iroh endpoint creation in the backend.
- [x] 3.2 Implement a minimal host agent with a stable node identity.
- [x] 3.3 Dial the host from authenticated pairing material.
- [x] 3.4 Add a diagnostic request/response protocol over the Iroh channel.

## 4. Remote-control viability

- [ ] 4.1 Stream synthetic or captured frames from host to client.
- [ ] 4.2 Render frames in the Tauri UI.
- [ ] 4.3 Send pointer and keyboard events from client to host.
- [ ] 4.4 Measure rough latency and CPU for the MVP path.

## 5. Review gate

- [x] 5.1 Update OpenSpec tasks and evidence docs with actual results.
- [x] 5.2 Decide whether the MVP should proceed, pivot token mode, or stop.
- [x] 5.3 Validate OpenSpec artifacts with `openspec validate --all --strict`.
