//! The SWE family: real repository issues from Aether's shared `swe-v1` set,
//! run on Archon's `direct` pipeline per model against the best recent
//! release and the native CLI, gated so a release is never worse than either.
//!
//! Aether's runner owns the set, its sealed holdout, the instance images and
//! the grading; archon-bench chooses the arms, prints the estimate, reads the
//! runner's aggregates and renders the gate (see `bench/results-swe/README.md`).

pub mod aggregate;
pub mod arms;
pub mod estimate;
pub mod gate;
pub mod manifest;
pub mod release;
pub mod report;
pub mod runner;
pub mod sealed;
pub mod stats;
