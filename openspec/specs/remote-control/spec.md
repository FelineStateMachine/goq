# Remote Control

## Purpose

Define the observable behavior for controlling a paired host after Keyhome establishes an authenticated Iroh session.

## Requirements

### Requirement: Host-mediated screen stream
The system SHALL receive desktop frames from a host agent over the authenticated session.

#### Scenario: Stream starts
- **GIVEN** the client has an authenticated session to the host agent
- **WHEN** remote control starts
- **THEN** the host agent sends desktop frames
- **AND** the client renders the stream in the app UI.

### Requirement: Input forwarding
The system SHALL forward keyboard and pointer input from the client UI to the host agent only while a remote-control session is active.

#### Scenario: Pointer input during active session
- **GIVEN** a remote-control session is active
- **WHEN** the user moves or clicks inside the remote-control surface
- **THEN** the client forwards normalized pointer events to the host agent.

#### Scenario: Input after disconnect
- **GIVEN** a remote-control session has ended
- **WHEN** the user types or clicks in the client UI
- **THEN** no remote input events are sent to the host.

### Requirement: Local safety controls
The system SHALL provide visible session state and a local disconnect control during remote control.

#### Scenario: User disconnects
- **GIVEN** a remote-control session is active
- **WHEN** the user activates disconnect
- **THEN** the client terminates the remote-control protocol streams
- **AND** the UI returns to a non-controlling state.
