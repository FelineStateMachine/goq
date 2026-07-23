# Frozen compatibility identifiers

goq is the project and public site, Sigil is the Linux host, and Portal is the
client. Some earlier `sigil` and `sigil-spark` values are nevertheless
permanent compatibility identifiers. They are not display branding and must
not be changed as part of a product rename or cleanup.

## Portal enrollment identity

Portal derives one stable Iroh endpoint identity from the FIDO2
`hmac-secret`. Existing Sigil enrollment grants are bound to that endpoint.
Changing any derivation input silently produces a different endpoint and
strands the enrollment.

| Identifier | Frozen value |
| --- | --- |
| FIDO relying-party ID | `sigil` |
| HMAC salt message | `sigil-iroh-identity-v1` |
| Salt SHA-256 | `147ec51fc95a4add3b73f9f366b157fb5dec706a12173151062728dbad1584aa` |
| Resident user ID | `sigil-user` |
| Resident user name | `sigil` |
| Resident user display name | `Sigil` |
| Portal identity domain bytes | `goq/portal-client-identity/v1\0` |

The fixed non-secret test input consisting of 32 `0x2a` bytes derives the
secret-key seed
`78ccce6e04070fff0ac5f74cde6d948ea48042a66f45a8c58a96eb459ca10dd9`
and Iroh endpoint ID
`0383aa3774fe624d7a3bc9189c64770e077c43db7315dde1df19085538adc136`.
Portal has a golden test for the complete derivation. This vector is test data,
not a deployed credential.

## Sigil host filesystem namespace

The following host paths are frozen because the installer, service, rollback
tooling, Decky plugin, and recovery documentation consume them independently:

| Purpose | Frozen path |
| --- | --- |
| Installed releases and current link | `~/.local/libexec/sigil-spark` |
| Host configuration | `~/.config/sigil-spark/host.toml` |
| Identity and package-owned shared data | `~/.local/share/sigil-spark` |
| Authorization and configuration state | `~/.local/state/sigil-spark` |
| Volatile daemon management runtime | `$XDG_RUNTIME_DIR/sigil-spark` |

For an explicit management runtime root, Sigil also appends the frozen child
`sigil-spark`. The daemon lifecycle lock and status document live below that
child. A repository gate keeps the independent Decky, systemd, installer,
activation, staging, and Rust consumers synchronized.

## Linux virtual-input ABI

All three virtual devices use Linux `BUS_VIRTUAL`, vendor `0x5347`, and device
version `1`.

| Device | Product | Name |
| --- | ---: | --- |
| Pointer | `1` | `Sigil Spark Virtual Pointer` |
| Gamepad | `2` | `Sigil Spark Virtual Gamepad` |
| Keyboard | `3` | `Sigil Spark Virtual Keyboard` |

These values are visible to Gamescope, udev, libinput, games, diagnostics, and
hardware evidence. Host tests pin the complete tuple, and a package test
requires the shipped udev rule to match its checked-in frozen source exactly.

## Change policy

Never silently edit a frozen value or accept an alternate value at runtime.
Any intentional successor requires a separately versioned identifier and an
explicit migration design covering existing FIDO credentials, Sigil
enrollments, local-management consumers, udev/libinput rules, and rollback.
The migration must preserve a clear failure mode and must be proven with an
already-enrolled Portal and an upgraded host before release.
