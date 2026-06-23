# Iroh Session

## Purpose

Define Keyhome's peer-to-peer session establishment over native Iroh after hardware-backed local authentication.

## Requirements

### Requirement: Native Iroh endpoint
The system SHALL run a native Iroh endpoint in the Rust backend rather than routing the MVP session through browser networking APIs.

#### Scenario: Client starts networking
- **GIVEN** hardware authentication has succeeded
- **WHEN** the backend prepares a connection
- **THEN** it initializes a native Iroh endpoint
- **AND** exposes connection progress to the UI.

### Requirement: Authenticated host dialing
The system SHALL dial only the host identity obtained from the authenticated pairing material.

#### Scenario: Paired host is dialed
- **GIVEN** authenticated pairing material contains a remote host identifier
- **WHEN** the user connects
- **THEN** the backend dials that host over Iroh
- **AND** binds the session to the authenticated client identity.

#### Scenario: Host identity mismatch
- **GIVEN** the backend receives a response from a peer whose identity does not match the pairing material
- **WHEN** session establishment is evaluated
- **THEN** the backend rejects the session
- **AND** the UI reports an authentication or pairing mismatch.

### Requirement: Protocol separation
The system SHALL use an application protocol that separates authentication, control input, video frames, and diagnostics well enough to test each path independently.

#### Scenario: Diagnostics without remote control
- **GIVEN** the client has authenticated to the host
- **WHEN** the user runs a connectivity diagnostic
- **THEN** the system can verify Iroh reachability and protocol negotiation without starting desktop streaming.
