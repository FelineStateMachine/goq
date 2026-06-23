# Keyhome agent guide

This repo is in planning/spec mode until Dami explicitly approves MVP implementation.

## Working rules

- Use OpenSpec for capability planning before coding.
- Treat `openspec/` artifacts as the source of truth for requirements and MVP scope.
- Do not build a browser-only version; the core architecture is native Tauri + Rust backend.
- Keep the first MVP focused on viability evidence, not polish.
- Prefer small, verifiable Rust spikes over broad scaffolding.

## MVP evidence standard

A claim is not accepted unless backed by a runnable spike, a crate/API proof, or a documented blocker with exact error output.

The highest-risk assumptions are:

1. Native YubiKey flows usable from a Tauri backend.
2. Safe storage or derivation of Iroh identity/addressing material from a YubiKey.
3. Iroh session setup and authenticated protocol binding.
4. Remote desktop stream/input latency and OS permissions.

## OpenSpec workflow

- Create/modify proposal, design, tasks, and delta specs under `openspec/changes/<change>/`.
- Validate with `openspec validate --all --strict` before reporting specs as done.
- Do not implement active changes until approved.
