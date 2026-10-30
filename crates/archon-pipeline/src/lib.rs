//! Pipelines: discovery and validation (`catalog`, `spec`, `builtin`),
//! prompt templating (`template`), the autonomy knobs' stages and gates
//! (`autonomy`), team stage names and merges (`team`), swarm plans that
//! assign a team its roles (`swarm`), what archond ships
//! (`ship`), the run state machine (`engine`), and edits to the unstarted
//! stages of a run (`reshape`).
//! No process spawning and no store access: the daemon drives this crate.

pub mod autonomy;
pub mod builtin;
pub mod catalog;
pub mod contract;
pub mod engine;
mod gate;
pub mod proposals;
pub mod reshape;
pub mod review;
pub mod risk;
mod sections;
pub mod ship;
pub mod spec;
pub mod swarm;
pub mod team;
pub mod template;

pub use catalog::{Catalog, CatalogSources};
pub use engine::{ApprovedProposal, EngineAction, OperatorWait, RunDriver, SpawnPlan};
pub use gate::{record_refusal, refuses_ship_record};
pub use ship::{ShipOutcome, ShipPlan, ShipRecord, attribution_lines, pr_body};
pub use spec::PipelineSpec;
pub use template::RunVars;
