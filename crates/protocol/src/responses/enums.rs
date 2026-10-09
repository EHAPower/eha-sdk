// Copyright The eha-sdk Contributors

use super::*;

macro_rules! decode_enum {
    ($name:ident, $($variant:ident),+ $(,)?) => {
        impl $name {
            /// 从已校验的线上判别值构造枚举；未定义值返回 `None`。
            pub const fn from_u8(value: u8) -> Option<Self> {
                match value {
                    $(candidate if candidate == Self::$variant as u8 => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

decode_enum!(TargetMode, None, Position, Velocity, Force, Impedance);
decode_enum!(TargetIngress, None, Can, Usb);
decode_enum!(ReferenceKind, None, PositionMm, VelocityMmPerSecond, ForceN);
decode_enum!(DesiredAxis, None, Idle, ClosedLoop);
decode_enum!(ContactState, Never, Active, Expired);
decode_enum!(StartupSource, Unavailable, User, Factory);
decode_enum!(
    FallbackReason,
    None,
    UserEmpty,
    ReadFailed,
    ParseFailed,
    FormatUnsupported,
    NumericRepresentationFailed,
    SourceUnavailable
);
decode_enum!(RunIdentity, Unavailable, Random);
decode_enum!(UpdateRoute, Absent, Present, Unverifiable);
decode_enum!(
    CanProfile,
    Classical500k,
    Classical1m,
    Fd500k2m,
    Fd500k500k,
    Fd1m2m,
    Fd1m5m,
    Fd1m8m
);
decode_enum!(EvidenceState, Unavailable, Available, Failed);
decode_enum!(PositiveForceChannel, Unavailable, ElectricalA, ElectricalB);
decode_enum!(ModelState, Unavailable, Available, NotApplicable);
decode_enum!(
    DiagnosticDomain,
    Control,
    Can,
    Usb,
    Driver,
    Position,
    Pressure,
    ConfigStore,
    Platform,
    Maintenance
);
decode_enum!(
    DiagnosticObject,
    Overall,
    Position,
    Velocity,
    PressureA,
    PressureB,
    Force,
    MotorOutput,
    DriverAxis,
    UserRecord,
    Application,
    UpdateRoute,
    Can,
    Usb
);
decode_enum!(
    OperationPhase,
    None,
    ConditionCheck,
    WaitingEntry,
    SendCalled,
    WaitingFeedback,
    WaitingExit,
    FormatParse,
    StorageReplacement,
    ActualReadback,
    ResultDelivery,
    ApplicationHandoff,
    ResourceRecovery
);
decode_enum!(
    NativeDomain,
    None,
    OdriveAxisError,
    OdriveAxisState,
    Can,
    Usb,
    RecordParse,
    Flash,
    OdriveSensorlessError,
    OdriveMotorError
);
decode_enum!(
    DetailUnit,
    None,
    Mm,
    MmPerSecond,
    Mpa,
    N,
    Rpm,
    TurnsPerSecond,
    Bytes
);
decode_enum!(ConfigView, Factory, UserRecord, Startup, Communication);
decode_enum!(
    ConfigRecordState,
    Complete,
    Empty,
    Incomplete,
    FormatUnsupported,
    NumericRepresentationFailed,
    ReadFailed,
    ViewUnavailable
);
decode_enum!(
    MaintenanceOperation,
    SaveConfig,
    RestoreFactory,
    ResetApplication,
    EnterUpdate
);
decode_enum!(
    MaintenanceState,
    NotStarted,
    Started,
    InProgress,
    FirmwareStepComplete,
    Failed,
    WaitingDeadline,
    ResultUnknown,
    ApplicationHandoffStarted
);
decode_enum!(
    UnavailableSubject,
    Identity,
    Status,
    Measurements,
    Diagnostics,
    Config,
    OperationResult,
    ResultRelease
);
