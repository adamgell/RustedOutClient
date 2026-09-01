mod events;
mod manager;
mod model;

pub use crate::fallback::FallbackPreferences;
pub use events::{
    AppCommand, AppEvent, DesktopSize, InputAction, OpenOptions, ResizeProtocolOutcome,
    SessionTransportEvent,
};
pub use manager::{
    BackendFuture, ManagedSession, ProductionBackend, SessionBackend, SessionManager,
    APP_QUEUE_CAPACITY,
};
pub use model::{
    PublicError, PublicErrorKind, ResizeStatus, RfbFailureDetail, SessionId, SessionPhase,
    SessionSnapshot, SessionTransitionError,
};
