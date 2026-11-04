//! Screen-share broadcasts (spec 07 FR-7.41 … FR-7.50) and plugin-data broadcasts (spec 09
//! FR-9.59): one publisher uploads a stream once and the relay copies it to every admitted viewer. The bot creates broadcasts and admits viewers
//! through the API; the registry keeps them in memory only, like pairs.

pub mod control;
mod queue;
mod registry;
mod session;

pub use registry::{
    AdmitError, BroadcastEnd, BroadcastId, BroadcastUse, CreateError, PublisherBind, Registry,
    ViewerBind,
};
pub use session::{run_publisher, run_subscriber};
