//! Tooling for working on the launcher, compiled only under cfg(test) and
//! reached through make targets.
//!
//! The performance gate and the scan bench (perf), one frame drawn without a
//! compositor and what it measures out to (screenshot), and the encoder that
//! writes that frame to a file (png).

mod perf;
mod png;
mod screenshot;
