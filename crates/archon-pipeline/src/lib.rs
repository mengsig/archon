//! Pipelines: discovery and validation (`catalog`, `spec`, `builtin`),
//! prompt templating (`template`), the autonomy knobs' stages and gates
//! (`autonomy`), team stage names and merges (`team`), what archond ships
//! (`ship`), and the run state machine (`engine`).
//! No process spawning and no store access: the daemon drives this crate.

pub mod autonomy;
pub mod builtin;
pub mod catalog;
pub mod contract;
pub mod engine;
mod gate;
pub mod review;
mod sections;
pub mod ship;
pub mod spec;
pub mod team;
pub mod template;

pub use catalog::{Catalog, CatalogSources};
pub use engine::{EngineAction, OperatorWait, RunDriver, SpawnPlan};
pub use ship::{ShipOutcome, ShipPlan, ShipRecord};
pub use spec::PipelineSpec;
pub use template::RunVars;
