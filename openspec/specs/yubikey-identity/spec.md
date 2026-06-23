# YubiKey Identity

## Purpose

Define how Keyhome uses a YubiKey as the local hardware authority for unlocking remote-control access and retrieving or deriving connection identity material.

## Requirements

### Requirement: Hardware presence gate
The system SHALL require a supported YubiKey to be present before initiating a protected remote-control session.

#### Scenario: No token is present
- **GIVEN** the Keyhome client is open
- **AND** no supported YubiKey is available to the backend
- **WHEN** the user attempts to connect
- **THEN** the client reports that hardware authentication is required
- **AND** no Iroh session is dialed.

#### Scenario: Token is inserted
- **GIVEN** the Keyhome client is waiting for hardware authentication
- **WHEN** a supported YubiKey becomes available
- **THEN** the backend detects the token
- **AND** the UI can prompt the user for the next required action.

### Requirement: User-presence or PIN verification
The system SHALL require the YubiKey flow's configured verification step before releasing any session-unlocking result.

#### Scenario: Touch is required
- **GIVEN** the selected YubiKey mode requires touch
- **WHEN** the backend starts authentication
- **THEN** the user is prompted to touch the token
- **AND** the session-unlocking result is unavailable until the touch succeeds.

#### Scenario: PIN is required
- **GIVEN** the selected YubiKey mode requires a PIN
- **WHEN** the backend starts authentication
- **THEN** the UI requests the PIN through a local prompt
- **AND** the backend does not persist the PIN after the attempt.

### Requirement: Token-bound connection material
The system SHALL obtain the remote peer addressing and client authentication material from token-bound data or from data decryptable only after successful token verification.

#### Scenario: Paired machine data exists
- **GIVEN** the YubiKey contains or unlocks pairing data for a host machine
- **WHEN** hardware authentication succeeds
- **THEN** the backend obtains the remote Iroh node identifier or equivalent dial material
- **AND** obtains client-side session identity material needed to authenticate to that host.

#### Scenario: Pairing data is missing
- **GIVEN** the YubiKey does not contain or unlock host pairing data
- **WHEN** hardware authentication succeeds
- **THEN** the client reports that setup is required
- **AND** no remote-control session is attempted.
