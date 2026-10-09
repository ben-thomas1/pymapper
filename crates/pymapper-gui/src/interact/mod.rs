//! Interaction with the code map: hover neighbourhood, select / focus, fuzzy search-and-jump,
//! path A -> B, overlays (cycles, layer violations, finding badges) and filters (confidence,
//! origin), the details panel and the source pane.

pub mod findings;
pub mod fuzzy;
pub mod highlight;
pub mod marks;
pub mod search;
pub mod sceneindex;
pub mod source;
pub mod state;
pub mod worker;

#[cfg(test)]
mod tests;
