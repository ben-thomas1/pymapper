//! Secondary views of the analysed project, linked to the code map by a shared selection.
//!
//! ```text
//!   quadtree.rs  own Barnes-Hut quadtree (flat arrays, rebuilt per iteration)
//!   force.rs     force-directed layout (ForceAtlas2 forces, cooling), stepped
//! ```

pub mod dsm;
pub mod force;
pub mod graph;
pub mod quadtree;

#[cfg(test)]
mod tests;
