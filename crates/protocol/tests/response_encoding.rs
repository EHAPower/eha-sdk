// Copyright The eha_controller Contributors

use protocol::{
    Direction, Error, Message, MessageKind, Response, SampleData, decode,
    responses::{
        CanProfile, CommunicationSettingsFields, ConfigDataFields, ConfigDataWriter,
        ConfigRecordState, ConfigView, Constraints, ContactState, DataUnavailableFields,
        DesiredAxis, DetailUnit, DiagnosticDomain, DiagnosticEvidence, DiagnosticFields,
        DiagnosticImpact, DiagnosticObject, DiagnosticsFields, DriverState, EvidenceState,
        FallbackReason, IdentityFields, IdentityText, MaintenanceOperation, MaintenanceState,
        MeasurementFields, MeasurementValues, MissingEvidence, ModelState, NativeDomain,
        OperationEvidence, OperationPhase, OperationResultFields, PositiveForceChannel,
        ReferenceKind, RunIdentity, SourceQuality, StartupSource, TargetIngress, TargetMode,
        TelemetryFacts, TelemetryFields, UnavailableSubject, UpdateRoute, ValueFields, ValueResult,
        ValueState, encode_communication_settings, encode_data_unavailable, encode_diagnostics,
        encode_identity, encode_measurements, encode_operation_result, encode_status,
        encode_telemetry,
    },
    validate,
};

fn sample(query_id: u32) -> SampleData {
    SampleData {
        query_id,
        run_nonce: [1; 16],
        snapshot_sequence: 7,
        snapshot_time_us: 100,
    }
}

fn available(value: f32) -> ValueFields {
    ValueFields::new(
        value,
        ValueState::new(ValueResult::Available, SourceQuality::Qualified, false),
    )
}

#[path = "response_encoding/maintenance.rs"]
mod maintenance;
#[path = "response_encoding/observations.rs"]
mod observations;
