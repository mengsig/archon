//! The benchmark harness as a library, so the runner's pieces are testable
//! and reusable (ARCHITECTURE §15).

pub mod arms;
pub mod grade;
pub mod isolation;
pub mod metrics;
pub mod plan;
pub mod process;
pub mod report;
pub mod runner;
pub mod smoke;
pub mod summary;
pub mod swe;
pub mod task;
pub mod usage;
pub mod verify;
pub mod workspace;
