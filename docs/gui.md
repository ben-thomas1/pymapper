# pymapper-gui

Native GUI shell (`crates/pymapper-gui`, binary `pymapper-gui`). winit owns the window
and event loop. wgpu owns the GPU. egui draws only the side panels and shares wgpu's device
and queue through egui-wgpu. The centre of the window is the canvas, drawn by our own
instanced renderer (`src/map/`, see [Canvas renderer](#canvas-renderer)).

The CLI (`pymapper`) does not depend on any GPU crate. `pymapper-gui` is a workspace member
but not a default member, so build it explicitly:

```
cargo build --release -p pymapper-gui      # target/release/pymapper-gui
cargo run -p pymapper-gui -- [PATH]        # debug build; wgpu validation on
cargo run --release -p pymapper-gui -- --deps PATH        # code map incl. the venv (muted)
cargo run --release -p pymapper-gui -- --stress 1000000   # synthetic 1M rects + 1M edges
cargo test -p pymapper-gui
```

`PATH` is analysed like `pymapper check PATH` (same config discovery, project root, cache).
Load options: `--deps` / `--no-deps`, `--stdlib` / `--no-stdlib`, `--isolated` (ignore
config files), `--no-cache`, `--cache-dir DIR`. `--watch` keeps the map up to date while files
change (see [Watch mode](#watch-mode)).

## Layout

```
+-----------+--------------------------+------------+
| search    |                          | details    |
| tree      |   canvas (own pipeline)  |            |
|           |                          |            |
|           +--------------------------+            |
|           | findings                 |            |
+-----------+--------------------------+------------+
```

All three panels can be resized. The canvas takes whatever space is left. egui reports the
canvas rect in points each frame, and `PhysicalRect::from_points` converts it to framebuffer
pixels. The conversion rounds edges rather than sizes, so a panel and the canvas meet on the
same pixel at any scale factor (1x, 1.25x, 1.5x, 2x).

## Modules

```
crates/pymapper-gui/src/
  main.rs       args -> EventLoop -> App; exit codes 0 / 2
  app.rs        winit ApplicationHandler: window, surface lifecycle, egui input, redraws
  gpu.rs        instance / adapter / device / surface configuration (backend per OS)
  render.rs     one frame: clear + canvas + egui in a single render pass
  canvas.rs     canvas: background clear + map renderer, logical -> physical pixel rects
  map/          instanced code-map renderer (see "Canvas renderer")
  codemap/      the code map: in-process analysis, semantic zoom, bundled edges, scenes
                (see "Code map")
  views/        secondary views linked to the code map by selection: force-directed
                graph, dependency matrix, N-hop focus (see "Secondary views")
  mapui.rs      code map controls (details panel) and legend (canvas overlay)
  panels.rs     egui panel layout (left search/tree, right details, bottom findings),
                canvas input (pan / zoom / hover)
  schedule.rs   render-on-demand scheduler (pure, unit-tested)
  stats.rs      frame-time ring buffer + percentiles, process CPU time / peak RSS
  measure.rs    --measure: frame time + idle CPU report
  args.rs       hand-written argument parser (clap is reserved for the CLI)
crates/pymapper-gui/tests/offscreen.rs
                renders the UI and the map into an offscreen texture and checks pixels
crates/pymapper-gui/examples/canvas_bench.rs
                offscreen frame-time benchmark of the map on the stress scene
crates/pymapper-gui/examples/views_bench.rs
                CPU benchmark of the secondary views on a real project
```

## Canvas renderer

`src/map/` draws the code map. egui never touches it.

```
map/
  scene.rs   GPU instance structs, LOD bands, SceneBuilder -> Scene (sorted by layer, grid bin)
  grid.rs    own spatial grid: a short pyramid of uniform grids (culling + hit testing)
  band.rs    band-filtered slots + per-bin slot tables, rebuilt only on LOD change
  camera.rs  pan / zoom, world <-> screen, zoom limits (f32 precision budget)
  hit.rs     exact hit tests (rounded rects, Bezier ribbons) over grid candidates
  text.rs    fonts + rasterisation (glyphon's cosmic-text), own glyph atlas packer, label layout
  gpu.rs     wgpu pipelines, static instance buffers, atlas texture, camera uniform, draw list
  stress.rs  deterministic synthetic scene (packages -> modules -> functions + curves)
  map.wgsl   all shaders; one 48-byte camera uniform shared by every pipeline
```

### What is drawn

| primitive    | instance | draw                                   | shading                                   |
|--------------|----------|----------------------------------------|-------------------------------------------|
| rect         | 32 B     | 4-vertex strip per instance            | SDF rounded box: fill + inner stroke, AA  |
| line         | 40 B     | 4-vertex strip per instance            | ribbon, distance-to-centre AA             |
| curve        | 40 B     | 34-vertex strip (16 segments) in the VS | cubic Bezier ribbon of constant width     |
| glyph        | 32 B     | 4-vertex strip per instance            | `textureLoad` from an R8 alpha atlas      |

Rect radius, stroke width and edge width are in points and scaled by the DPI factor in the
shader. Colours are gamma-space RGBA8 (like egui), output premultiplied; for `*Srgb` targets
the shader decodes them first. Layers draw in ascending order (rect layers, then lines, then
curves, then labels, then the hover highlight). Items in one layer should not overlap
(true for a treemap level), because their order inside the layer is the grid order.

### Camera and LOD (no CPU work while navigating)

- The camera (centre, zoom, canvas origin, target size, text metrics, DPI) is a uniform.
  Panning and zooming rewrite those 48 bytes and nothing else.
- Zoom is grouped into bands: `band = floor(log2(zoom)) + 128`, where zoom is physical pixels
  per world unit, so each band spans a factor of 2. Every item has a `Lod { min, max }`
  band range. Rects get one automatically: visible from the first band in which they are at
  least 2 px (`MIN_RECT_PX`). Callers set explicit ranges for semantic zoom (e.g. rolled-up
  edges only at coarse bands).
- Per batch (rects, lines, curves), the set of visible items is rebuilt only when the camera
  crosses a band where that batch's membership changes (a 256-bit bitmap of change points).
  The rebuild is one linear filter pass over the batch, which is stored in grid order, and
  the batches are filtered concurrently. Its result is copied straight into wgpu's staging
  memory, one `memcpy` per run of consecutive items. Instance buffers are sized to fit
  (64 KiB granules) and shrink when less than a quarter used.
- Labels are rebuilt when the band changes, the DPI changes, or the view leaves the label
  region (the view plus one view of margin on each side). Panning inside the region costs
  nothing.

### Spatial grid (culling and hit testing)

One grid pyramid per layer: level 0 is uniform with about 8 items per cell (at most 1024
cells per axis). Each level above has cells 4 times larger, up to a single cell. An item
goes into exactly one bin: the cell holding its bbox centre, at the finest level whose cells
are at least as large as the item (a loose grid). A query therefore expands its rect by
half a cell per level. Nothing is stored twice and nothing is scanned linearly.

Bins are numbered level by level, row-major. Instances are sorted by bin, so the items of
one row of cells form one contiguous instance range:

- Culling: per frame, each layer's visible cells turn into instance ranges. Ranges
  are merged across the smallest gaps until there are at most 16 per layer
  (`limit_ranges`, which is optimal for the extra instances it draws). They are then issued
  as `draw(0..verts, start..end)` over the static buffer. Off-screen instances inside a
  range are clipped by the GPU.
- Hit testing: the same query for the cursor (plus tolerance) yields candidates. Exact
  tests follow. Edges come first, to within 3 pt of the ribbon. They are rejected by
  bbox, then by fat line (the chord strip bounded by the control points), then by pruned
  de Casteljau subdivision down to exactly the 16 segments the shader draws. Rects come
  next, from the top layer down, with the corner radius taken into account. Candidate
  scans above 16k edges run in parallel (rayon). Hover hit tests are skipped when
  pointer, camera and bands are unchanged.

Long edges sit in coarse levels, so in a dense "hairball" of long edges most of them are
candidates for any point. That cost is inherent to bbox indexing. With 1M random-ish edges
it is the slowest path (see the table below). Bundled and rolled-up edges (S7.4) keep it
small.

### Labels

Fonts come from egui's built-in font bytes, loaded into a cosmic-text `FontSystem` (via
glyphon) without a system font scan, which keeps startup fast and memory low. Glyphs are
rasterised by swash at the label size, 4 horizontal subpixel bins, into an own R8 atlas
(shelf packer, 1024^2, doubles when full). Layout is a pen walk over advances. There is
no shaping, because labels are short identifiers. ASCII glyphs are found in a flat table.

Each glyph instance carries the labelled rect, so the vertex shader places the label at the
rect's top-left (snapped to whole pixels) and applies label LOD per frame:

- The rect must be tall enough for one line.
- The rect must be wide enough for the label's first 4 characters (or all of a shorter
  label).
- Glyphs that would cross the rect's right edge are dropped, which truncates the label.

The CPU only chooses which rects get labels. Those are rects in the band, inside the label
region, that can fit a label somewhere in the band. At most 16 384 are kept, largest first.

### Memory

Per item: 32 B (rect) or 40 B (edge) instance, 2 B LOD, 4 B id, plus 8 B label span and the
label text for rects. Grid bin tables add about 0.5 B per item. Band slots add 4 B per
visible item. The GPU holds only the visible band. The 1M + 1M stress scene is 102 MiB of
CPU scene data. The GPU buffers peak at 73 MiB when everything is in the band.

### API for the code map (S7.4)

```rust
let mut b = SceneBuilder::with_capacity(rects, edges);
b.add_rect(id, layer, WorldRect::new(x0, y0, x1, y1), RectStyle { fill, stroke, radius, stroke_width }, Some(label));
b.add_curve(id, [p0, c0, c1, p1], color, width_pt, Lod { min, max });
let mut map = MapState::new(Arc::new(b.build()));   // or map.set_scene(...)
// per frame: map.apply_input(&ui.canvas_input, ppp); renderer.render(.., FrameInput { map: &mut map, .. })
```

World coordinates should stay within about +-2^14. The camera caps zoom at `2^21 / max|coord|`
so f32 error stays under 1/8 px.

## Code map

`src/codemap/` turns an analysed project into scenes for the renderer. No GPU code: all of
it is unit-tested.

```
codemap/
  load.rs     in-process analysis through core (same pipeline as `pymapper check`):
              config, roots, walks, Index (cache), imports, calls, Rollup, Aggregates
  metrics.rs  size / colour / edge metrics, colour ramps, legend data
  zoom.rs     reveal bands per node: which hierarchy cut is shown at each LOD band
  edges.rs    cut edges of every band with all edge metrics, pruned per source
  bundle.rs   hierarchical edge bundling: control polygon -> B-spline -> Bezier segments
  scene.rs    CodeMap -> map::Scene (rect / curve LODs, label carriers) + Legend
  worker.rs   background thread: load -> layout -> edges -> scene per request; watch-mode
              updates (see "Watch mode")
  transition.rs  watch-mode transitions: overlay rects animating an update
```

```
  pymapper-gui PATH
     │ MapWorker thread ─ load (once) ─ CodeMap::build (per size metric) ─ CodeMap::scene
     │                                  layout, reveal bands, edge table   (per colour / edge
     │                                                                     metric, bundling)
     ▼ Published { scene, legend, map } ──> UI thread: MapState::replace_scene (camera kept)
```

The UI thread never waits: it polls the worker once per frame (one uncontended lock), and
the worker wakes it through egui's repaint callback. While loading, the canvas shows the
phase. Requests coalesce (only the newest pending one is built). A size metric change
rebuilds the layout and edge table; colour, edge metric, bundling strength and the edge
toggles only rebuild the scene.

### Treemap

Cells are the core squarified layout (S7.3) of every node, sized by the selected metric
(lines of code by default, also symbols, cyclomatic / cognitive sums, uniform), with
third-party and stdlib code in the muted region on the right. Layer = depth, so children
draw over their parent. The map uses the core layout config with a taller header band (160
world units, still capped at 8% of a cell's height) so top-level names fit at the fit zoom.

Colour metrics: kind (categorical), max cyclomatic / cognitive complexity (absolute scale,
top of the ramp at 30 / 45), cyclomatic per 100 lines (top at 25), lines of code and
symbols (log scale relative to the largest node at the same depth). External cells are
blended 65% toward grey.

### Semantic zoom

The rollup level follows the on-screen size of each subtree, not one global depth. A node's
children appear once the node is `REVEAL_PX` = 160 physical pixels big (square root of its
area). Per node, `zoom.rs` computes two bands:

- `rev[v]`: from here `v` is shown (0 for distributions, else its parent's `open`);
- `open[v]`: from here `v`'s children are shown (never for leaves).

The *cut* at band `b` maps every node to its deepest shown ancestor-or-self. Children are
shown all at once, so a cut is monotone in pre-order and the core rollup can aggregate under
it in one pass (`Rollup::map_with`). A shown node is an edge endpoint in one of two states:
collapsed (it stands for its whole subtree) or open (its children stand for themselves, it
keeps only its own code, e.g. module-level statements). An edge `A -> B` aggregates the same
base pairs in every band where both endpoints keep their state, so it is emitted once, with
the intersection of the two state intervals as its LOD. The renderer then switches rollup
levels per band with its usual band filter; navigation does no other CPU work.

Rects get `LOD.min = max(rev, auto size LOD)`. A cell that opens moves its label: a
transparent *label carrier* covers the cell while it is collapsed, and another covers its
header band once it is open, so parent and child labels never overlap. Fully transparent
rects are culled in the vertex shader (`RectStyle::label_only`), and the shader's label LOD
hides a header label until the band is tall enough.

### Edges and metrics

For every band where the cut changes (16 on the venv), `edges.rs` fills the cut map, runs
`Rollup::map_with` (call sites, distinct pairs, imports, confidence), then one pass over the
base call pairs adds the distinct-callee metrics with a per-source stamp array:

| metric              | per edge `A -> B`                                                      |
|---------------------|------------------------------------------------------------------------|
| call sites          | distinct call sites in `A` with an edge into `B` (core rollup)         |
| distinct pairs      | distinct (caller, callee) symbol pairs (core rollup)                   |
| LOC reached         | lines of the distinct callees in `B`, each at its full span            |
| API surface used    | distinct callees in `B`                                                |
| complexity-weighted | `sum over pairs of sites x max(1, cc_sum(callee))`                     |
| imports             | module import rows (draws the import graph instead of the call graph)  |

Only rows new at the band are kept, pruned to the union of each source's 8 heaviest edges
under every metric (`EdgeParams::per_source`). Third-party and stdlib nodes never open for
edges below depth 1 (`EXTERNAL_EDGE_DEPTH`): edges into a venv stop at its packages. Edges
between two external nodes are hidden unless "Edges inside third-party / stdlib" is on. A
scene draws at most 40 000 edges first shown in any one band (the heaviest under the
selected metric).

Cost: per band O(n) for the cut plus O(m log d) for `m` base pairs; bands run on 4 workers
with 16 bytes per node of scratch each. The edge table is 36 bytes per edge.

### Bundling

`bundle.rs` routes `A -> B` through the hierarchy (Holten's hierarchical edge bundling): the
control polygon is `A`'s anchor, the centres of `A`'s ancestors up to the lowest common
ancestor, the LCA, `B`'s ancestors down from it, and `B`'s anchor. Distributions have no
common ancestor, so the region centres act as roots (edges into the venv bundle through the
main and the external region's centres). The polygon is straightened by the bundling
strength (default 0.85; slider in the panel), thinned to at most 8 points and drawn as a
clamped uniform cubic B-spline: one curve instance per segment (2-3 points: one cubic). A
collapsed endpoint is anchored at its cell's centre, an open one at its header band. Edge
width (0.75-5 pt) and alpha follow the log of the selected weight relative to the heaviest
edge in the scene; colour runs from the caller side (yellow) to the callee side (pink), or
blue to violet for imports. Edges whose calls are all only possible / unresolved are drawn
at 60% alpha.

### Panels, legend, hover

The details panel has the code map controls (size, colour, edge metric, bundling, edge
toggles). The legend sits at the canvas's bottom-left: size metric, colour swatches or ramp
with its scale, the muted swatch when external code is shown, and the edge metric with a
width scale from 1 to the heaviest weight drawn. Hovering a cell shows its qualified id,
kind, lines, symbols and complexity; hovering an edge shows both endpoints and every edge
metric.

### Results (S7.4)

Release build, Apple M4 Pro (Metal), offscreen 2880x1800 px at 2x, frame = CPU + GPU
(`cargo run --release -p pymapper-gui --example codemap_bench -- PATH [--deps] [--isolated]
[--cache-dir DIR]`), warm index cache. aiml-lectio with its venv (`--deps`): 62 331 files,
1 023 762 nodes, 1.43M call pairs.

| phase                                  | time      |
|----------------------------------------|-----------|
| load (walk, index from cache, resolve, rollup, aggregates) | 679 ms |
| code map build (layout 35 ms from the layout cache, reveal bands 9 ms, edges 44 ms) | 88 ms |
| rebuild for another size metric (fresh layout) | 102 ms |
| scene build (1.02M cells + carriers, 70k edges / 380k segments) | 92-107 ms |

| scenario (240 frames)          | frame p50 | p95      | max      | CPU p50  | instances |
|--------------------------------|-----------|----------|----------|----------|-----------|
| fit view                       | 1.51 ms   | 1.58 ms  | 2.20 ms  | 0.038 ms | 26 620    |
| pan at fit zoom                | 1.61 ms   | 1.73 ms  | 4.44 ms  | 0.105 ms | 26 620    |
| pan at 8x (functions revealed) | 1.68 ms   | 2.39 ms  | 3.61 ms  | 0.079 ms | 63 065    |
| pan at 64x                     | 1.29 ms   | 1.40 ms  | 1.88 ms  | 0.066 ms | 41 270    |
| zoom sweep 1x -> 512x -> 1x    | 1.60 ms   | 5.49 ms  | 10.8 ms  | 0.079 ms | up to ~65k |

- The zoom sweep crosses all 16 semantic-zoom bands both ways: 22 band rebuilds (mean 1.5,
  max 2.4 ms) and uploads (mean 2.5, max 5.6 ms); its max frame is a band change.
- Hover hit tests: mean 21 us at fit, 87 us at 8x, 79 us at 64x (max 0.7 ms).
- Memory: scene 100 MiB CPU (cells, carriers, curves, grids, labels), code map 38 MiB
  (layout, reveal bands, edge table), GPU buffers 28 MiB. Peak RSS 1.1 GiB, of which
  about 660 MiB is the loaded index of the venv.

Smaller inputs (same bench, cold layout):

| input                                   | nodes  | load   | code map build | edges / segments | scene build | frame p50 / max (zoom sweep) | peak RSS |
|-----------------------------------------|--------|--------|----------------|------------------|-------------|------------------------------|----------|
| aiml-lectio first party (1 803 files)   | 24 866 | 191 ms | 7.7 ms         | 59.7k / 307k     | 13.7 ms     | 1.31 / 5.7 ms                | 180 MiB  |
| stdlib 3.13 as a project (630 files)    | 18 755 | 73 ms  | 8.0 ms         | 37.4k / 124k     | 5.6 ms      | 0.96 / 7.3 ms                | 81 MiB   |

Hover hit tests on the first-party map took 155 us on average at 8x and 64x, with single
outliers of 20-29 ms in 20 000 samples (dense long bundled segments; see the hit-test notes
above).

Windowed (`pymapper-gui --measure --frames 600 --isolated --deps PATH`, same project, 120 Hz):
120.0 fps vsync-bound, CPU work p50 0.38 ms, p99 4.4 ms, max 6.7 ms; idle 5 s: 0.4 ms CPU,
0 frames; launch to the first frame with the map: 1.15 s; peak RSS 1.05 GiB. With a
project, `--measure` starts timing once the map is shown.

## Secondary views

`src/views/` adds three views next to the code map, as tabs over the canvas's top-left
corner: **Graph**, **Matrix** and **Focus**. They use the same instanced renderer and the
same `MapState`. Switching views swaps the scene and restores that view's own camera. A
selection made in any view is shown in all of them.

```
views/
  quadtree.rs  own Barnes-Hut quadtree: flat SoA cells, rebuilt per iteration, fixed-size
               query stack (no recursion, no allocation), parallel queries on a shared tree
  force.rs     force-directed layout: ForceAtlas2 forces, FR-style cooling, one step at a time
  graph.rs     view graphs: the code map's current level, N-hop focus, weighted CSR
               adjacency, radial layout
  dsm.rs       dependency matrix: SCC condensation, sink-first Kahn order, Eades-Lin-Smyth
               inside cycles
  scene.rs     node-link and matrix scenes for the renderer, selection outlines
  worker.rs    background thread: requests -> scenes (progressive for the graph)
  ui.rs        egui: tabs, per-view options; text.rs: hover details, panel texts, marks
```

```
  UI thread                              pymapper-views thread
  tab / option / click ── Request ─────> Graph : cut current level -> ForceLayout::step ...
     │                    (newest wins)          snapshot at once, then every 50 ms, final
     │                                   Matrix: Level::Module rollup -> order -> scene
     │                                   Focus : adjacency (once) -> N-hop walk -> radial
     ▼ Published { seq, scene, content } <── notify (repaint request)
  MapState::show_scene / replace_scene (camera kept for snapshots of the same request)
```

The thread starts the first time a secondary view is opened. Requests coalesce. A force
layout checks for a newer request between iterations and gives way to it, but keeps its
state, so opening the graph again resumes the layout where it stopped. Stale results are
ignored by sequence number.

### Graph: force-directed, current level

The graph is the code map's *current level*: the cells the code map shows at its camera's
band (the semantic-zoom cut) that intersect its view, at the moment you leave the code map.
Collapsed cells are always nodes. Open cells (their children are shown) become nodes only
when their own code has an edge. The edges are the rolled-up dependencies between the nodes
(`Rollup::map_with` under the cut). At most 3 000 nodes are kept, the largest by lines of
code first. The panel shows how many were left out. Zoom the code map into a package, then
open Graph to see that package's structure.

- Start layout: the treemap centres of the cells, so the graph begins in the same geography
  as the code map, plus a tiny deterministic offset per node to break symmetry.
- Forces (mass `1 + degree`): repulsion `k_r m_i m_j / d` from every node through
  Barnes-Hut (`theta` 0.9 by default, adjustable as "Accuracy"); linear attraction
  `w d` along each edge, with `w` the log of the edge weight relative to the heaviest
  edge; constant gravity `k_g m_i` towards the origin.
- Each step moves a node by at most the temperature, which starts at a tenth of the extent
  and decays by 1.5% per step. The layout stops below 2% of the start temperature
  (259 steps), at 400 steps, or when the mean move drops below 0.002 edge lengths.
- Repulsion runs in parallel from 1 024 nodes. Every node's force is summed by one thread in
  a fixed order, so the result does not depend on the thread count.
- Drawing: circles sized by lines of code (radius 0.12-0.5 edge lengths), coloured with
  the code map's colour metric (external code muted). Straight edges are trimmed to the
  circles, offset sideways so `a -> b` and `b -> a` stay apart, and have a two-stroke
  arrowhead. Width and alpha follow the log of the weight (imports + call sites,
  imports, or call sites). Names sit to the right of the nodes and appear once a node is
  about one text line tall (the shader's label LOD).

### Matrix: dependency structure matrix

Packages and modules (plus third-party / stdlib ones and their distributions when
"Third-party / stdlib modules" is on, and only the selection's subtree when "Only modules
under the selection" is on: the scope is the selection at the moment the box is ticked, so clicking rows of a scoped
matrix does not rescope it). Cell `(r, c)` is marked when row `r` depends on column `c`
(imports and/or calls rolled up with `Level::Module`). The order exposes layering:

1. strongly connected components (core Tarjan) and their condensation;
2. components sink-first by Kahn's algorithm, ties broken by the smallest node id (pre-order),
   so a package's modules stay together where the layering allows;
3. inside a cycle, members in reverse Eades-Lin-Smyth order (a greedy minimum feedback arc
   set heuristic, `O((V + E) log V)` with a lazy heap).

A layered system is then strictly lower triangular. Marks above the diagonal (red) are
feedback dependencies. They lie only inside the cycles' diagonal blocks (tinted red). Rows
carry `number qualified.name` labels, and columns carry the row number. Cells are drawn at
every zoom. Labels appear once a row is a text line tall.

### Focus: N-hop neighbourhood

The neighbourhood of the selection within 1-6 hops, following callers, callees or both. A
class or function walks the node-level call graph (weights: call sites). A package, module or
distribution walks the module graph (imports and calls rolled up to modules). Hop by hop,
the next ring's candidates are the unseen neighbours, each with its heaviest link. When they
exceed the node budget (default 400), the heaviest win. The layout is radial: the focus is
at the centre, hop `k` is on ring `k`, and each ring is ordered by its parents' angles, so
subtrees stay together. Clicking a node refocuses on it. Both adjacencies are weighted
forward + reverse CSRs, built the first time they are needed (from the rollup's base pairs,
without materialising them) and kept for the analysis.

### Linked selection

- A click selects what is under the pointer: a code map cell, a graph or focus node, a
  matrix row header or column header (that module), or a matrix cell (its row). A click on
  empty space clears the selection. Edges and cycle blocks keep it.
- Every view outlines the selection in the overlay pass (`MapState::set_marks`, no scene
  rebuild): the code map outlines the cell at any zoom, the graph and focus views outline
  the node of the selection's deepest shown ancestor-or-self, and the matrix bands that
  module's row and column.
- On returning to the code map after selecting elsewhere, the camera moves to the
  selection if its cell is off screen or smaller than 24 px.
- The details panel shows the selection's qualified id and a "Focus view on it" button,
  plus a summary of the active view (nodes, edges, layout progress; matrix size, cycles,
  feedback cells). Hovering shows node, edge, cell or cycle details.

### Cost

| view   | build                                                         | memory                                     |
|--------|---------------------------------------------------------------|--------------------------------------------|
| graph  | cut O(visible hierarchy + n + m); O(n log n) per step          | 4 B/node + 16 B/edge, layout ~20 B/node + tree |
| matrix | O(n + m) rollup + O(V + E) components + O((V + E) log V) order | 4 B/row + 16 B/cell (+ 8 B/row index)      |
| focus  | adjacency O(n + m) once; walk O(degrees of expanded nodes)    | adjacency 8 B/node + 16 B/pair             |

### Results (S7.6)

Release build, Apple M4 Pro, index cache in `target/bench-cache`
(`cargo run --release -p pymapper-gui --example views_bench -- --isolated [--deps]
--cache-dir DIR PATH`). Zoom levels are relative to the fit zoom of a 2880x1800 canvas; the
view is the canvas centred on the map.

| input (nodes / call pairs)                         | graph at fit: cut, nodes / edges, layout (steps) | graph at 8x | matrix (modules, cells, cycles) | focus walk + scene (2 hops) |
|----------------------------------------------------|--------------------------------------------------|-------------|---------------------------------|-----------------------------|
| stdlib 3.13 as a project (18.8k / 25.3k)           | 0.3 ms, 963 / 3 842, 83 ms (259)                 | 279 nodes, 13 ms | 0.5 ms build + 0.2 ms scene; 632, 2 988, 7 (largest 297) | mean 0.05 ms, max 0.14 ms |
| aiml-lectio first party (24.9k / 35.5k)            | 0.3 ms, 818 / 3 653, 70 ms (259)                 | 250 nodes, 11 ms | 0.9 ms + 0.5 ms; 1 934, 6 251, 12 (largest 192) | mean 0.06 ms, max 0.16 ms |
| aiml-lectio + venv (1.02M / 1.43M)                 | 6.0 ms, 1 521 / 10 388, 117 ms (259)             | 316 nodes, 17 ms | first party 17 ms + 0.5 ms; with external 57 ms + 30 ms: 73 227, 427 836, 436 (largest 13 077) | mean 0.16 ms, max 1.4 ms |

- Graph cut on the venv: 6 ms at the fit zoom, 13 ms at 8x and 24 ms at 64x. The O(n + m)
  rollup under the cut dominates. Node-link scenes take 0.9 ms (1.5k nodes, 31k lines
  incl. arrowheads), so a snapshot every 50 ms costs under 2% of the layout thread.
- Synthetic force layouts (clustered, 3 edges per node): 0.42 ms per step at 1k nodes,
  1.4-2.0 ms at 10k, 8.3-9.0 ms at 50k (quadtree build 4.8 ms of it, single-threaded). The
  graph's default budget of 3 000 nodes converges in well under a second.
- Focus adjacencies on the venv: call graph 5.1 ms (29.6 MiB), module graph 14.8 ms
  (14.7 MiB), built on first use.
- Matrix with every venv module: 648k rects, a 37.8 MiB scene. The order (with a 13k-module
  cycle) takes 57 ms in total.
- Peak RSS of the bench: 50 MiB (stdlib), 78 MiB (aiml-lectio first party), 847 MiB with
  the venv, of which about 670 MiB is the loaded index.

Not measured: frame times of the secondary views. Their scenes go through the same renderer
and are far smaller than the code map's (at most about 650k rects for the full-venv matrix,
30k lines for the graph), so the code map numbers above bound them.

## Watch mode

`pymapper-gui --watch PATH` applies file changes to the open map as they happen, through the
same watcher and incremental update as `pymapper check --watch` (`pymapper_core::watch`,
`incremental::Program`).

```
  pymapper-watch thread       pymapper-map thread                      UI thread
  Watcher::wait ── batch ──>  classify against the index (shared read)
                              release wanted ─────────────────────────> about_to_wait: drop the
                              wait until the CodeMap is unique   <───── CodeMap, stop the views
                              keep the old layout (its level Arcs)      thread, released
                              Analysis::update: rewalk?, Program::update,
                                Rollup::build, Aggregates::build
                              CodeMap::build (pre-order layout), Transition::build, scene
                              Published { update } ──────────────────> replace_scene (camera
                                                                        kept), remap selection,
                                                                        animate the overlays
```

- **Exclusive update, no copy.** The incremental update patches the index in place, so the
  map thread asks the UI to let go of the code map first (handled in `about_to_wait`, so an
  occluded window does not hold an update back). The scene does not reference the analysis
  and stays on screen; only hover details pause for the update. The secondary views drop
  their state and rebuild for the new map.
- **Stable geography.** Watch mode lays the treemap out in pre-order (`Order::Preorder`):
  siblings keep their order when sizes change, so an edit grows or shrinks cells in place
  instead of re-sorting them. The camera is kept.
- **Transitions.** `Transition::build` compares the old and new layouts through the index
  diff's node map: cells that moved by more than 2% of their size get a white outline that
  morphs from the old to the new rect (350 ms, ease-out); changed cells (and the module of
  every re-extracted file) flash amber, added ones green, removed ones shrink away in red
  (1.2 s fade). They are overlay rects drawn with the selection marks (`MapState::set_overlay`):
  no scene rebuild or re-upload per frame, at most 4 096 per update (largest cells first).
  The loop redraws continuously only while a transition runs.
- **Selection** carries over through the node map (cleared when its node is gone).
- A changed `pyproject.toml` / `pymapper.toml` reloads from scratch (no transition; the old
  analysis is freed first). A failed reload shows the error and retries on the next change.
- Not watched: third-party and stdlib files. The details panel shows the last update: files
  re-extracted / added / removed and the edit-to-map latency with its parts.

### Results (S7.7)

`cargo run --release -p pymapper-gui --example watch_bench -- PATH [--deps] --edits 12`
(Apple M4 Pro, 14 cores): the project's Python files are copied to `$TMPDIR` (with `--deps`
its `.venv` is linked in), loaded in watch mode without cache, then 6 files across the size
distribution get a function appended and are restored, 12 writes, each timed from the write
to the published scene (GPU upload on the next frame not included). Medians (p90 / worst):

| input                                    | nodes     | write -> scene | incremental update | rebuild (layout / edges / scene) | overlays | peak RSS |
|------------------------------------------|-----------|----------------|--------------------|----------------------------------|----------|----------|
| aiml-lectio first party (1 803 files)    | 24 866    | 83 ms (85 / 88) | 4.5 ms            | 32.5 ms (2.2 / 9.9 / 17.2)       | <= 67    | 252 MB   |
| aiml-lectio with its venv (63 290 files) | 1 032 041 | 347 ms (351 / 354) | 74 ms          | 223 ms (28 / 44 / 86)            | <= 192   | 1.26 GB  |

- The write-to-scene time is the OS delivery (about 11 ms with FSEvents) plus the 30 ms
  debounce window plus the work. On the first-party map the work is 37 ms, two thirds of it
  the scene and edge table rebuilt from scratch.
- With the venv the rebuild dominates: rollup + aggregates (about 52 ms), layout 28 ms,
  edges 44 ms and the scene 86 ms are full rebuilds. Making them incremental (only the
  changed subtree's cells and edges) is the obvious next step for million-node maps.
- The release handshake costs nothing measurable here (no UI holds the map in the bench);
  in the app it is one pass through the event loop: 2.9 ms in a windowed run
  (`pymapper-gui --watch --measure`, 120 Hz, three edits of a 3-file project while the
  scripted frames ran: edit to map 39 ms; `--measure` reports the last update on a `watch:`
  line).

## Render on demand

The loop never redraws on a timer. It sleeps in the OS event wait (`ControlFlow::Wait`)
until one of these happens:

- An input event that egui says needs a repaint (`egui_winit::State::on_window_event`).
- egui asks for a later frame after drawing one (`repaint_delay`: zero while animating,
  a finite delay for tooltips or the cursor blink, `Duration::MAX` when idle). The
  scheduler then uses `ControlFlow::WaitUntil` for that deadline only.
- Another thread calls `egui::Context::request_repaint`, which will be the layout thread
  later. egui's repaint callback posts a `UserEvent::Repaint` through the event-loop proxy.
  The event is dropped if a frame has finished since the request was made, because that
  frame's `repaint_delay` already covered it.
- Resize, scale-factor change, or the window becoming visible again (`Occluded(false)`).

The surface uses vsync (`PresentMode::AutoVsync`, frame latency 2). Because frames are
only drawn on demand, vsync costs nothing while the window is idle. Covered or minimised
windows draw nothing.

The window is created hidden and shown after the first frame is presented, so it never
flashes blank. On macOS a hidden window has no drawable, and wgpu reports `Occluded`. In
that case the app shows the window and retries once. Wayland ignores the hidden flag: a
Wayland window maps on its first commit.

Surface errors never abort the app. `Timeout` retries. `Outdated` and `Suboptimal`
reconfigure from the window's *current* size (not the last configured one, which could stay
outdated forever). `Lost` recreates the surface. `Validation` drops the frame, because wgpu
has already sent the error to its handler.

## HiDPI and resize

- egui-winit gets the native scale factor at startup and on `ScaleFactorChanged`, so
  panels are laid out in points and rasterised at the native resolution.
- The surface is configured in physical pixels. It is clamped to the device's maximum
  texture dimension, and left unconfigured while the window has zero area (minimised). The
  device requests `Limits::default().using_resolution(adapter.limits())`, so large HiDPI
  displays work.
- The canvas shader works in physical pixels: grid spacing and line width are multiplied by
  the scale factor.
- `Resized` reconfigures the surface and calls `request_redraw` directly, because the loop
  may not reach `about_to_wait` during a macOS live resize.

## Platforms

| OS    | Backend | Window system                         | Status                          |
|-------|---------|---------------------------------------|---------------------------------|
| macOS | Metal   | AppKit                                | built, tested, measured         |
| Linux | Vulkan  | Wayland first, X11/XWayland fallback  | compiles (cfg-gated), not run   |

The per-OS choices live in `crates/pymapper-gui/Cargo.toml` (target-specific features) and
`gpu::BACKENDS`:

- macOS: `wgpu/metal`. winit uses no extra features.
- Linux: `wgpu/vulkan`. winit uses `wayland`, `wayland-dlopen`,
  `wayland-csd-adwaita-notitle`, `x11`, and egui-winit uses `wayland`, `x11`. The
  Wayland, X11 and xkb client libraries are loaded with dlopen at runtime, so a build needs
  no system `-dev` packages. At runtime it needs the Vulkan loader plus a driver
  (`libvulkan.so.1`), and either `libwayland-client.so` + `libxkbcommon.so` or the X11
  libraries. winit picks Wayland when `WAYLAND_DISPLAY` (or `WAYLAND_SOCKET`) is set, and
  X11 via `DISPLAY` otherwise. To force X11 under Wayland, run with `WAYLAND_DISPLAY=`
  unset. With `wayland-csd-adwaita-notitle`, compositors without server-side
  decorations (GNOME) draw client-side decorations without a title text, which avoids a
  font-rendering dependency.
- Other targets fall back to wgpu's primary backends. They are unsupported.

To check that the Linux paths compile from macOS (the target only needs `rust-std`):

```
rustup target add aarch64-unknown-linux-gnu
cargo check  -p pymapper-gui --all-targets --target aarch64-unknown-linux-gnu
cargo clippy -p pymapper-gui --all-targets --target aarch64-unknown-linux-gnu -- -D warnings
```

`ru_maxrss` is in bytes on macOS and in KiB on Linux. `stats::process_usage` normalises it
to bytes.

Environment overrides (all read by wgpu): `WGPU_BACKEND` (for example `metal` or `vulkan`),
`WGPU_POWER_PREF` (`low`, the default because the map is 2D, or `high`), and `WGPU_DEBUG` /
`WGPU_VALIDATION` for instance flags. Validation is on in debug builds and off in release.

## Measuring: `pymapper-gui --measure`

```
pymapper-gui --measure [--frames N] [--idle SECS] [PATH]
```

The measurement runs in four phases:

1. 30 warm-up frames, which are not recorded (shader compilation, font atlas upload).
2. N timed frames (default 300), with a redraw forced every frame.
3. A 0.5 s settle period, so egui can finish any follow-up repaints.
4. An idle window (default 5 s) where nothing touches the window.

It then prints a report and exits (code 0). Measuring needs a window that can present.
The window floats above other windows in this mode, but it cannot cross Spaces, so a
full-screen app on another Space still occludes it. If no frame is presented for 10 s while
frames are forced, the run stops with exit code 2 and says so rather than waiting forever.

Per-frame CPU times are split into:

- `ui`: egui input, layout and tessellation.
- `upload+encode+submit+present`.
- `cpu work`: the sum of the two above.
- `swapchain acquire`: waiting for a drawable. This includes vsync back-pressure, so it is
  not work.

The idle line shows process CPU time across all threads (`getrusage`), frames rendered and
event-loop wakeups during the idle window. Run it on a release build. Moving the mouse over
the window during the idle window counts as wakeups and frames.

`--measure` does not measure GPU execution time separately. That would need timestamp
queries, which add a device feature requirement. The offscreen `canvas_bench` example
measures CPU and GPU time together by waiting for the GPU after each frame (see below).

### Results (macOS, shell with placeholder canvas)

Release build, Apple M4 Pro (Metal), 120 Hz display, surface 2880x1800 px at 2x:

| frames | cpu work p50 / p99 / max | ui p50   | acquire p50 (vsync) | fps   | idle CPU             | idle frames | peak RSS  |
|--------|--------------------------|----------|---------------------|-------|----------------------|-------------|-----------|
| 300    | 0.070 / 0.222 / 0.265 ms | 0.016 ms | 8.24 ms             | 120.0 | 0.5 ms in 5 s (0.010%) | 0         | 110.2 MiB |
| 600    | 0.127 / 0.273 / 0.310 ms | 0.031 ms | 8.19 ms             | 120.0 | 0.4 ms in 10 s (0.004%) | 0        | 105.2 MiB |

Launch to first frame presented: 140-200 ms over several runs. Across runs, idle CPU was
0.3-0.5 ms per 5-10 s window, with 0 frames rendered and 1 event wakeup. Peak RSS was
96-111 MiB.

Measured from outside the process (`ps`), a release window left idle for 30 s accumulated
at most 10 ms of CPU time, which is the resolution of `ps`.

Where the memory goes (`vmmap --summary` on an idle window at 2880x1800):

- 60.5 MB `IOSurface`, which is the swapchain drawables (about 20 MB each at this size).
- About 16 MB malloc (egui font atlas, tessellation buffers, wgpu bookkeeping).
- The rest is dirty `__DATA` of system frameworks (Metal, AppKit).

Lowering `FRAME_LATENCY` to 1, which sets the CAMetalLayer drawable count to 2, did not
change the IOSurface footprint in a test, so it stays at 2 (the wgpu default).

### Map renderer results (1M rects + 1M edges)

`--stress N` replaces the empty map with the synthetic scene (N rects in 100 packages x 100
modules x ~99 functions, N Bezier edges: 70% local, 20% within a package range, 10% global).
With `--measure`, the timed frames follow a scripted path: zoom from fit to 128x and back
while drifting.

Release build, Apple M4 Pro (Metal), 2880x1800 px at 2x, 120 Hz:

```
pymapper-gui --measure --stress 1000000 --frames 600
timed frames: 600 (116.9 fps, vsync-bound)
  cpu work (ui + submit)   mean 0.879  p50 0.646  p95 2.701  p99 7.266  max 21.145 ms
idle: 5.0 s, cpu 0.8 ms (0.015% of one core), frames rendered 0, event wakeups 1
peak RSS: 410.8 MiB
scene build 100.6 ms; band rebuilds 9 (mean 1.95, max 4.05 ms);
band uploads 9 (mean 4.64, max 14.88 ms); label rebuilds 58 (mean 0.88, max 2.97 ms)
```

`cargo run --release -p pymapper-gui --example canvas_bench -- 1000000 1000000` renders
offscreen (no vsync, no window) and waits for the GPU after every frame. Frame time is
therefore the serialised CPU + GPU cost. 240 frames per scenario:

| scenario                             | frame p50 | p95      | max      | CPU p50  | instances drawn |
|--------------------------------------|-----------|----------|----------|----------|-----------------|
| fit view (natural LOD)               | 1.11 ms   | 1.27 ms  | 1.40 ms  | 0.055 ms | 71 200          |
| pan at fit zoom                      | 0.93 ms   | 1.00 ms  | 1.50 ms  | 0.071 ms | 71 200          |
| pan at 16x (grid culling)            | 7.01 ms   | 8.60 ms  | 9.49 ms  | 0.077 ms | 228 968         |
| pan at 128x (grid culling)           | 2.56 ms   | 3.16 ms  | 3.36 ms  | 0.074 ms | 135 583         |
| zoom sweep 1x -> 128x -> 1x          | 5.54 ms   | 18.4 ms  | 37.4 ms  | 0.080 ms | up to ~230k     |
| everything drawn, fit (2M instances) | 45.8 ms   | 48.9 ms  | 51.4 ms  | 0.079 ms | 2 000 990       |

Notes on the table:

- Scene build (generate + sort + grids) takes 108 ms. The first frame (fit, band build,
  labels, upload) takes 12 ms.
- Band rebuilds: mean 1.44 ms, max 2.39 ms. Band uploads: mean 3.8 ms, max 6.0 ms of CPU.
  Label rebuilds: mean 0.30 ms.
- In the zoom sweep, the max frame includes a band upload of up to 72 MiB.
- At 16x the frame time is GPU fill: about 10k long translucent edges cross the view.
  The forced worst case is fill-bound the same way, and the natural LOD never draws it.
- Hit test (20k random points): 2.9 us mean at fit, 351 us at 16x and 319 us at 128x (dense
  long edges, parallel scan). Rects alone take about 0.1 us. The first parallel scan
  spawns rayon's pool (one 22 ms outlier).
- Memory: scene 102 MiB CPU, GPU buffers 73 MiB peak, atlas 1 MiB.

At 100k + 100k the fit view already shows every edge (7.2 ms, fill-bound). Zoomed views
take 1-1.7 ms, and hit tests 117-194 us.

## Dependencies

Only the GUI crates the dependency policy allows: winit, wgpu, egui, egui-wgpu, egui-winit,
glyphon and pollster, plus anyhow, libc (getrusage) and rayon (parallel band filtering and
dense hit tests). Default features are off where a crate has any. Each crate gets only what
it needs:

- wgpu: `std`, `parking_lot`, `wgsl`, plus a backend per OS.
- egui: `default_fonts`.
- egui-winit: `clipboard`, so copy and paste work in text fields.
- winit: `rwh_06`.
- glyphon (0.12, same wgpu major): used for its cosmic-text re-export (font database,
  swash rasterisation). glyphon's own screen-space `TextRenderer` is not used. It re-prepares
  vertices whenever text moves, and the map needs world-anchored labels that cost nothing
  while the camera moves.

Known transitive cost: egui-winit's `clipboard` feature enables `arboard` with
`image-data`, which pulls in the `image` crate (png/tiff decoders) for image clipboard
support we do not use. This affects binary size and build time, not runtime memory, and
egui-winit does not expose a switch for it. The alternative is our own NSPasteboard /
wl-clipboard layer, which is a follow-up if binary size matters.
