//! Whole-program analyses. Each analysis declares a cost class; expensive ones are gated.

pub mod coupling;
pub mod cycles;
pub mod deadcode;
pub mod dupes;
pub mod hotspot;
pub mod layers;
pub mod testfiles;
