# Project: Keyhome

## Purpose

Keyhome explores a hardware-key-gated remote-control app for reaching a user's own machine. The intended user flow is: launch native app, plug in YubiKey, tap or enter PIN if required, read/derive the remote identity from the token, connect over Iroh, and control the remote desktop.

## Scope

In scope:

- Tauri client with Rust backend.
- Native YubiKey integration.
- Native Iroh node and authenticated peer protocol.
- Host agent needed for screen capture and input injection.
- MVP viability spikes and measurements.

Out of scope for the initial MVP:

- Browser-only WebAuthn PRF implementation.
- Multi-user SaaS account management.
- Polished installers and auto-update infrastructure.
- Enterprise device management.

## Requirements style

- Specs define observable behavior.
- Change specs use delta headers.
- MVP work must include evidence-producing tasks, not only scaffolding.

## Current priority

Plan and validate the first viability-testing MVP before implementation.
