## ADDED Requirements

### Requirement: Evidence-producing MVP workflow
The system SHALL provide a viability workflow that records whether each high-risk integration works under real execution.

#### Scenario: Spike succeeds
- **GIVEN** a viability spike is run
- **WHEN** the spike demonstrates the intended behavior
- **THEN** the result is recorded with platform, command, dependency versions, and observed output.

#### Scenario: Spike fails
- **GIVEN** a viability spike is run
- **WHEN** the spike fails or is blocked
- **THEN** the blocker is recorded with exact error output
- **AND** the next proposed alternative is documented.

### Requirement: Minimal end-to-end remote-control proof
The system SHALL define a minimal end-to-end proof that starts after YubiKey verification and ends with rendered frames plus forwarded input over Iroh.

#### Scenario: End-to-end proof runs
- **GIVEN** a supported YubiKey and paired host agent are available
- **WHEN** the user authenticates and starts the MVP session
- **THEN** the client establishes an authenticated Iroh channel
- **AND** renders host-provided frames
- **AND** forwards at least one pointer or keyboard event to the host.

### Requirement: Viability gate decision
The system SHALL produce a review decision after the MVP spikes complete.

#### Scenario: Review gate is reached
- **GIVEN** hardware auth, Iroh, and remote-control spikes have recorded evidence
- **WHEN** the MVP review is performed
- **THEN** the project records whether to proceed, pivot the token/auth mode, or stop
- **AND** identifies the highest remaining technical risk.
