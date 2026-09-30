use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::stdout;
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Gauge, List, ListItem, ListState, Paragraph, Row, Table,
    TableState, Tabs,
    canvas::{Canvas, Line as CanvasLine, Rectangle as CanvasRect},
};

use crate::cycles;
use crate::dead_code;
use crate::graph::{self, CallGraph};
use crate::layout;
use crate::loader::{self, LoadProgress, ThreadCounts};
use crate::metrics::{self, GraphMetrics};
use crate::report::{self, ModuleSummaryRow, ReportRow};
use crate::types::ModuleInfo;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

struct AnalysisResult {
    module: ModuleInfo,
    metrics: HashMap<String, GraphMetrics>,
    call_graph: CallGraph,
    cycle_graph: CallGraph,
    dead_count: usize,
    cycle_count: usize,
}

enum ParseMsg {
    Walking,
    WalkProgress { discovered: usize },
    Progress { done: usize, total: usize },
    Warning(String),
    Failed(String),
    BuildingGraph { done: usize, total: usize },
    ComputingMetrics { done: usize, total: usize },
    BuildingCycleGraph { done: usize, total: usize },
    Done(AnalysisResult),
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Home,
    Report,
    Modules,
    DeadCode,
    Cycles,
    Graph,
}

impl Tab {
    fn index(self) -> usize {
        match self {
            Tab::Home => 0,
            Tab::Report => 1,
            Tab::Modules => 2,
            Tab::DeadCode => 3,
            Tab::Cycles => 4,
            Tab::Graph => 5,
        }
    }

    fn next(self) -> Tab {
        match self {
            Tab::Home => Tab::Report,
            Tab::Report => Tab::Modules,
            Tab::Modules => Tab::DeadCode,
            Tab::DeadCode => Tab::Cycles,
            Tab::Cycles => Tab::Graph,
            Tab::Graph => Tab::Home,
        }
    }

    fn prev(self) -> Tab {
        match self {
            Tab::Home => Tab::Graph,
            Tab::Report => Tab::Home,
            Tab::Modules => Tab::Report,
            Tab::DeadCode => Tab::Modules,
            Tab::Cycles => Tab::DeadCode,
            Tab::Graph => Tab::Cycles,
        }
    }
}

struct DeadRow {
    name: String,
    loc: usize,
    lines: String,
    module: String,
    classification: String,
}

struct HomeData {
    paths: Vec<String>,
    file_count: usize,
    func_count: usize,
    class_count: usize,
    module_count: usize,
    avg_cc: f64,
    max_cc: usize,
    max_cc_func: String,
    dead_count: usize,
    cycle_count: usize,
    total_loc: usize,
}

struct SourceView {
    title: String,
    highlighted_lines: Vec<Line<'static>>,
    scroll: usize,
    visible_height: usize,
}

// ---------------------------------------------------------------------------
// Graph tab data structures
// ---------------------------------------------------------------------------

struct GraphTabData {
    func_names: Vec<String>,
    callers: HashMap<String, Vec<(String, usize)>>,
    callees: HashMap<String, Vec<(String, usize)>>,
    name_to_index: HashMap<String, usize>,
    cc_map: HashMap<String, usize>,
    external_nodes: HashSet<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum GraphSortCol {
    Name,
    McC,
}

impl GraphSortCol {
    fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::McC => "McC",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Name => Self::McC,
            Self::McC => Self::Name,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum GraphFocus {
    Canvas,
    FuncList,
    Callers,
    Callees,
}

impl GraphFocus {
    fn next(self) -> Self {
        match self {
            Self::Canvas => Self::Callers,
            Self::Callers => Self::Callees,
            Self::Callees => Self::Canvas,
            Self::FuncList => Self::Callers,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Canvas => Self::Callees,
            Self::Callers => Self::Canvas,
            Self::Callees => Self::Callers,
            Self::FuncList => Self::Callees,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum CanvasMode {
    Local,
    Global,
}

struct TreeOverlay {
    root_name: String,
    downstream: bool,
    max_depth: usize,
    lines: Vec<TreeLine>,
    scroll: usize,
    cursor: usize,
    visible_height: usize,
    collapsed: HashSet<String>,
}

struct TreeLine {
    indent: usize,
    name: String,
    mcc: usize,
    is_external: bool,
    has_children: bool,
    is_collapsed: bool,
    is_cycle: bool,
}

struct FuncPicker {
    query: String,
    filtered: Vec<usize>,
    cursor: usize,
    scroll: usize,
    visible_height: usize,
}

// ---------------------------------------------------------------------------
// Sort column enums
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum ReportSortCol {
    Name,
    McC,
    Cognitive,
    Nesting,
    Loc,
    Params,
    FanIn,
    FanOut,
    Rfc,
    Depth,
    TLoc,
    TMcC,
    MI,
    Returns,
    BoolExpr,
    Lines,
}

impl ReportSortCol {
    fn label(self) -> &'static str {
        match self {
            Self::Name => "Function",
            Self::McC => "McC",
            Self::Cognitive => "Cognitive",
            Self::Nesting => "Nesting",
            Self::Loc => "LOC",
            Self::Params => "Params",
            Self::FanIn => "FI",
            Self::FanOut => "FO",
            Self::Rfc => "RFC",
            Self::Depth => "Depth",
            Self::TLoc => "T-LOC",
            Self::TMcC => "T-McC",
            Self::MI => "MI",
            Self::Returns => "Ret",
            Self::BoolExpr => "Bool",
            Self::Lines => "Lines",
        }
    }

    fn all() -> &'static [ReportSortCol] {
        &[
            Self::Name,
            Self::Loc,
            Self::TLoc,
            Self::McC,
            Self::TMcC,
            Self::Cognitive,
            Self::Nesting,
            Self::Params,
            Self::FanIn,
            Self::FanOut,
            Self::Rfc,
            Self::Depth,
            Self::MI,
            Self::Returns,
            Self::BoolExpr,
            Self::Lines,
        ]
    }

    fn from_index(i: usize) -> Self {
        Self::all()[i.min(Self::all().len() - 1)]
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ModuleSortCol {
    Module,
    Funcs,
    TLoc,
    AvgMcC,
    MaxMcC,
    CrossMod,
    Dead,
    Ca,
    Ce,
    Instability,
}

impl ModuleSortCol {
    fn label(self) -> &'static str {
        match self {
            Self::Module => "Module",
            Self::Funcs => "Funcs",
            Self::TLoc => "T-LOC",
            Self::AvgMcC => "Avg McC",
            Self::MaxMcC => "Max McC",
            Self::CrossMod => "X-Mod",
            Self::Dead => "Dead",
            Self::Ca => "Ca",
            Self::Ce => "Ce",
            Self::Instability => "I",
        }
    }

    fn all() -> &'static [ModuleSortCol] {
        &[
            Self::Module,
            Self::Funcs,
            Self::TLoc,
            Self::AvgMcC,
            Self::MaxMcC,
            Self::CrossMod,
            Self::Dead,
            Self::Ca,
            Self::Ce,
            Self::Instability,
        ]
    }

    fn from_index(i: usize) -> Self {
        Self::all()[i.min(Self::all().len() - 1)]
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct App {
    active_tab: Tab,
    analysis: Option<AnalysisResult>,
    parse_receiver: mpsc::Receiver<ParseMsg>,
    paths: Vec<String>,
    file_count: usize,
    parse_done: usize,
    graph_done: usize,
    graph_total: usize,
    metrics_done: usize,
    metrics_total: usize,
    cycle_graph_done: usize,
    cycle_graph_total: usize,

    home_data: Option<HomeData>,
    report_rows: Option<Vec<ReportRow>>,
    module_rows: Option<Vec<ModuleSummaryRow>>,
    dead_rows: Option<Vec<DeadRow>>,
    cycle_groups: Option<Vec<Vec<String>>>,

    report_table_state: TableState,
    module_table_state: TableState,
    dead_table_state: TableState,
    cycle_list_state: ListState,

    report_sort_col: ReportSortCol,
    report_sort_asc: bool,
    report_selected_col: usize,

    module_sort_col: ModuleSortCol,
    module_sort_asc: bool,
    module_selected_col: usize,

    source_view: Option<SourceView>,
    last_terminal_height: u16,

    graph_tab_data: Option<GraphTabData>,
    graph_func_list_state: ListState,
    graph_callers_state: ListState,
    graph_callees_state: ListState,
    graph_focus: GraphFocus,
    graph_nav_history: Vec<usize>,
    graph_last_selected_func: Option<usize>,
    tree_overlay: Option<TreeOverlay>,
    func_picker: Option<FuncPicker>,
    graph_sort_col: GraphSortCol,
    graph_sort_asc: bool,

    // Canvas state
    graph_canvas_mode: CanvasMode,
    graph_sidebar_visible: bool,
    graph_hop_depth: usize,
    graph_canvas_selected_node: Option<String>,
    graph_local_layout_cache: HashMap<(String, usize), HashMap<String, (f64, f64)>>,
    graph_global_layout_cache: Option<HashMap<String, (f64, f64)>>,

    log_messages: Vec<String>,
    walk_discovered: usize,
    walk_complete: bool,
    parse_complete: bool,
    graph_complete: bool,
    metrics_complete: bool,
    cycle_graph_complete: bool,
    walk_elapsed: Duration,
    parse_elapsed: Duration,
    graph_elapsed: Duration,
    metrics_elapsed: Duration,
    cycle_graph_elapsed: Duration,
    phase_start: Instant,
}

// ---------------------------------------------------------------------------
// Terminal guard
// ---------------------------------------------------------------------------

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(stdout(), LeaveAlternateScreen);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Launches the interactive TUI explorer.
pub fn run_tui(
    paths: &[String],
    thread_counts: ThreadCounts,
) -> Result<(), Box<dyn std::error::Error>> {
    let paths_owned: Vec<String> = paths.to_vec();
    let (tx, rx) = mpsc::channel();

    let paths_for_thread = paths_owned.clone();
    let tx_progress = Mutex::new(tx.clone());
    thread::spawn(move || {
        let combined =
            loader::load_modules_with_progress(&paths_for_thread, thread_counts, |msg| {
                let parse_msg = match msg {
                    LoadProgress::Walking => ParseMsg::Walking,
                    LoadProgress::WalkProgress { discovered } => {
                        ParseMsg::WalkProgress { discovered }
                    }
                    LoadProgress::Parsing { done, total } => ParseMsg::Progress { done, total },
                    LoadProgress::Warning(s) => ParseMsg::Warning(s),
                };
                let _ = tx_progress.lock().unwrap().send(parse_msg);
            });
        let combined = match combined {
            Ok(combined) => combined,
            Err(error) => {
                let _ = tx.send(ParseMsg::Failed(error));
                return;
            }
        };

        let call_graph = graph::build_call_graph_with_progress(&combined, false, |done, total| {
            let _ = tx_progress
                .lock()
                .unwrap()
                .send(ParseMsg::BuildingGraph { done, total });
        });
        let metrics = metrics::compute_graph_metrics_with_progress(
            &call_graph,
            &combined.functions,
            |done, total| {
                let _ = tx_progress
                    .lock()
                    .unwrap()
                    .send(ParseMsg::ComputingMetrics { done, total });
            },
        );
        let cycle_graph = graph::build_call_graph_with_progress(&combined, true, |done, total| {
            let _ = tx_progress
                .lock()
                .unwrap()
                .send(ParseMsg::BuildingCycleGraph { done, total });
        });
        let dead_count =
            dead_code::find_dead_code(&combined.functions, &combined.classes, &metrics).len();
        let cycle_count = cycles::find_cycles(&cycle_graph).len();
        let _ = tx.send(ParseMsg::Done(AnalysisResult {
            module: combined,
            metrics,
            call_graph,
            cycle_graph,
            dead_count,
            cycle_count,
        }));
    });

    crossterm::terminal::enable_raw_mode()?;
    let _guard = TerminalGuard;
    crossterm::execute!(stdout(), EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App {
        active_tab: Tab::Home,
        analysis: None,
        parse_receiver: rx,
        paths: paths_owned,
        file_count: 0,
        parse_done: 0,
        graph_done: 0,
        graph_total: 0,
        metrics_done: 0,
        metrics_total: 0,
        cycle_graph_done: 0,
        cycle_graph_total: 0,
        home_data: None,
        report_rows: None,
        module_rows: None,
        dead_rows: None,
        cycle_groups: None,
        report_table_state: TableState::default(),
        module_table_state: TableState::default(),
        dead_table_state: TableState::default(),
        cycle_list_state: ListState::default(),
        report_sort_col: ReportSortCol::McC,
        report_sort_asc: false,
        report_selected_col: 3, // McC
        module_sort_col: ModuleSortCol::TLoc,
        module_sort_asc: false,
        module_selected_col: 2, // T-LOC
        source_view: None,
        last_terminal_height: 0,
        graph_tab_data: None,
        graph_func_list_state: ListState::default(),
        graph_callers_state: ListState::default(),
        graph_callees_state: ListState::default(),
        graph_focus: GraphFocus::Canvas,
        graph_nav_history: Vec::new(),
        graph_last_selected_func: None,
        tree_overlay: None,
        func_picker: None,
        graph_sort_col: GraphSortCol::Name,
        graph_sort_asc: true,
        graph_canvas_mode: CanvasMode::Local,
        graph_sidebar_visible: true,
        graph_hop_depth: 2,
        graph_canvas_selected_node: None,
        graph_local_layout_cache: HashMap::new(),
        graph_global_layout_cache: None,
        log_messages: Vec::new(),
        walk_discovered: 0,
        walk_complete: false,
        parse_complete: false,
        graph_complete: false,
        metrics_complete: false,
        cycle_graph_complete: false,
        walk_elapsed: Duration::ZERO,
        parse_elapsed: Duration::ZERO,
        graph_elapsed: Duration::ZERO,
        metrics_elapsed: Duration::ZERO,
        cycle_graph_elapsed: Duration::ZERO,
        phase_start: Instant::now(),
    };

    loop {
        poll_messages(&mut app).map_err(std::io::Error::other)?;
        ensure_tab_data(&mut app);

        terminal.draw(|f| render(f, &mut app))?;

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    break;
                }
                if app.tree_overlay.is_some() {
                    handle_key_tree_overlay(&mut app, key);
                } else if app.source_view.is_some() {
                    if handle_key_source_view(&mut app, key) {
                        break;
                    }
                } else if app.func_picker.is_some() {
                    handle_key_func_picker(&mut app, key);
                } else if handle_key(&mut app, key) {
                    break;
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Message polling
// ---------------------------------------------------------------------------

fn poll_messages(app: &mut App) -> Result<(), String> {
    loop {
        let msg = match app.parse_receiver.try_recv() {
            Ok(msg) => msg,
            Err(mpsc::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::TryRecvError::Disconnected) if app.analysis.is_some() => return Ok(()),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("analysis worker stopped before completion".to_string());
            }
        };
        match msg {
            ParseMsg::Failed(error) => return Err(error),
            ParseMsg::Walking => {
                app.phase_start = Instant::now();
            }
            ParseMsg::WalkProgress { discovered } => {
                app.walk_discovered = discovered;
            }
            ParseMsg::Progress { done, total } => {
                if !app.walk_complete {
                    app.walk_complete = true;
                    app.walk_elapsed = app.phase_start.elapsed();
                    app.phase_start = Instant::now();
                }
                app.parse_done = done;
                app.file_count = total;
            }
            ParseMsg::Warning(msg) => {
                app.log_messages.push(msg);
            }
            ParseMsg::BuildingGraph { done, total } => {
                if !app.parse_complete {
                    app.parse_complete = true;
                    app.parse_elapsed = app.phase_start.elapsed();
                    app.phase_start = Instant::now();
                }
                app.graph_done = done;
                app.graph_total = total;
                if done == total && !app.graph_complete {
                    app.graph_complete = true;
                    app.graph_elapsed = app.phase_start.elapsed();
                    app.phase_start = Instant::now();
                }
            }
            ParseMsg::ComputingMetrics { done, total } => {
                app.metrics_done = done;
                app.metrics_total = total;
                if done == total && !app.metrics_complete {
                    app.metrics_complete = true;
                    app.metrics_elapsed = app.phase_start.elapsed();
                    app.phase_start = Instant::now();
                }
            }
            ParseMsg::BuildingCycleGraph { done, total } => {
                app.cycle_graph_done = done;
                app.cycle_graph_total = total;
                if done == total && !app.cycle_graph_complete {
                    app.cycle_graph_complete = true;
                    app.cycle_graph_elapsed = app.phase_start.elapsed();
                }
            }
            ParseMsg::Done(result) => {
                app.analysis = Some(result);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Lazy tab data
// ---------------------------------------------------------------------------

fn ensure_tab_data(app: &mut App) {
    let analysis = match &app.analysis {
        Some(a) => a,
        None => return,
    };

    match app.active_tab {
        Tab::Home => {
            if app.home_data.is_none() {
                let total_loc: usize = analysis.module.functions.iter().map(|f| f.line_count).sum();
                let func_count = analysis.module.functions.len();
                let avg_cc = if func_count == 0 {
                    0.0
                } else {
                    let total_cc: usize =
                        analysis.module.functions.iter().map(|f| f.complexity).sum();
                    (total_cc as f64 / func_count as f64 * 100.0).round() / 100.0
                };
                let (max_cc, max_cc_func) = analysis
                    .module
                    .functions
                    .iter()
                    .map(|f| (f.complexity, f.qualified_name()))
                    .max_by_key(|(cc, _)| *cc)
                    .unwrap_or((0, String::new()));

                let mut module_set = std::collections::HashSet::new();
                for f in &analysis.module.functions {
                    module_set.insert(f.source_module.clone());
                }

                app.home_data = Some(HomeData {
                    paths: app.paths.clone(),
                    file_count: app.file_count,
                    func_count,
                    class_count: analysis.module.classes.len(),
                    module_count: module_set.len(),
                    avg_cc,
                    max_cc,
                    max_cc_func,
                    dead_count: analysis.dead_count,
                    cycle_count: analysis.cycle_count,
                    total_loc,
                });
            }
        }
        Tab::Report => {
            if app.report_rows.is_none() {
                let (mut rows, _) = report::build_report_rows_unsorted(
                    &analysis.module,
                    &analysis.metrics,
                    None,
                    None,
                );
                sort_report_rows(&mut rows, app.report_sort_col, app.report_sort_asc);
                app.report_rows = Some(rows);
                app.report_table_state.select(Some(0));
            }
        }
        Tab::Modules => {
            if app.module_rows.is_none() {
                let mut rows =
                    report::build_module_summary(&analysis.module, &analysis.metrics, None);
                sort_module_rows(&mut rows, app.module_sort_col, app.module_sort_asc);
                app.module_rows = Some(rows);
                app.module_table_state.select(Some(0));
            }
        }
        Tab::DeadCode => {
            if app.dead_rows.is_none() {
                let options = dead_code::DeadCodeOptions {
                    classify: true,
                    ..Default::default()
                };
                let entries = dead_code::find_dead_code_classified(
                    &analysis.module.functions,
                    &analysis.module.classes,
                    &analysis.metrics,
                    &options,
                );
                let rows: Vec<DeadRow> = entries
                    .iter()
                    .map(|e| DeadRow {
                        name: e.func.qualified_name(),
                        loc: e.func.line_count,
                        lines: format!("{}-{}", e.func.start_line, e.func.end_line),
                        module: e.func.source_module.clone(),
                        classification: match e.classification {
                            dead_code::DeadCodeClass::TrulyDead => "candidate".to_string(),
                            dead_code::DeadCodeClass::EntryPoint => "entry-point".to_string(),
                            dead_code::DeadCodeClass::Override => "override".to_string(),
                            dead_code::DeadCodeClass::Inherited => "inherited".to_string(),
                            dead_code::DeadCodeClass::TestFixture => "test-fixture".to_string(),
                        },
                    })
                    .collect();
                let has_rows = !rows.is_empty();
                app.dead_rows = Some(rows);
                if has_rows {
                    app.dead_table_state.select(Some(0));
                }
            }
        }
        Tab::Cycles => {
            if app.cycle_groups.is_none() {
                let found = cycles::find_cycles(&analysis.cycle_graph);
                let has_cycles = !found.is_empty();
                app.cycle_groups = Some(found);
                if has_cycles {
                    app.cycle_list_state.select(Some(0));
                }
            }
        }
        Tab::Graph => {
            if app.graph_tab_data.is_none() {
                let cg = &analysis.call_graph;
                let mut func_names: Vec<String> = analysis
                    .module
                    .functions
                    .iter()
                    .map(|f| f.qualified_name())
                    .collect();
                // Include external nodes so they're navigable
                for ext in &cg.external_nodes {
                    func_names.push(ext.clone());
                }

                let mut cc_map: HashMap<String, usize> = HashMap::new();
                for f in &analysis.module.functions {
                    cc_map.insert(f.qualified_name(), f.complexity);
                }

                let mut callers: HashMap<String, Vec<(String, usize)>> = HashMap::new();
                let mut callees: HashMap<String, Vec<(String, usize)>> = HashMap::new();

                let idx_to_name: HashMap<petgraph::graph::NodeIndex, &str> = cg
                    .node_map
                    .iter()
                    .map(|(name, &idx)| (idx, name.as_str()))
                    .collect();

                for edge in cg.graph.edge_indices() {
                    let (src, dst) = cg.graph.edge_endpoints(edge).unwrap();
                    let src_name = idx_to_name[&src];
                    let dst_name = idx_to_name[&dst];
                    let src_cc = cc_map.get(src_name).copied().unwrap_or(0);
                    let dst_cc = cc_map.get(dst_name).copied().unwrap_or(0);

                    callees
                        .entry(src_name.to_string())
                        .or_default()
                        .push((dst_name.to_string(), dst_cc));
                    callers
                        .entry(dst_name.to_string())
                        .or_default()
                        .push((src_name.to_string(), src_cc));
                }

                // Sort caller/callee lists by name
                for v in callers.values_mut() {
                    v.sort_by(|a, b| a.0.cmp(&b.0));
                    v.dedup_by(|a, b| a.0 == b.0);
                }
                for v in callees.values_mut() {
                    v.sort_by(|a, b| a.0.cmp(&b.0));
                    v.dedup_by(|a, b| a.0 == b.0);
                }

                let external_nodes = cg.external_nodes.clone();

                sort_graph_func_names(
                    &mut func_names,
                    &cc_map,
                    app.graph_sort_col,
                    app.graph_sort_asc,
                );

                let name_to_index: HashMap<String, usize> = func_names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (n.clone(), i))
                    .collect();

                let has_funcs = !func_names.is_empty();
                app.graph_tab_data = Some(GraphTabData {
                    func_names,
                    callers,
                    callees,
                    name_to_index,
                    cc_map,
                    external_nodes,
                });
                if has_funcs {
                    app.graph_func_list_state.select(Some(0));
                    if app.graph_canvas_selected_node.is_none() {
                        if let Some(data) = &app.graph_tab_data {
                            app.graph_canvas_selected_node = data.func_names.first().cloned();
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sorting
// ---------------------------------------------------------------------------

fn sort_report_rows(rows: &mut [ReportRow], col: ReportSortCol, asc: bool) {
    rows.sort_by(|a, b| {
        let ord = match col {
            ReportSortCol::Name => a.name.cmp(&b.name),
            ReportSortCol::McC => a.cc.cmp(&b.cc),
            ReportSortCol::Cognitive => a.cognitive.cmp(&b.cognitive),
            ReportSortCol::Nesting => a.nesting.cmp(&b.nesting),
            ReportSortCol::Loc => a.loc.cmp(&b.loc),
            ReportSortCol::Params => a.params.cmp(&b.params),
            ReportSortCol::FanIn => a.fan_in.cmp(&b.fan_in),
            ReportSortCol::FanOut => a.fan_out.cmp(&b.fan_out),
            ReportSortCol::Rfc => a.rfc.cmp(&b.rfc),
            ReportSortCol::Depth => a.depth.cmp(&b.depth),
            ReportSortCol::TLoc => a.t_loc.cmp(&b.t_loc),
            ReportSortCol::TMcC => a.t_cc.cmp(&b.t_cc),
            ReportSortCol::MI => a.mi.partial_cmp(&b.mi).unwrap_or(std::cmp::Ordering::Equal),
            ReportSortCol::Returns => a.returns.cmp(&b.returns),
            ReportSortCol::BoolExpr => a.bool_expr.cmp(&b.bool_expr),
            ReportSortCol::Lines => a.lines.cmp(&b.lines),
        };
        if asc { ord } else { ord.reverse() }
    });
}

fn sort_module_rows(rows: &mut [ModuleSummaryRow], col: ModuleSortCol, asc: bool) {
    rows.sort_by(|a, b| {
        let ord = match col {
            ModuleSortCol::Module => a.module.cmp(&b.module),
            ModuleSortCol::Funcs => a.func_count.cmp(&b.func_count),
            ModuleSortCol::TLoc => a.total_loc.cmp(&b.total_loc),
            ModuleSortCol::AvgMcC => a
                .avg_cc
                .partial_cmp(&b.avg_cc)
                .unwrap_or(std::cmp::Ordering::Equal),
            ModuleSortCol::MaxMcC => a.max_cc.cmp(&b.max_cc),
            ModuleSortCol::CrossMod => a.cross_module.cmp(&b.cross_module),
            ModuleSortCol::Dead => a.dead_count.cmp(&b.dead_count),
            ModuleSortCol::Ca => a.ca.cmp(&b.ca),
            ModuleSortCol::Ce => a.ce.cmp(&b.ce),
            ModuleSortCol::Instability => a
                .instability
                .partial_cmp(&b.instability)
                .unwrap_or(std::cmp::Ordering::Equal),
        };
        if asc { ord } else { ord.reverse() }
    });
}

fn sort_graph_func_names(
    names: &mut [String],
    cc_map: &HashMap<String, usize>,
    col: GraphSortCol,
    asc: bool,
) {
    names.sort_by(|a, b| {
        let ord = match col {
            GraphSortCol::Name => a.cmp(b),
            GraphSortCol::McC => {
                let a_cc = cc_map.get(a).copied().unwrap_or(0);
                let b_cc = cc_map.get(b).copied().unwrap_or(0);
                a_cc.cmp(&b_cc).then_with(|| a.cmp(b))
            }
        };
        if asc { ord } else { ord.reverse() }
    });
}

fn resort_graph(app: &mut App) {
    let data = match &mut app.graph_tab_data {
        Some(d) => d,
        None => return,
    };
    sort_graph_func_names(
        &mut data.func_names,
        &data.cc_map,
        app.graph_sort_col,
        app.graph_sort_asc,
    );
    data.name_to_index = data
        .func_names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), i))
        .collect();
    app.graph_func_list_state.select(Some(0));
    app.graph_callers_state.select(None);
    app.graph_callees_state.select(None);
    app.graph_nav_history.clear();
}

// ---------------------------------------------------------------------------
// Key handling — main
// ---------------------------------------------------------------------------

/// Returns true if the app should quit.
fn handle_key(app: &mut App, key: event::KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,

        // Tab switching by number
        KeyCode::Char('1') => switch_tab(app, Tab::Report),
        KeyCode::Char('2') => switch_tab(app, Tab::Modules),
        KeyCode::Char('3') => switch_tab(app, Tab::DeadCode),
        KeyCode::Char('4') => switch_tab(app, Tab::Cycles),
        KeyCode::Char('5') => switch_tab(app, Tab::Graph),
        KeyCode::Char('0') => switch_tab(app, Tab::Home),

        // Cmd+Left / Cmd+Right for tab switching
        KeyCode::Left if key.modifiers.contains(KeyModifiers::SUPER) => {
            switch_tab(app, app.active_tab.prev());
        }
        KeyCode::Right if key.modifiers.contains(KeyModifiers::SUPER) => {
            switch_tab(app, app.active_tab.next());
        }

        _ if app.active_tab == Tab::Graph => {
            return handle_key_graph(app, key);
        }

        // Tab/Shift+Tab cycle
        KeyCode::Tab => {
            let next = if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.active_tab.prev()
            } else {
                app.active_tab.next()
            };
            switch_tab(app, next);
        }

        // Left/Right for column highlight (Report & Modules tabs only)
        KeyCode::Left => move_col(app, -1),
        KeyCode::Right => move_col(app, 1),

        // Cmd+Up/Down: jump ~75% of visible rows
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SUPER) => {
            let page = ((app.last_terminal_height.saturating_sub(5)) as f64 * 0.75) as i32;
            move_cursor(app, -page);
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SUPER) => {
            let page = ((app.last_terminal_height.saturating_sub(5)) as f64 * 0.75) as i32;
            move_cursor(app, page);
        }

        // Row navigation
        KeyCode::Char('j') | KeyCode::Down => move_cursor(app, 1),
        KeyCode::Char('k') | KeyCode::Up => move_cursor(app, -1),
        KeyCode::PageDown => move_cursor(app, 20),
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => move_cursor(app, 20),
        KeyCode::PageUp => move_cursor(app, -20),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            move_cursor(app, -20)
        }
        KeyCode::Char('g') | KeyCode::Home => move_to(app, 0),
        KeyCode::Char('G') | KeyCode::End => move_to_end(app),

        // Sort by highlighted column
        KeyCode::Char('s') => sort_by_selected(app),

        // Enter: view source code
        KeyCode::Enter => open_source_view(app),

        _ => {}
    }
    false
}

// ---------------------------------------------------------------------------
// Key handling — source view overlay
// ---------------------------------------------------------------------------

/// Returns true if the app should quit.
fn handle_key_source_view(app: &mut App, key: event::KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
            app.source_view = None;
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SUPER) => {
            scroll_source_page(app, 0.75);
        }
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SUPER) => {
            scroll_source_page(app, -0.75);
        }
        KeyCode::Char('j') | KeyCode::Down => scroll_source(app, 1),
        KeyCode::Char('k') | KeyCode::Up => scroll_source(app, -1),
        KeyCode::PageDown => scroll_source(app, 20),
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            scroll_source(app, 20)
        }
        KeyCode::PageUp => scroll_source(app, -20),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            scroll_source(app, -20)
        }
        KeyCode::Char('g') | KeyCode::Home => {
            if let Some(sv) = &mut app.source_view {
                sv.scroll = 0;
            }
        }
        KeyCode::Char('G') | KeyCode::End => {
            if let Some(sv) = &mut app.source_view {
                sv.scroll = sv.highlighted_lines.len().saturating_sub(1);
            }
        }
        _ => {}
    }
    false
}

fn scroll_source(app: &mut App, delta: i32) {
    if let Some(sv) = &mut app.source_view {
        let max = sv.highlighted_lines.len().saturating_sub(1);
        let current = sv.scroll as i32;
        sv.scroll = (current + delta).clamp(0, max as i32) as usize;
    }
}

fn scroll_source_page(app: &mut App, fraction: f64) {
    if let Some(sv) = &mut app.source_view {
        let delta = (sv.visible_height as f64 * fraction) as i32;
        let max = sv.highlighted_lines.len().saturating_sub(sv.visible_height);
        let current = sv.scroll as i32;
        sv.scroll = (current + delta).clamp(0, max as i32) as usize;
    }
}

// ---------------------------------------------------------------------------
// Tab switching
// ---------------------------------------------------------------------------

fn switch_tab(app: &mut App, tab: Tab) {
    if app.active_tab == tab {
        return;
    }
    app.active_tab = tab;
    ensure_tab_data(app);
    match tab {
        Tab::Home => {}
        Tab::Report => app.report_table_state.select(Some(0)),
        Tab::Modules => app.module_table_state.select(Some(0)),
        Tab::DeadCode => {
            if app.dead_rows.as_ref().is_some_and(|r| !r.is_empty()) {
                app.dead_table_state.select(Some(0));
            }
        }
        Tab::Cycles => {
            if app.cycle_groups.as_ref().is_some_and(|c| !c.is_empty()) {
                app.cycle_list_state.select(Some(0));
            }
        }
        Tab::Graph => {
            app.graph_focus = GraphFocus::FuncList;
            if app
                .graph_tab_data
                .as_ref()
                .is_some_and(|d| !d.func_names.is_empty())
            {
                app.graph_func_list_state.select(Some(0));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Row navigation
// ---------------------------------------------------------------------------

fn row_count(app: &App) -> usize {
    match app.active_tab {
        Tab::Home => 0,
        Tab::Report => app.report_rows.as_ref().map_or(0, |r| r.len()),
        Tab::Modules => app.module_rows.as_ref().map_or(0, |r| r.len()),
        Tab::DeadCode => app.dead_rows.as_ref().map_or(0, |r| r.len()),
        Tab::Cycles => app.cycle_groups.as_ref().map_or(0, |c| c.len()),
        Tab::Graph => graph_row_count(app),
    }
}

fn graph_row_count(app: &App) -> usize {
    let data = match &app.graph_tab_data {
        Some(d) => d,
        None => return 0,
    };
    match app.graph_focus {
        GraphFocus::Canvas => 0, // canvas uses spatial navigation, not row count
        GraphFocus::FuncList => data.func_names.len(),
        GraphFocus::Callers => {
            let sel = graph_selected_func_name(app);
            sel.and_then(|n| data.callers.get(&n))
                .map_or(0, |v| v.len())
        }
        GraphFocus::Callees => {
            let sel = graph_selected_func_name(app);
            sel.and_then(|n| data.callees.get(&n))
                .map_or(0, |v| v.len())
        }
    }
}

fn graph_selected_func_name(app: &App) -> Option<String> {
    let data = app.graph_tab_data.as_ref()?;
    let sel = app.graph_func_list_state.selected()?;
    data.func_names.get(sel).cloned()
}

fn current_selection(app: &App) -> Option<usize> {
    match app.active_tab {
        Tab::Home => None,
        Tab::Report => app.report_table_state.selected(),
        Tab::Modules => app.module_table_state.selected(),
        Tab::DeadCode => app.dead_table_state.selected(),
        Tab::Cycles => app.cycle_list_state.selected(),
        Tab::Graph => match app.graph_focus {
            GraphFocus::Canvas => None, // canvas uses spatial selection
            GraphFocus::FuncList => app.graph_func_list_state.selected(),
            GraphFocus::Callers => app.graph_callers_state.selected(),
            GraphFocus::Callees => app.graph_callees_state.selected(),
        },
    }
}

fn set_selection(app: &mut App, idx: usize) {
    match app.active_tab {
        Tab::Home => {}
        Tab::Report => app.report_table_state.select(Some(idx)),
        Tab::Modules => app.module_table_state.select(Some(idx)),
        Tab::DeadCode => app.dead_table_state.select(Some(idx)),
        Tab::Cycles => app.cycle_list_state.select(Some(idx)),
        Tab::Graph => match app.graph_focus {
            GraphFocus::Canvas => {} // canvas uses spatial selection
            GraphFocus::FuncList => app.graph_func_list_state.select(Some(idx)),
            GraphFocus::Callers => app.graph_callers_state.select(Some(idx)),
            GraphFocus::Callees => app.graph_callees_state.select(Some(idx)),
        },
    }
}

fn move_cursor(app: &mut App, delta: i32) {
    let count = row_count(app);
    if count == 0 {
        return;
    }
    let current = current_selection(app).unwrap_or(0) as i32;
    let next = (current + delta).clamp(0, count as i32 - 1) as usize;
    set_selection(app, next);
}

fn move_to(app: &mut App, idx: usize) {
    let count = row_count(app);
    if count == 0 {
        return;
    }
    set_selection(app, idx.min(count - 1));
}

fn move_to_end(app: &mut App) {
    let count = row_count(app);
    if count == 0 {
        return;
    }
    set_selection(app, count - 1);
}

// ---------------------------------------------------------------------------
// Column highlight navigation
// ---------------------------------------------------------------------------

fn col_count(app: &App) -> usize {
    match app.active_tab {
        Tab::Report => ReportSortCol::all().len(),
        Tab::Modules => ModuleSortCol::all().len(),
        _ => 0,
    }
}

fn move_col(app: &mut App, delta: i32) {
    let count = col_count(app) as i32;
    if count == 0 {
        return;
    }
    match app.active_tab {
        Tab::Report => {
            let next = (app.report_selected_col as i32 + delta).rem_euclid(count) as usize;
            app.report_selected_col = next;
        }
        Tab::Modules => {
            let next = (app.module_selected_col as i32 + delta).rem_euclid(count) as usize;
            app.module_selected_col = next;
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Sort by highlighted column
// ---------------------------------------------------------------------------

fn sort_by_selected(app: &mut App) {
    match app.active_tab {
        Tab::Report => {
            let col = ReportSortCol::from_index(app.report_selected_col);
            if col == app.report_sort_col {
                app.report_sort_asc = !app.report_sort_asc;
            } else {
                app.report_sort_col = col;
                app.report_sort_asc = col == ReportSortCol::Name;
            }
            if let Some(ref mut rows) = app.report_rows {
                sort_report_rows(rows, app.report_sort_col, app.report_sort_asc);
            }
            app.report_table_state.select(Some(0));
        }
        Tab::Modules => {
            let col = ModuleSortCol::from_index(app.module_selected_col);
            if col == app.module_sort_col {
                app.module_sort_asc = !app.module_sort_asc;
            } else {
                app.module_sort_col = col;
                app.module_sort_asc = col == ModuleSortCol::Module;
            }
            if let Some(ref mut rows) = app.module_rows {
                sort_module_rows(rows, app.module_sort_col, app.module_sort_asc);
            }
            app.module_table_state.select(Some(0));
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Source view
// ---------------------------------------------------------------------------

fn open_source_view(app: &mut App) {
    let func_name = match app.active_tab {
        Tab::Report => {
            let sel = match app.report_table_state.selected() {
                Some(s) => s,
                None => return,
            };
            match app.report_rows.as_ref().and_then(|r| r.get(sel)) {
                Some(r) => r.name.clone(),
                None => return,
            }
        }
        Tab::DeadCode => {
            let sel = match app.dead_table_state.selected() {
                Some(s) => s,
                None => return,
            };
            match app.dead_rows.as_ref().and_then(|r| r.get(sel)) {
                Some(r) => r.name.clone(),
                None => return,
            }
        }
        Tab::Graph => match graph_selected_func_name(app) {
            Some(name) => name,
            None => return,
        },
        _ => return,
    };

    let analysis = match &app.analysis {
        Some(a) => a,
        None => return,
    };

    let func = analysis
        .module
        .functions
        .iter()
        .find(|f| f.qualified_name() == func_name);
    let func = match func {
        Some(f) => f,
        None => return,
    };

    let source_file = &func.source_file;
    let start = func.start_line;
    let end = func.end_line;

    let content = match fs::read_to_string(source_file) {
        Ok(c) => c,
        Err(e) => {
            app.source_view = Some(SourceView {
                title: func_name,
                highlighted_lines: vec![Line::from(format!(
                    "Error reading {}: {}",
                    source_file, e
                ))],
                scroll: 0,
                visible_height: 0,
            });
            return;
        }
    };

    let all_lines: Vec<&str> = content.lines().collect();
    let display_start = start.saturating_sub(1).min(all_lines.len());
    let display_end = end.min(all_lines.len());

    let ss = syntect::parsing::SyntaxSet::load_defaults_newlines();
    let ts = syntect::highlighting::ThemeSet::load_defaults();
    let theme = &ts.themes["base16-ocean.dark"];
    let syntax = ss
        .find_syntax_by_extension("py")
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut h = syntect::easy::HighlightLines::new(syntax, theme);

    // Warm up the highlighter so parser state (strings, comments, etc.) is correct.
    for line in &all_lines[..display_start] {
        let _ = h.highlight_line(&format!("{}\n", line), &ss);
    }

    let mut highlighted_lines = Vec::new();
    for (i, text) in all_lines[display_start..display_end].iter().enumerate() {
        let line_num = display_start + i + 1;
        let mut spans = vec![Span::styled(
            format!("{:>4} ", line_num),
            Style::default().fg(Color::DarkGray),
        )];
        if let Ok(regions) = h.highlight_line(&format!("{}\n", text), &ss) {
            for (style, segment) in regions {
                spans.push(Span::styled(segment.to_string(), syntect_to_ratatui(style)));
            }
        } else {
            spans.push(Span::raw(text.to_string()));
        }
        highlighted_lines.push(Line::from(spans));
    }

    app.source_view = Some(SourceView {
        title: func_name,
        highlighted_lines,
        scroll: 0,
        visible_height: 0,
    });
}

// ---------------------------------------------------------------------------
// Rendering — main frame
// ---------------------------------------------------------------------------

fn render(f: &mut ratatui::Frame, app: &mut App) {
    app.last_terminal_height = f.area().height;

    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(f.area());

    // Tab bar
    let titles = vec![
        "0:Home",
        "1:Report",
        "2:Modules",
        "3:Dead Code",
        "4:Cycles",
        "5:Graph",
    ];
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title(" PyMapper "))
        .select(app.active_tab.index())
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, chunks[0]);

    // Content area
    match app.active_tab {
        Tab::Home => render_home(f, app, chunks[1]),
        Tab::Report => {
            if app.analysis.is_none() {
                render_loading(f, app, chunks[1]);
            } else {
                render_report(f, app, chunks[1]);
            }
        }
        Tab::Modules => {
            if app.analysis.is_none() {
                render_loading(f, app, chunks[1]);
            } else {
                render_modules(f, app, chunks[1]);
            }
        }
        Tab::DeadCode => {
            if app.analysis.is_none() {
                render_loading(f, app, chunks[1]);
            } else {
                render_dead_code(f, app, chunks[1]);
            }
        }
        Tab::Cycles => {
            if app.analysis.is_none() {
                render_loading(f, app, chunks[1]);
            } else {
                render_cycles(f, app, chunks[1]);
            }
        }
        Tab::Graph => {
            if app.analysis.is_none() {
                render_loading(f, app, chunks[1]);
            } else {
                render_graph(f, app, chunks[1]);
            }
        }
    }

    // Status bar
    let status = build_status_bar(app);
    f.render_widget(
        Paragraph::new(status).style(Style::default().bg(Color::DarkGray).fg(Color::White)),
        chunks[2],
    );

    // Source view overlay (rendered last, on top)
    if app.source_view.is_some() {
        render_source_view(f, app, f.area());
    }

    // Tree overlay (rendered on top of everything)
    if app.tree_overlay.is_some() {
        render_tree_overlay(f, app, f.area());
    }
}

fn build_status_bar(app: &App) -> Line<'static> {
    if app.tree_overlay.is_some() {
        return Line::from(vec![
            Span::styled(" Esc", Style::default().fg(Color::Yellow)),
            Span::raw(":close  "),
            Span::styled("j/k", Style::default().fg(Color::Yellow)),
            Span::raw(":move  "),
            Span::styled("d", Style::default().fg(Color::Yellow)),
            Span::raw(":toggle dir  "),
            Span::styled("+/-", Style::default().fg(Color::Yellow)),
            Span::raw(":depth  "),
            Span::styled("Space", Style::default().fg(Color::Yellow)),
            Span::raw(":collapse  "),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw(":re-root"),
        ]);
    }

    if app.source_view.is_some() {
        return Line::from(vec![
            Span::styled(" Esc", Style::default().fg(Color::Yellow)),
            Span::raw(":close  "),
            Span::styled("j/k", Style::default().fg(Color::Yellow)),
            Span::raw(":scroll  "),
            Span::styled("g/G", Style::default().fg(Color::Yellow)),
            Span::raw(":top/bottom"),
        ]);
    }

    let sort_hint = match app.active_tab {
        Tab::Report => {
            let dir = if app.report_sort_asc { "asc" } else { "desc" };
            format!("  sort:{} {}", app.report_sort_col.label(), dir)
        }
        Tab::Modules => {
            let dir = if app.module_sort_asc { "asc" } else { "desc" };
            format!("  sort:{} {}", app.module_sort_col.label(), dir)
        }
        _ => String::new(),
    };

    let mut spans = vec![
        Span::styled(" q", Style::default().fg(Color::Yellow)),
        Span::raw(":quit  "),
    ];

    if app.active_tab == Tab::Graph {
        spans.extend([
            Span::styled("Tab", Style::default().fg(Color::Yellow)),
            Span::raw(":focus  "),
            Span::styled("j/k", Style::default().fg(Color::Yellow)),
            Span::raw(":move  "),
            Span::styled("s", Style::default().fg(Color::Yellow)),
            Span::raw(":sort  "),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw(":jump/source  "),
            Span::styled("BS", Style::default().fg(Color::Yellow)),
            Span::raw(":back  "),
            Span::styled("/", Style::default().fg(Color::Yellow)),
            Span::raw(":search  "),
            Span::styled("t", Style::default().fg(Color::Yellow)),
            Span::raw(":tree"),
        ]);
        let dir = if app.graph_sort_asc { "asc" } else { "desc" };
        spans.push(Span::styled(
            format!("  sort:{} {}", app.graph_sort_col.label(), dir),
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        spans.extend([
            Span::styled("Tab", Style::default().fg(Color::Yellow)),
            Span::raw(":switch  "),
            Span::styled("j/k", Style::default().fg(Color::Yellow)),
            Span::raw(":move"),
        ]);

        if matches!(app.active_tab, Tab::Report | Tab::Modules) {
            spans.extend([
                Span::raw("  "),
                Span::styled("\u{2190}/\u{2192}", Style::default().fg(Color::Yellow)),
                Span::raw(":col  "),
                Span::styled("s", Style::default().fg(Color::Yellow)),
                Span::raw(":sort"),
            ]);
        }

        if matches!(app.active_tab, Tab::Report | Tab::DeadCode) {
            spans.extend([
                Span::raw("  "),
                Span::styled("Enter", Style::default().fg(Color::Yellow)),
                Span::raw(":source"),
            ]);
        }
    }

    if !sort_hint.is_empty() {
        spans.push(Span::styled(
            sort_hint,
            Style::default().fg(Color::DarkGray),
        ));
    }

    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Rendering — loading state (used by non-Home tabs)
// ---------------------------------------------------------------------------

fn render_loading(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let bar_count = progress_bar_count(app);
    let inner_chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(bar_count as u16),
        Constraint::Min(0),
    ])
    .split(area);

    render_progress_bars(f, app, inner_chunks[1]);
}

// ---------------------------------------------------------------------------
// Rendering — stacked progress bars
// ---------------------------------------------------------------------------

fn progress_bar_count(app: &App) -> usize {
    let mut count = 1; // walking bar always visible
    if app.walk_complete {
        count += 1; // parsing bar
    }
    if app.parse_complete {
        count += 1; // build graph bar
    }
    if app.graph_complete {
        count += 1; // compute metrics bar
    }
    if app.metrics_complete {
        count += 1; // build cycle graph bar
    }
    count
}

fn format_elapsed(d: Duration) -> String {
    format!("{:.6}s", d.as_secs_f64())
}

fn render_progress_bars(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let bar_count = progress_bar_count(app);
    let constraints: Vec<Constraint> = (0..bar_count).map(|_| Constraint::Length(1)).collect();
    let slots = Layout::vertical(constraints).split(area);

    let active_style = Style::default().fg(Color::Cyan).bg(Color::Black);
    let done_style = Style::default().fg(Color::Green).bg(Color::Black);

    // Walking bar (slot 0)
    let walk_gauge = if app.walk_complete {
        Gauge::default()
            .gauge_style(done_style)
            .ratio(1.0)
            .label(format!(
                "Walked \u{2713} ({})",
                format_elapsed(app.walk_elapsed)
            ))
    } else {
        Gauge::default()
            .gauge_style(active_style)
            .ratio(1.0)
            .label(format!("Walking... {} files found", app.walk_discovered))
    };
    f.render_widget(walk_gauge, slots[0]);

    // Parsing bar (slot 1, if visible)
    if app.walk_complete && slots.len() > 1 {
        let parse_gauge = if app.parse_complete {
            Gauge::default()
                .gauge_style(done_style)
                .ratio(1.0)
                .label(format!(
                    "Parsed {} files \u{2713} ({})",
                    app.file_count,
                    format_elapsed(app.parse_elapsed)
                ))
        } else {
            let ratio = if app.file_count > 0 {
                (app.parse_done as f64 / app.file_count as f64).min(1.0)
            } else {
                0.0
            };
            Gauge::default()
                .gauge_style(active_style)
                .ratio(ratio)
                .label(format!(
                    "Parsing {}/{} files",
                    app.parse_done, app.file_count
                ))
        };
        f.render_widget(parse_gauge, slots[1]);
    }

    // Build graph bar (slot 2, if visible)
    if app.parse_complete && slots.len() > 2 {
        let gauge = if app.graph_complete {
            Gauge::default()
                .gauge_style(done_style)
                .ratio(1.0)
                .label(format!(
                    "Call graph ({} funcs) \u{2713} ({})",
                    app.graph_total,
                    format_elapsed(app.graph_elapsed)
                ))
        } else {
            let ratio = if app.graph_total > 0 {
                (app.graph_done as f64 / app.graph_total as f64).min(1.0)
            } else {
                0.0
            };
            Gauge::default()
                .gauge_style(active_style)
                .ratio(ratio)
                .label(format!(
                    "Building call graph {}/{}",
                    app.graph_done, app.graph_total
                ))
        };
        f.render_widget(gauge, slots[2]);
    }

    // Compute metrics bar (slot 3, if visible)
    if app.graph_complete && slots.len() > 3 {
        let gauge = if app.metrics_complete {
            Gauge::default()
                .gauge_style(done_style)
                .ratio(1.0)
                .label(format!(
                    "Metrics ({} funcs) \u{2713} ({})",
                    app.metrics_total,
                    format_elapsed(app.metrics_elapsed)
                ))
        } else {
            let ratio = if app.metrics_total > 0 {
                (app.metrics_done as f64 / app.metrics_total as f64).min(1.0)
            } else {
                0.0
            };
            Gauge::default()
                .gauge_style(active_style)
                .ratio(ratio)
                .label(format!(
                    "Computing metrics {}/{}",
                    app.metrics_done, app.metrics_total
                ))
        };
        f.render_widget(gauge, slots[3]);
    }

    // Build cycle graph bar (slot 4, if visible)
    if app.metrics_complete && slots.len() > 4 {
        let gauge = if app.cycle_graph_complete {
            Gauge::default()
                .gauge_style(done_style)
                .ratio(1.0)
                .label(format!(
                    "Cycle graph ({} funcs) \u{2713} ({})",
                    app.cycle_graph_total,
                    format_elapsed(app.cycle_graph_elapsed)
                ))
        } else {
            let ratio = if app.cycle_graph_total > 0 {
                (app.cycle_graph_done as f64 / app.cycle_graph_total as f64).min(1.0)
            } else {
                0.0
            };
            Gauge::default()
                .gauge_style(active_style)
                .ratio(ratio)
                .label(format!(
                    "Building cycle graph {}/{}",
                    app.cycle_graph_done, app.cycle_graph_total
                ))
        };
        f.render_widget(gauge, slots[4]);
    }
}

// ---------------------------------------------------------------------------
// Rendering — Home tab
// ---------------------------------------------------------------------------

const BANNER_HEIGHT: u16 = 6;

const BANNER_SNAKE_COL: usize = 19;

fn build_banner_lines(width: u16) -> Vec<Line<'static>> {
    let lines: &[&str] = &[
        r"                    ____        __  __",
        r"    _____          |  _ \ _   _|  \/  | __ _ _ __  _ __   ___ _ __",
        r"   /     \         | |_) | | | | |\/| |/ _` | '_ \| '_ \ / _ \ '__|",
        r"  | () () |        |  __/| |_| | |  | | (_| | |_) | |_) |  __/ |",
        r"   \  ~  /         |_|    \__, |_|  |_|\__,_| .__/| .__/ \___|_|",
        r"    |||||                   |___/             |_|   |_|",
    ];

    let max_w = lines.iter().map(|l| l.len()).max().unwrap_or(0);
    let pad = ((width as usize).saturating_sub(max_w)) / 2;
    let padding = " ".repeat(pad);

    let top = (0.0f32, 220.0, 255.0);
    let bot = (220.0, 60.0, 255.0);
    let num = lines.len() as f32;

    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let vy = i as f32 / (num - 1.0);
            let snake_end = BANNER_SNAKE_COL.min(line.len());
            let snake_part = &line[..snake_end];
            let text_part = if line.len() > BANNER_SNAKE_COL {
                &line[BANNER_SNAKE_COL..]
            } else {
                ""
            };

            let mut spans = vec![Span::raw(padding.clone())];

            // Snake face: green structure, yellow eyes, red tongue
            for ch in snake_part.chars() {
                let style = match ch {
                    '(' | ')' => Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                    '~' => Style::default().fg(Color::Rgb(255, 80, 80)),
                    ' ' => Style::default(),
                    _ => Style::default().fg(Color::Rgb(80, 255, 80)),
                };
                spans.push(Span::styled(ch.to_string(), style));
            }

            // Text: per-character 2D gradient (cyan top-left → magenta bottom-right)
            let text_len = text_part.len() as f32;
            for (j, ch) in text_part.chars().enumerate() {
                if ch == ' ' {
                    spans.push(Span::raw(" "));
                } else {
                    let vx = if text_len > 1.0 {
                        j as f32 / (text_len - 1.0)
                    } else {
                        0.0
                    };
                    let t = (vy * 0.7 + vx * 0.3).min(1.0);
                    let r = (top.0 + t * (bot.0 - top.0)) as u8;
                    let g = (top.1 + t * (bot.1 - top.1)) as u8;
                    let b = (top.2 + t * (bot.2 - top.2)) as u8;
                    spans.push(Span::styled(
                        ch.to_string(),
                        Style::default().fg(Color::Rgb(r, g, b)),
                    ));
                }
            }

            Line::from(spans)
        })
        .collect()
}

fn render_home(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" Home ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let log_height = if app.log_messages.is_empty() {
        0
    } else {
        (app.log_messages.len() as u16 + 2)
            .min(inner.height / 3)
            .max(3)
    };

    let bar_count = progress_bar_count(app) as u16;

    if app.analysis.is_none() {
        // --- Loading state ---
        let chunks = Layout::vertical([
            Constraint::Length(1),             // top padding
            Constraint::Length(BANNER_HEIGHT), // ASCII art
            Constraint::Length(1),             // spacer
            Constraint::Length(1),             // scanning paths
            Constraint::Min(0),                // flexible spacer
            Constraint::Length(log_height),    // log area
            Constraint::Length(bar_count),     // progress bars
        ])
        .split(inner);

        // ASCII art
        let art = Paragraph::new(build_banner_lines(inner.width));
        f.render_widget(art, chunks[1]);

        // Scanning paths
        let paths_text = Paragraph::new(Line::from(vec![
            Span::styled("  Scanning: ", Style::default().fg(Color::DarkGray)),
            Span::styled(app.paths.join(", "), Style::default().fg(Color::DarkGray)),
        ]));
        f.render_widget(paths_text, chunks[3]);

        // Log area
        if !app.log_messages.is_empty() {
            render_log_area(f, app, chunks[5]);
        }

        // Progress bars at bottom
        render_progress_bars(f, app, chunks[6]);

        return;
    }

    // --- Post-load state ---
    let home = match &app.home_data {
        Some(h) => h,
        None => return,
    };

    let chunks = Layout::vertical([
        Constraint::Length(1),             // top padding
        Constraint::Length(BANNER_HEIGHT), // ASCII art
        Constraint::Length(1),             // spacer
        Constraint::Length(1),             // scanning paths
        Constraint::Length(1),             // separator
        Constraint::Length(6),             // overview
        Constraint::Length(1),             // separator
        Constraint::Length(7),             // complexity
        Constraint::Min(0),                // flexible spacer
        Constraint::Length(log_height),    // log area
        Constraint::Length(bar_count),     // progress bars
    ])
    .split(inner);

    // ASCII art
    let art = Paragraph::new(build_banner_lines(inner.width));
    f.render_widget(art, chunks[1]);

    // Scanning paths
    let paths_text = Paragraph::new(Line::from(vec![
        Span::styled("  Scanning: ", Style::default().fg(Color::DarkGray)),
        Span::styled(home.paths.join(", "), Style::default().fg(Color::DarkGray)),
    ]));
    f.render_widget(paths_text, chunks[3]);

    // Overview section
    let overview_lines = vec![
        Line::from(vec![Span::styled(
            "  Overview",
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::White),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::raw("    Files       "),
            Span::styled(
                home.file_count.to_string(),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::raw("    Functions   "),
            Span::styled(
                home.func_count.to_string(),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::raw("    Classes     "),
            Span::styled(
                home.class_count.to_string(),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::raw("    Modules     "),
            Span::styled(
                home.module_count.to_string(),
                Style::default().fg(Color::Cyan),
            ),
        ]),
    ];
    f.render_widget(Paragraph::new(overview_lines), chunks[5]);

    // Complexity section
    let max_cc_style = if home.max_cc >= 10 {
        Style::default().fg(Color::Red)
    } else if home.max_cc >= 5 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Green)
    };

    let dead_style = if home.dead_count > 0 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Green)
    };

    let cycle_style = if home.cycle_count > 0 {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    };

    let complexity_lines = vec![
        Line::from(vec![Span::styled(
            "  Complexity",
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::White),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::raw("    Total LOC   "),
            Span::styled(home.total_loc.to_string(), Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::raw("    Avg McC     "),
            Span::styled(
                format!("{:.2}", home.avg_cc),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::raw("    Max McC     "),
            Span::styled(home.max_cc.to_string(), max_cc_style),
            Span::raw("  "),
            Span::styled(&home.max_cc_func, Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(vec![
            Span::raw("    Dead Funcs  "),
            Span::styled(home.dead_count.to_string(), dead_style),
        ]),
        Line::from(vec![
            Span::raw("    Cycles      "),
            Span::styled(home.cycle_count.to_string(), cycle_style),
        ]),
    ];
    f.render_widget(Paragraph::new(complexity_lines), chunks[7]);

    // Log area
    if !app.log_messages.is_empty() {
        render_log_area(f, app, chunks[9]);
    }

    // Progress bars at bottom
    render_progress_bars(f, app, chunks[10]);
}

fn render_log_area(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Warnings ")
        .style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let visible = inner.height as usize;
    let skip = app.log_messages.len().saturating_sub(visible);
    let items: Vec<ListItem> = app
        .log_messages
        .iter()
        .skip(skip)
        .map(|msg| {
            ListItem::new(Line::from(vec![
                Span::styled("warning: ", Style::default().fg(Color::Yellow)),
                Span::styled(msg.as_str(), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let list = List::new(items);
    f.render_widget(list, inner);
}

// ---------------------------------------------------------------------------
// Windowed row rendering helper
// ---------------------------------------------------------------------------

/// Computes the visible window of rows and builds Row widgets only for visible rows.
/// Returns (windowed rows, real offset). After `render_stateful_widget`, call
/// `restore_table_state` to put the global selected/offset back.
fn windowed_rows<'a, T>(
    rows_data: &'a [T],
    table_state: &mut TableState,
    area_height: u16,
    build_row: impl Fn(&'a T) -> Row<'a>,
) -> (Vec<Row<'a>>, usize) {
    // 3 = top border + header row + bottom border
    let visible_count = (area_height.saturating_sub(3)) as usize;
    if visible_count == 0 || rows_data.is_empty() {
        return (Vec::new(), 0);
    }

    let selected = table_state.selected().unwrap_or(0);
    let mut offset = table_state.offset();

    // Clamp offset so selected stays in view
    if selected < offset {
        offset = selected;
    } else if selected >= offset + visible_count {
        offset = selected + 1 - visible_count;
    }

    let end = (offset + visible_count).min(rows_data.len());
    let rows: Vec<Row> = rows_data[offset..end].iter().map(build_row).collect();

    // Set local indices for rendering — ratatui sees offset=0 and a local selected
    *table_state.offset_mut() = 0;
    table_state.select(Some(selected - offset));

    (rows, offset)
}

/// Restores global selected/offset after windowed rendering.
fn restore_table_state(table_state: &mut TableState, offset: usize) {
    if let Some(local) = table_state.selected() {
        table_state.select(Some(local + offset));
    }
    *table_state.offset_mut() = offset;
}

/// Restores global selected/offset for ListState after windowed rendering.
fn restore_list_state(list_state: &mut ListState, offset: usize) {
    if let Some(local) = list_state.selected() {
        list_state.select(Some(local + offset));
    }
    *list_state.offset_mut() = offset;
}

// ---------------------------------------------------------------------------
// Rendering — Report tab
// ---------------------------------------------------------------------------

fn render_report(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let rows_data = match &app.report_rows {
        Some(r) => r,
        None => return,
    };

    if rows_data.is_empty() {
        let msg =
            Paragraph::new("No functions found.").block(Block::default().borders(Borders::ALL));
        f.render_widget(msg, area);
        return;
    }

    let all_cols = ReportSortCol::all();
    let header_cells: Vec<Cell> = all_cols
        .iter()
        .enumerate()
        .map(|(i, col)| {
            let is_sort = *col == app.report_sort_col;
            let is_selected = i == app.report_selected_col;
            let arrow = if is_sort {
                if app.report_sort_asc {
                    " \u{25b2}"
                } else {
                    " \u{25bc}"
                }
            } else {
                ""
            };
            let label = format!("{}{}", col.label(), arrow);

            let mut style = Style::default().add_modifier(Modifier::BOLD);
            if is_selected {
                style = style.bg(Color::DarkGray).fg(Color::Yellow);
            }
            if is_sort {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            Cell::from(label).style(style)
        })
        .collect();
    let header = Row::new(header_cells)
        .style(Style::default().fg(Color::White))
        .bottom_margin(0);

    let total_count = rows_data.len();
    let (rows, offset) = windowed_rows(rows_data, &mut app.report_table_state, area.height, |r| {
        let cc_style = if r.cc >= 10 {
            Style::default().fg(Color::Red)
        } else if r.cc >= 5 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Green)
        };
        let cog_style = if r.cognitive >= 10 {
            Style::default().fg(Color::Red)
        } else if r.cognitive >= 5 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Green)
        };
        Row::new(vec![
            Cell::from(r.name.as_str()),
            Cell::from(r.loc.to_string()),
            Cell::from(r.t_loc.to_string()),
            Cell::from(r.cc.to_string()).style(cc_style),
            Cell::from(r.t_cc.to_string()),
            Cell::from(r.cognitive.to_string()).style(cog_style),
            Cell::from(r.nesting.to_string()),
            Cell::from(r.params.to_string()),
            Cell::from(r.fan_in.to_string()),
            Cell::from(r.fan_out.to_string()),
            Cell::from(r.rfc.to_string()),
            Cell::from(r.depth.to_string()),
            Cell::from(format!("{:.1}", r.mi)),
            Cell::from(r.returns.to_string()),
            Cell::from(r.bool_expr.to_string()),
            Cell::from(r.lines.as_str()),
        ])
    });

    let widths = [
        Constraint::Min(20),    // Name
        Constraint::Length(5),  // LOC
        Constraint::Length(7),  // T-LOC
        Constraint::Length(6),  // McC
        Constraint::Length(8),  // T-McC
        Constraint::Length(11), // Cognitive
        Constraint::Length(9),  // Nesting
        Constraint::Length(6),  // Params
        Constraint::Length(5),  // FI
        Constraint::Length(5),  // FO
        Constraint::Length(5),  // RFC
        Constraint::Length(7),  // Depth
        Constraint::Length(6),  // MI
        Constraint::Length(5),  // Ret
        Constraint::Length(6),  // Bool
        Constraint::Length(9),  // Lines
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Report ({} functions) ", total_count)),
        )
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("\u{25b6} ");

    f.render_stateful_widget(table, area, &mut app.report_table_state);
    restore_table_state(&mut app.report_table_state, offset);
}

// ---------------------------------------------------------------------------
// Rendering — Modules tab
// ---------------------------------------------------------------------------

fn render_modules(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let rows_data = match &app.module_rows {
        Some(r) => r,
        None => return,
    };

    if rows_data.is_empty() {
        let msg = Paragraph::new("No modules found.").block(Block::default().borders(Borders::ALL));
        f.render_widget(msg, area);
        return;
    }

    let all_cols = ModuleSortCol::all();
    let header_cells: Vec<Cell> = all_cols
        .iter()
        .enumerate()
        .map(|(i, col)| {
            let is_sort = *col == app.module_sort_col;
            let is_selected = i == app.module_selected_col;
            let arrow = if is_sort {
                if app.module_sort_asc {
                    " \u{25b2}"
                } else {
                    " \u{25bc}"
                }
            } else {
                ""
            };
            let label = format!("{}{}", col.label(), arrow);

            let mut style = Style::default().add_modifier(Modifier::BOLD);
            if is_selected {
                style = style.bg(Color::DarkGray).fg(Color::Yellow);
            }
            if is_sort {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            Cell::from(label).style(style)
        })
        .collect();
    let header = Row::new(header_cells).style(Style::default().fg(Color::White));

    let total_count = rows_data.len();
    let (rows, offset) = windowed_rows(rows_data, &mut app.module_table_state, area.height, |r| {
        let dead_style = if r.dead_count > 0 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        Row::new(vec![
            Cell::from(r.module.as_str()),
            Cell::from(r.func_count.to_string()),
            Cell::from(r.total_loc.to_string()),
            Cell::from(format!("{:.2}", r.avg_cc)),
            Cell::from(r.max_cc.to_string()),
            Cell::from(r.cross_module.to_string()),
            Cell::from(r.dead_count.to_string()).style(dead_style),
            Cell::from(r.ca.to_string()),
            Cell::from(r.ce.to_string()),
            Cell::from(format!("{:.2}", r.instability)),
        ])
    });

    let widths = [
        Constraint::Min(15),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Length(6),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Modules ({}) ", total_count)),
        )
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("\u{25b6} ");

    f.render_stateful_widget(table, area, &mut app.module_table_state);
    restore_table_state(&mut app.module_table_state, offset);
}

// ---------------------------------------------------------------------------
// Rendering — Dead Code tab
// ---------------------------------------------------------------------------

fn render_dead_code(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let rows_data = match &app.dead_rows {
        Some(r) => r,
        None => return,
    };

    if rows_data.is_empty() {
        let msg = Paragraph::new("No dead code found.")
            .style(Style::default().fg(Color::Green))
            .block(Block::default().borders(Borders::ALL).title(" Dead Code "));
        f.render_widget(msg, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from("Function").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Module").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Class").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("LOC").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Lines").style(Style::default().add_modifier(Modifier::BOLD)),
    ])
    .style(Style::default().fg(Color::White));

    let total_count = rows_data.len();
    let (rows, offset) = windowed_rows(rows_data, &mut app.dead_table_state, area.height, |r| {
        let class_style = match r.classification.as_str() {
            "candidate" => Style::default().fg(Color::Red),
            "entry-point" => Style::default().fg(Color::Green),
            "override" => Style::default().fg(Color::Cyan),
            "test-fixture" => Style::default().fg(Color::Yellow),
            _ => Style::default(),
        };
        Row::new(vec![
            Cell::from(r.name.as_str()),
            Cell::from(r.module.as_str()),
            Cell::from(r.classification.as_str()).style(class_style),
            Cell::from(r.loc.to_string()),
            Cell::from(r.lines.as_str()),
        ])
    });

    let widths = [
        Constraint::Min(20),
        Constraint::Min(10),
        Constraint::Length(13),
        Constraint::Length(5),
        Constraint::Length(9),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Dead Code ({}) ", total_count)),
        )
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("\u{25b6} ");

    f.render_stateful_widget(table, area, &mut app.dead_table_state);
    restore_table_state(&mut app.dead_table_state, offset);
}

// ---------------------------------------------------------------------------
// Rendering — Cycles tab
// ---------------------------------------------------------------------------

fn render_cycles(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let cycle_data = match &app.cycle_groups {
        Some(c) => c,
        None => return,
    };

    if cycle_data.is_empty() {
        let msg = Paragraph::new("No cycles detected.")
            .style(Style::default().fg(Color::Green))
            .block(Block::default().borders(Borders::ALL).title(" Cycles "));
        f.render_widget(msg, area);
        return;
    }

    let total_count = cycle_data.len();

    // Compute visible window manually (windowed_list_items doesn't carry index)
    let visible_count = (area.height.saturating_sub(2)) as usize;
    let selected = app.cycle_list_state.selected().unwrap_or(0);
    let mut list_offset = app.cycle_list_state.offset();
    if selected < list_offset {
        list_offset = selected;
    } else if visible_count > 0 && selected >= list_offset + visible_count {
        list_offset = selected + 1 - visible_count;
    }
    let end = if visible_count > 0 {
        (list_offset + visible_count).min(cycle_data.len())
    } else {
        cycle_data.len()
    };

    let items: Vec<ListItem> = cycle_data[list_offset..end]
        .iter()
        .enumerate()
        .map(|(i, cycle)| {
            let global_i = list_offset + i;
            let chain: String = cycle
                .iter()
                .chain(std::iter::once(&cycle[0]))
                .cloned()
                .collect::<Vec<_>>()
                .join(" \u{2192} ");
            ListItem::new(format!(
                "Cycle {} ({} functions): {}",
                global_i + 1,
                cycle.len(),
                chain
            ))
        })
        .collect();

    *app.cycle_list_state.offset_mut() = 0;
    app.cycle_list_state.select(Some(selected - list_offset));

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Cycles ({}) ", total_count)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("\u{25b6} ");

    f.render_stateful_widget(list, area, &mut app.cycle_list_state);
    restore_list_state(&mut app.cycle_list_state, list_offset);
}

// ---------------------------------------------------------------------------
// Rendering — Source view overlay
// ---------------------------------------------------------------------------

fn syntect_to_ratatui(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b))
}

fn render_source_view(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let sv = match &mut app.source_view {
        Some(s) => s,
        None => return,
    };

    // Center overlay at ~80% of screen
    let popup = centered_rect(90, 85, area);

    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", sv.title))
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let visible_height = inner.height as usize;
    sv.visible_height = visible_height;
    let max_scroll = sv.highlighted_lines.len().saturating_sub(visible_height);
    let scroll = sv.scroll.min(max_scroll);

    let visible_lines: Vec<Line> = sv
        .highlighted_lines
        .iter()
        .skip(scroll)
        .take(visible_height)
        .cloned()
        .collect();

    let paragraph = Paragraph::new(visible_lines);
    f.render_widget(paragraph, inner);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vert = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vert[1])[1]
}

// ---------------------------------------------------------------------------
// Graph tab — key handling
// ---------------------------------------------------------------------------

/// Returns true if the app should quit.
fn handle_key_graph(app: &mut App, key: event::KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,

        // Tab cycles focus within graph
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.graph_focus = app.graph_focus.prev();
            } else {
                app.graph_focus = app.graph_focus.next();
            }
            // Select first item in newly focused panel if nothing selected
            if app.graph_focus != GraphFocus::Canvas {
                let count = graph_row_count(app);
                if count > 0 && current_selection(app).is_none() {
                    set_selection(app, 0);
                }
            }
        }
        KeyCode::BackTab => {
            app.graph_focus = app.graph_focus.prev();
            if app.graph_focus != GraphFocus::Canvas {
                let count = graph_row_count(app);
                if count > 0 && current_selection(app).is_none() {
                    set_selection(app, 0);
                }
            }
        }

        // Toggle local <-> global view
        KeyCode::Char('v') => {
            app.graph_canvas_mode = match app.graph_canvas_mode {
                CanvasMode::Local => CanvasMode::Global,
                CanvasMode::Global => CanvasMode::Local,
            };
        }

        // Toggle sidebar visibility
        KeyCode::Char('h') if app.graph_focus == GraphFocus::Canvas => {
            app.graph_sidebar_visible = !app.graph_sidebar_visible;
        }

        // Hop depth adjustment (local view only)
        KeyCode::Char('+') | KeyCode::Char('=') if app.graph_canvas_mode == CanvasMode::Local => {
            app.graph_hop_depth = (app.graph_hop_depth + 1).min(3);
        }
        KeyCode::Char('-') if app.graph_canvas_mode == CanvasMode::Local => {
            app.graph_hop_depth = app.graph_hop_depth.saturating_sub(1).max(1);
        }

        // Canvas spatial navigation
        KeyCode::Up | KeyCode::Char('k') if app.graph_focus == GraphFocus::Canvas => {
            canvas_move(app, layout::Direction::Up);
        }
        KeyCode::Down | KeyCode::Char('j') if app.graph_focus == GraphFocus::Canvas => {
            canvas_move(app, layout::Direction::Down);
        }
        KeyCode::Left if app.graph_focus == GraphFocus::Canvas => {
            canvas_move(app, layout::Direction::Left);
        }
        KeyCode::Right if app.graph_focus == GraphFocus::Canvas => {
            canvas_move(app, layout::Direction::Right);
        }

        // Sidebar list navigation (Callers/Callees focus)
        KeyCode::Char('j') | KeyCode::Down => move_cursor(app, 1),
        KeyCode::Char('k') | KeyCode::Up => move_cursor(app, -1),
        KeyCode::PageDown => move_cursor(app, 20),
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => move_cursor(app, 20),
        KeyCode::PageUp => move_cursor(app, -20),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            move_cursor(app, -20)
        }
        KeyCode::Char('g') | KeyCode::Home => move_to(app, 0),
        KeyCode::Char('G') | KeyCode::End => move_to_end(app),

        // Sort (sidebar only)
        KeyCode::Char('s') => {
            if app.graph_focus == GraphFocus::FuncList {
                let col = app.graph_sort_col.next();
                if col == app.graph_sort_col {
                    app.graph_sort_asc = !app.graph_sort_asc;
                } else {
                    app.graph_sort_col = col;
                    app.graph_sort_asc = col == GraphSortCol::Name;
                }
                resort_graph(app);
            }
        }

        // Enter: re-center (canvas) or jump (sidebar)
        KeyCode::Enter => match app.graph_focus {
            GraphFocus::Canvas => canvas_enter(app),
            GraphFocus::FuncList => open_source_view(app),
            GraphFocus::Callers | GraphFocus::Callees => {
                graph_jump_to_selected(app);
            }
        },

        // Backspace: go back in navigation history
        KeyCode::Backspace => {
            graph_nav_back(app);
        }

        // Function picker
        KeyCode::Char('/') => {
            if let Some(data) = &app.graph_tab_data {
                let all: Vec<usize> = (0..data.func_names.len()).collect();
                app.func_picker = Some(FuncPicker {
                    query: String::new(),
                    filtered: all,
                    cursor: 0,
                    scroll: 0,
                    visible_height: 0,
                });
            }
        }

        // Tree overlay — from canvas or sidebar
        KeyCode::Char('t') => {
            let name = if app.graph_focus == GraphFocus::Canvas {
                app.graph_canvas_selected_node.clone()
            } else {
                graph_selected_func_name(app)
            };
            if let Some(name) = name {
                open_tree_overlay(app, &name, true);
            }
        }

        _ => {}
    }
    false
}

fn canvas_move(app: &mut App, direction: layout::Direction) {
    let positions = match app.graph_canvas_mode {
        CanvasMode::Local => {
            let center = match &app.graph_canvas_selected_node {
                Some(n) => n.clone(),
                None => return,
            };
            let cache_key = (center.clone(), app.graph_hop_depth);
            if let Some(cached) = app.graph_local_layout_cache.get(&cache_key) {
                cached.clone()
            } else {
                let data = match &app.graph_tab_data {
                    Some(d) => d,
                    None => return,
                };
                let callers_map = graph_callers_names(data);
                let callees_map = graph_callees_names(data);
                let pos = layout::sugiyama_layout(
                    &center,
                    &callers_map,
                    &callees_map,
                    app.graph_hop_depth,
                );
                app.graph_local_layout_cache.insert(cache_key, pos.clone());
                pos
            }
        }
        CanvasMode::Global => match &app.graph_global_layout_cache {
            Some(p) => p.clone(),
            None => return,
        },
    };

    let current_node = match &app.graph_canvas_selected_node {
        Some(n) => n.clone(),
        None => {
            // Select the first node if nothing selected
            if let Some(name) = positions.keys().next() {
                app.graph_canvas_selected_node = Some(name.clone());
            }
            return;
        }
    };

    let current_pos = match positions.get(&current_node) {
        Some(p) => *p,
        None => return,
    };

    if let Some(next) =
        layout::nearest_node_in_direction(current_pos, direction, &positions, &current_node)
    {
        app.graph_canvas_selected_node = Some(next.clone());
        // Sync func list selection
        if let Some(data) = &app.graph_tab_data {
            if let Some(&idx) = data.name_to_index.get(&next) {
                app.graph_func_list_state.select(Some(idx));
            }
        }
    }
}

fn graph_jump_to_selected(app: &mut App) {
    let data = match &app.graph_tab_data {
        Some(d) => d,
        None => return,
    };

    let target_name = match app.graph_focus {
        GraphFocus::Callers => {
            let sel = match app.graph_callers_state.selected() {
                Some(s) => s,
                None => return,
            };
            let func_name = match graph_selected_func_name(app) {
                Some(n) => n,
                None => return,
            };
            data.callers
                .get(&func_name)
                .and_then(|v| v.get(sel))
                .map(|(n, _)| n.clone())
        }
        GraphFocus::Callees => {
            let sel = match app.graph_callees_state.selected() {
                Some(s) => s,
                None => return,
            };
            let func_name = match graph_selected_func_name(app) {
                Some(n) => n,
                None => return,
            };
            data.callees
                .get(&func_name)
                .and_then(|v| v.get(sel))
                .map(|(n, _)| n.clone())
        }
        _ => return,
    };

    let target_name = match target_name {
        Some(n) => n,
        None => return,
    };

    let target_idx = data.name_to_index.get(&target_name).copied();

    if let Some(idx) = target_idx {
        // Push current position to history
        if let Some(current) = app.graph_func_list_state.selected() {
            app.graph_nav_history.push(current);
        }
        app.graph_func_list_state.select(Some(idx));
        // Also sync canvas selected node
        if let Some(data) = &app.graph_tab_data {
            if let Some(name) = data.func_names.get(idx) {
                app.graph_canvas_selected_node = Some(name.clone());
            }
        }
        app.graph_focus = GraphFocus::Canvas;
        // Reset caller/callee states
        app.graph_callers_state.select(None);
        app.graph_callees_state.select(None);
    }
}

fn canvas_enter(app: &mut App) {
    if let Some(ref node) = app.graph_canvas_selected_node.clone() {
        // Push current to history
        if let Some(ref current) = app.graph_canvas_selected_node {
            app.graph_nav_history.push(
                app.graph_tab_data
                    .as_ref()
                    .and_then(|d| d.name_to_index.get(current).copied())
                    .unwrap_or(0),
            );
        }
        if app.graph_canvas_mode == CanvasMode::Global {
            app.graph_canvas_mode = CanvasMode::Local;
        }
        app.graph_canvas_selected_node = Some(node.clone());
        // Also sync the func list selection
        if let Some(data) = &app.graph_tab_data {
            if let Some(&idx) = data.name_to_index.get(node) {
                app.graph_func_list_state.select(Some(idx));
            }
        }
    }
}

fn graph_nav_back(app: &mut App) {
    if let Some(prev) = app.graph_nav_history.pop() {
        let count = app
            .graph_tab_data
            .as_ref()
            .map_or(0, |d| d.func_names.len());
        if prev < count {
            app.graph_func_list_state.select(Some(prev));
            // Sync canvas selected node
            if let Some(data) = &app.graph_tab_data {
                if let Some(name) = data.func_names.get(prev) {
                    app.graph_canvas_selected_node = Some(name.clone());
                }
            }
            app.graph_focus = GraphFocus::Canvas;
            app.graph_callers_state.select(None);
            app.graph_callees_state.select(None);
        }
    }
}

// ---------------------------------------------------------------------------
// Graph tab — function picker
// ---------------------------------------------------------------------------

fn handle_key_func_picker(app: &mut App, key: event::KeyEvent) {
    let picker = match &mut app.func_picker {
        Some(p) => p,
        None => return,
    };

    match key.code {
        KeyCode::Esc => {
            app.func_picker = None;
        }
        KeyCode::Enter => {
            if let Some(&idx) = picker.filtered.get(picker.cursor) {
                if let Some(data) = &app.graph_tab_data {
                    if let Some(name) = data.func_names.get(idx) {
                        let name = name.clone();
                        // Push current to history
                        if let Some(current) = app.graph_func_list_state.selected() {
                            app.graph_nav_history.push(current);
                        }
                        app.graph_canvas_selected_node = Some(name.clone());
                        if let Some(&list_idx) = data.name_to_index.get(&name) {
                            app.graph_func_list_state.select(Some(list_idx));
                        }
                        app.graph_focus = GraphFocus::Canvas;
                        app.graph_callers_state.select(None);
                        app.graph_callees_state.select(None);
                    }
                }
            }
            app.func_picker = None;
        }
        KeyCode::Down | KeyCode::Char('j')
            if key.modifiers == KeyModifiers::NONE || key.code == KeyCode::Down =>
        {
            if !picker.filtered.is_empty() {
                picker.cursor = (picker.cursor + 1).min(picker.filtered.len() - 1);
            }
        }
        KeyCode::Up | KeyCode::Char('k')
            if key.modifiers == KeyModifiers::NONE || key.code == KeyCode::Up =>
        {
            picker.cursor = picker.cursor.saturating_sub(1);
        }
        KeyCode::Backspace => {
            picker.query.pop();
            update_func_picker_filter(app);
        }
        KeyCode::Char(c) => {
            let picker = app.func_picker.as_mut().unwrap();
            picker.query.push(c);
            update_func_picker_filter(app);
        }
        _ => {}
    }
}

fn update_func_picker_filter(app: &mut App) {
    let data = match &app.graph_tab_data {
        Some(d) => d,
        None => return,
    };
    let picker = match &mut app.func_picker {
        Some(p) => p,
        None => return,
    };

    if picker.query.is_empty() {
        picker.filtered = (0..data.func_names.len()).collect();
    } else {
        let query = picker.query.to_lowercase();
        picker.filtered = data
            .func_names
            .iter()
            .enumerate()
            .filter(|(_, name)| name.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect();
    }
    picker.cursor = 0;
    picker.scroll = 0;
}

fn render_func_picker(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let picker = match &mut app.func_picker {
        Some(p) => p,
        None => return,
    };

    let popup = centered_rect(60, 70, area);
    f.render_widget(Clear, popup);

    let total = app
        .graph_tab_data
        .as_ref()
        .map_or(0, |d| d.func_names.len());
    let title = format!(" Jump to ({}/{}) ", picker.filtered.len(), total);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    if inner.height < 2 {
        return;
    }

    // Search input row
    let search_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    };
    let search_line = Line::from(vec![
        Span::styled("/ ", Style::default().fg(Color::Yellow)),
        Span::raw(&picker.query),
        Span::styled(
            "_",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::SLOW_BLINK),
        ),
    ]);
    f.render_widget(Paragraph::new(search_line), search_area);

    // List area below the search input
    let list_area = Rect {
        x: inner.x,
        y: inner.y + 1,
        width: inner.width,
        height: inner.height - 1,
    };

    let visible_height = list_area.height as usize;
    picker.visible_height = visible_height;

    if picker.filtered.is_empty() {
        let msg = Paragraph::new("  (no matches)").style(Style::default().fg(Color::DarkGray));
        f.render_widget(msg, list_area);
        return;
    }

    // Scroll to keep cursor visible
    if picker.cursor < picker.scroll {
        picker.scroll = picker.cursor;
    } else if picker.cursor >= picker.scroll + visible_height {
        picker.scroll = picker.cursor + 1 - visible_height;
    }

    let cc_map = app.graph_tab_data.as_ref().map(|d| &d.cc_map);
    let func_names = app.graph_tab_data.as_ref().map(|d| &d.func_names);

    let visible_lines: Vec<Line> = picker
        .filtered
        .iter()
        .skip(picker.scroll)
        .take(visible_height)
        .enumerate()
        .map(|(vi, &idx)| {
            let global_i = picker.scroll + vi;
            let is_cursor = global_i == picker.cursor;

            let name = func_names
                .and_then(|n| n.get(idx))
                .map(|s| s.as_str())
                .unwrap_or("?");
            let cc = cc_map.and_then(|m| m.get(name).copied()).unwrap_or(0);

            let bg = if is_cursor {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };

            Line::from(vec![
                Span::styled(format!("  {}", name), bg.patch(cc_style(cc))),
                Span::styled(format!("  (McC: {})", cc), bg.fg(Color::DarkGray)),
            ])
        })
        .collect();

    let paragraph = Paragraph::new(visible_lines);
    f.render_widget(paragraph, list_area);
}

// ---------------------------------------------------------------------------
// Graph tab — rendering
// ---------------------------------------------------------------------------

fn render_graph(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let data = match &app.graph_tab_data {
        Some(d) => d,
        None => return,
    };

    if data.func_names.is_empty() {
        let msg =
            Paragraph::new("No functions found.").block(Block::default().borders(Borders::ALL));
        f.render_widget(msg, area);
        return;
    }

    // Layout: canvas (70%) + sidebar (30%), or full-width canvas if sidebar hidden
    if app.graph_sidebar_visible {
        let panels = Layout::horizontal([Constraint::Percentage(70), Constraint::Percentage(30)])
            .split(area);

        // Canvas panel
        match app.graph_canvas_mode {
            CanvasMode::Local => render_canvas_local(f, app, panels[0]),
            CanvasMode::Global => render_canvas_global(f, app, panels[0]),
        }

        // Sidebar panel
        render_graph_sidebar(f, app, panels[1]);
    } else {
        // Full-width canvas
        match app.graph_canvas_mode {
            CanvasMode::Local => render_canvas_local(f, app, area),
            CanvasMode::Global => render_canvas_global(f, app, area),
        }
    }

    // Function picker overlay
    render_func_picker(f, app, area);
}

fn render_graph_sidebar(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let data = match &app.graph_tab_data {
        Some(d) => d,
        None => return,
    };

    // Get the selected node name — prefer canvas selection, fall back to list
    let sel_name = app
        .graph_canvas_selected_node
        .clone()
        .or_else(|| graph_selected_func_name(app));

    // Detect selection change and reset callers/callees states
    let sel_index = app.graph_func_list_state.selected();
    if sel_index != app.graph_last_selected_func {
        app.graph_last_selected_func = sel_index;
        app.graph_callers_state = ListState::default();
        app.graph_callees_state = ListState::default();
    }

    if let Some(ref name) = sel_name {
        let cc = data.cc_map.get(name).copied().unwrap_or(0);
        let caller_list = data.callers.get(name);
        let callee_list = data.callees.get(name);
        let fi = caller_list.map_or(0, |v| v.len());
        let fo = callee_list.map_or(0, |v| v.len());

        let sidebar_chunks = Layout::vertical([
            Constraint::Length(3),      // header
            Constraint::Percentage(50), // callers
            Constraint::Percentage(50), // callees
        ])
        .split(area);

        // Header with node info
        let header_block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", truncate_label(name, 25)));
        let header_inner = header_block.inner(sidebar_chunks[0]);
        f.render_widget(header_block, sidebar_chunks[0]);

        let header_line = Line::from(vec![
            Span::raw("McC: "),
            Span::styled(cc.to_string(), cc_style(cc)),
            Span::raw("  FI: "),
            Span::styled(fi.to_string(), Style::default().fg(Color::Cyan)),
            Span::raw("  FO: "),
            Span::styled(fo.to_string(), Style::default().fg(Color::Cyan)),
        ]);
        f.render_widget(Paragraph::new(header_line), header_inner);

        // Callers sub-panel
        let callers_border = if app.graph_focus == GraphFocus::Callers {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        render_graph_sublist(
            f,
            &mut app.graph_callers_state,
            caller_list.map(|v| v.as_slice()).unwrap_or(&[]),
            &data.external_nodes,
            sidebar_chunks[1],
            format!(" Callers ({}) ", fi),
            callers_border,
            app.graph_focus == GraphFocus::Callers,
        );

        // Callees sub-panel
        let callees_border = if app.graph_focus == GraphFocus::Callees {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        render_graph_sublist(
            f,
            &mut app.graph_callees_state,
            callee_list.map(|v| v.as_slice()).unwrap_or(&[]),
            &data.external_nodes,
            sidebar_chunks[2],
            format!(" Callees ({}) ", fo),
            callees_border,
            app.graph_focus == GraphFocus::Callees,
        );
    } else {
        let msg = Paragraph::new("  Select a function")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title(" Details "));
        f.render_widget(msg, area);
    }
}

fn cc_style(cc: usize) -> Style {
    if cc >= 10 {
        Style::default().fg(Color::Red)
    } else if cc >= 5 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Green)
    }
}

fn render_graph_sublist(
    f: &mut ratatui::Frame,
    state: &mut ListState,
    entries: &[(String, usize)],
    external_nodes: &HashSet<String>,
    area: Rect,
    title: String,
    border_style: Style,
    is_focused: bool,
) {
    if entries.is_empty() {
        let msg = Paragraph::new("  (none)")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(border_style),
            );
        f.render_widget(msg, area);
        return;
    }

    let visible_count = (area.height.saturating_sub(2)) as usize;
    let selected = state.selected().unwrap_or(0);
    let mut list_offset = state.offset();
    if selected < list_offset {
        list_offset = selected;
    } else if visible_count > 0 && selected >= list_offset + visible_count {
        list_offset = selected + 1 - visible_count;
    }
    let end = (list_offset + visible_count).min(entries.len());

    let items: Vec<ListItem> = entries[list_offset..end]
        .iter()
        .map(|(name, mcc)| {
            let is_ext = external_nodes.contains(name);
            let name_style = if is_ext {
                Style::default().fg(Color::DarkGray)
            } else {
                cc_style(*mcc)
            };
            ListItem::new(Line::from(vec![
                Span::styled(name.as_str(), name_style),
                Span::styled(
                    format!("  (McC: {})", mcc),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    *state.offset_mut() = 0;
    if is_focused && !entries.is_empty() {
        state.select(Some(selected.saturating_sub(list_offset)));
    }

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(border_style),
        )
        .highlight_style(if is_focused {
            Style::default().bg(Color::DarkGray)
        } else {
            Style::default()
        })
        .highlight_symbol(if is_focused { "\u{25b6} " } else { "  " });

    f.render_stateful_widget(list, area, state);
    restore_list_state(state, list_offset);
}

// ---------------------------------------------------------------------------
// Tree overlay
// ---------------------------------------------------------------------------

fn open_tree_overlay(app: &mut App, root_name: &str, downstream: bool) {
    let mut overlay = TreeOverlay {
        root_name: root_name.to_string(),
        downstream,
        max_depth: 3,
        lines: Vec::new(),
        scroll: 0,
        cursor: 0,
        visible_height: 0,
        collapsed: HashSet::new(),
    };
    rebuild_tree_lines(app, &mut overlay);
    app.tree_overlay = Some(overlay);
}

fn rebuild_tree_lines(app: &App, overlay: &mut TreeOverlay) {
    let data = match &app.graph_tab_data {
        Some(d) => d,
        None => return,
    };
    let mut lines = Vec::new();
    let mut visited = HashSet::new();
    build_tree_recursive(
        data,
        &overlay.root_name,
        0,
        overlay.max_depth,
        overlay.downstream,
        &overlay.collapsed,
        &mut visited,
        &mut lines,
    );
    overlay.lines = lines;
    // Clamp cursor
    if !overlay.lines.is_empty() {
        overlay.cursor = overlay.cursor.min(overlay.lines.len() - 1);
    } else {
        overlay.cursor = 0;
    }
}

fn build_tree_recursive(
    data: &GraphTabData,
    name: &str,
    indent: usize,
    max_depth: usize,
    downstream: bool,
    collapsed: &HashSet<String>,
    visited: &mut HashSet<String>,
    lines: &mut Vec<TreeLine>,
) {
    let is_external = data.external_nodes.contains(name);
    if indent > 128 || lines.len() >= 10_000 {
        return;
    }
    let mcc = data.cc_map.get(name).copied().unwrap_or(0);
    let children = if downstream {
        data.callees.get(name)
    } else {
        data.callers.get(name)
    };
    let has_children = children.is_some_and(|v| !v.is_empty());
    let is_cycle = visited.contains(name);
    let is_collapsed = collapsed.contains(name);

    lines.push(TreeLine {
        indent,
        name: name.to_string(),
        mcc,
        is_external,
        has_children,
        is_collapsed: is_collapsed && has_children,
        is_cycle,
    });

    if is_cycle || is_collapsed || indent >= max_depth || !has_children {
        return;
    }

    visited.insert(name.to_string());

    if let Some(children) = children {
        for (child_name, _) in children {
            build_tree_recursive(
                data,
                child_name,
                indent + 1,
                max_depth,
                downstream,
                collapsed,
                visited,
                lines,
            );
        }
    }

    visited.remove(name);
}

fn tree_overlay_rebuild(app: &mut App) {
    let mut ov = app.tree_overlay.take().unwrap();
    rebuild_tree_lines(app, &mut ov);
    app.tree_overlay = Some(ov);
}

fn handle_key_tree_overlay(app: &mut App, key: event::KeyEvent) {
    let overlay = match &mut app.tree_overlay {
        Some(o) => o,
        None => return,
    };

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.tree_overlay = None;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if !overlay.lines.is_empty() {
                overlay.cursor = (overlay.cursor + 1).min(overlay.lines.len() - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            overlay.cursor = overlay.cursor.saturating_sub(1);
        }
        KeyCode::Char('d') => {
            // Toggle direction
            let name = overlay.root_name.clone();
            let downstream = !overlay.downstream;
            open_tree_overlay(app, &name, downstream);
        }
        KeyCode::Char('+') | KeyCode::Char('=') => {
            overlay.max_depth = (overlay.max_depth + 1).min(10);
            tree_overlay_rebuild(app);
        }
        KeyCode::Char('-') => {
            overlay.max_depth = overlay.max_depth.saturating_sub(1).max(1);
            tree_overlay_rebuild(app);
        }
        KeyCode::Char(' ') => {
            // Toggle collapse
            if let Some(line) = overlay.lines.get(overlay.cursor) {
                let name = line.name.clone();
                if overlay.collapsed.contains(&name) {
                    overlay.collapsed.remove(&name);
                } else {
                    overlay.collapsed.insert(name);
                }
                tree_overlay_rebuild(app);
            }
        }
        KeyCode::Enter => {
            // Re-root at cursor
            if let Some(line) = overlay.lines.get(overlay.cursor) {
                let name = line.name.clone();
                let downstream = overlay.downstream;
                open_tree_overlay(app, &name, downstream);
            }
        }
        KeyCode::Char('g') | KeyCode::Home => {
            overlay.cursor = 0;
        }
        KeyCode::Char('G') | KeyCode::End => {
            if !overlay.lines.is_empty() {
                overlay.cursor = overlay.lines.len() - 1;
            }
        }
        _ => {}
    }
}

fn render_tree_overlay(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let overlay = match &mut app.tree_overlay {
        Some(o) => o,
        None => return,
    };

    let popup = centered_rect(80, 80, area);
    f.render_widget(Clear, popup);

    let dir_label = if overlay.downstream {
        "Callees"
    } else {
        "Callers"
    };
    let title = format!(
        " Tree: {} ({}, depth {}) ",
        overlay.root_name, dir_label, overlay.max_depth
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let visible_height = inner.height as usize;
    overlay.visible_height = visible_height;

    if overlay.lines.is_empty() {
        let msg = Paragraph::new("  (no connections)").style(Style::default().fg(Color::DarkGray));
        f.render_widget(msg, inner);
        return;
    }

    // Scroll to keep cursor visible
    if overlay.cursor < overlay.scroll {
        overlay.scroll = overlay.cursor;
    } else if overlay.cursor >= overlay.scroll + visible_height {
        overlay.scroll = overlay.cursor + 1 - visible_height;
    }

    let scroll = overlay.scroll;
    let cursor = overlay.cursor;
    let visible_lines: Vec<Line> = overlay
        .lines
        .iter()
        .skip(scroll)
        .take(visible_height)
        .enumerate()
        .map(|(vi, tl)| {
            let global_i = scroll + vi;
            let is_cursor = global_i == cursor;

            let indent_str = "  ".repeat(tl.indent);
            let prefix = if tl.has_children {
                if tl.is_collapsed {
                    "\u{25b6} "
                } else {
                    "\u{25bc} "
                }
            } else {
                "  "
            };

            let mut suffix = String::new();
            if tl.is_cycle {
                suffix.push_str(" (cycle)");
            }
            if tl.is_external {
                suffix.push_str(" [ext]");
            }

            let name_style = if tl.is_external {
                Style::default().fg(Color::DarkGray)
            } else {
                cc_style(tl.mcc)
            };

            let bg = if is_cursor {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };

            Line::from(vec![
                Span::styled(indent_str, bg),
                Span::styled(prefix, bg.fg(Color::Yellow)),
                Span::styled(tl.name.clone(), bg.patch(name_style)),
                Span::styled(format!("  (McC: {})", tl.mcc), bg.fg(Color::DarkGray)),
                Span::styled(suffix, bg.fg(Color::DarkGray)),
            ])
        })
        .collect();

    let paragraph = Paragraph::new(visible_lines);
    f.render_widget(paragraph, inner);
}

// ---------------------------------------------------------------------------
// Canvas rendering helpers
// ---------------------------------------------------------------------------

/// Converts GraphTabData callers/callees to name-only maps for layout functions.
fn graph_callers_names(data: &GraphTabData) -> HashMap<String, Vec<String>> {
    data.callers
        .iter()
        .map(|(k, v)| (k.clone(), v.iter().map(|(n, _)| n.clone()).collect()))
        .collect()
}

fn graph_callees_names(data: &GraphTabData) -> HashMap<String, Vec<String>> {
    data.callees
        .iter()
        .map(|(k, v)| (k.clone(), v.iter().map(|(n, _)| n.clone()).collect()))
        .collect()
}

fn truncate_label(name: &str, max_len: usize) -> String {
    if max_len == 0 {
        String::new()
    } else if name.chars().count() <= max_len {
        name.to_string()
    } else {
        format!("{}…", name.chars().take(max_len - 1).collect::<String>())
    }
}

#[cfg(test)]
mod release_tests {
    #[test]
    fn unicode_graph_labels_do_not_split_utf8() {
        assert_eq!(super::truncate_label("数据处理流程", 4), "数据处…");
        assert_eq!(super::truncate_label("é", 1), "é");
        assert_eq!(super::truncate_label("name", 0), "");
    }
}

fn cc_color(cc: usize) -> Color {
    if cc >= 10 {
        Color::Red
    } else if cc >= 5 {
        Color::Yellow
    } else {
        Color::Green
    }
}

// ---------------------------------------------------------------------------
// Canvas — local ego graph rendering
// ---------------------------------------------------------------------------

fn render_canvas_local(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let data = match &app.graph_tab_data {
        Some(d) => d,
        None => return,
    };

    let center = match &app.graph_canvas_selected_node {
        Some(n) => n.clone(),
        None => {
            // Fall back to currently selected function in the list
            match graph_selected_func_name(app) {
                Some(n) => {
                    app.graph_canvas_selected_node = Some(n.clone());
                    n
                }
                None => return,
            }
        }
    };

    let callers_map = graph_callers_names(data);
    let callees_map = graph_callees_names(data);

    // Get or compute layout (cached by center node + hop depth)
    let cache_key = (center.clone(), app.graph_hop_depth);
    let positions = if let Some(cached) = app.graph_local_layout_cache.get(&cache_key) {
        cached.clone()
    } else {
        let pos = layout::sugiyama_layout(&center, &callers_map, &callees_map, app.graph_hop_depth);
        app.graph_local_layout_cache.insert(cache_key, pos.clone());
        pos
    };

    // Extract subgraph for edges
    let (_nodes, edges, back_edges) =
        layout::extract_ego_subgraph(&center, &callers_map, &callees_map, app.graph_hop_depth);

    if positions.is_empty() {
        let msg = Paragraph::new("  No connections to display.")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title(" Graph "));
        f.render_widget(msg, area);
        return;
    }

    // Compute viewport bounds
    let node_half_w = 6.0;
    let node_half_h = 1.5;
    let padding = 4.0;

    let min_x = positions
        .values()
        .map(|p| p.0)
        .fold(f64::INFINITY, f64::min)
        - node_half_w
        - padding;
    let max_x = positions
        .values()
        .map(|p| p.0)
        .fold(f64::NEG_INFINITY, f64::max)
        + node_half_w
        + padding;
    let min_y = positions
        .values()
        .map(|p| p.1)
        .fold(f64::INFINITY, f64::min)
        - node_half_h
        - padding;
    let max_y = positions
        .values()
        .map(|p| p.1)
        .fold(f64::NEG_INFINITY, f64::max)
        + node_half_h
        + padding;

    let selected_node = app.graph_canvas_selected_node.clone().unwrap_or_default();

    let canvas_border = if app.graph_focus == GraphFocus::Canvas {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };

    let hop_label = format!(" Local (depth {}) ", app.graph_hop_depth);
    let canvas_widget = Canvas::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(hop_label)
                .border_style(canvas_border),
        )
        .x_bounds([min_x, max_x])
        .y_bounds([min_y, max_y])
        .paint(move |ctx| {
            // Draw edges first (below nodes)
            for (from, to) in &edges {
                if let (Some(&(x1, y1)), Some(&(x2, y2))) = (positions.get(from), positions.get(to))
                {
                    let is_back_edge = back_edges.contains(&(from.clone(), to.clone()));
                    let touches_selected = *from == selected_node || *to == selected_node;
                    let edge_color = if is_back_edge {
                        Color::Magenta
                    } else if touches_selected {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    };

                    ctx.draw(&CanvasLine {
                        x1,
                        y1: y1 - node_half_h,
                        x2,
                        y2: y2 + node_half_h,
                        color: edge_color,
                    });

                    // Direction indicator at target
                    ctx.print(
                        x2,
                        y2 + node_half_h + 0.3,
                        Line::from(Span::styled("▼", Style::default().fg(edge_color))),
                    );
                }
            }

            // Draw nodes
            ctx.layer();
            for (name, &(x, y)) in &positions {
                let cc = data.cc_map.get(name).copied().unwrap_or(0);
                let is_external = data.external_nodes.contains(name);
                let is_selected = *name == selected_node;

                let node_color = if is_external {
                    Color::DarkGray
                } else {
                    cc_color(cc)
                };

                let border_color = if is_selected {
                    Color::White
                } else {
                    node_color
                };

                ctx.draw(&CanvasRect {
                    x: x - node_half_w,
                    y: y - node_half_h,
                    width: node_half_w * 2.0,
                    height: node_half_h * 2.0,
                    color: border_color,
                });

                let label = truncate_label(name, 18);
                let label_style = if is_selected {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(node_color)
                };
                ctx.print(
                    x - label.len() as f64 * 0.4,
                    y,
                    Line::from(Span::styled(label, label_style)),
                );
            }
        });

    f.render_widget(canvas_widget, area);
}

// ---------------------------------------------------------------------------
// Canvas — global overview rendering
// ---------------------------------------------------------------------------

fn render_canvas_global(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let data = match &app.graph_tab_data {
        Some(d) => d,
        None => return,
    };

    if data.func_names.is_empty() {
        let msg = Paragraph::new("  No functions found.")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Global Graph "),
            );
        f.render_widget(msg, area);
        return;
    }

    // Compute or use cached global layout
    if app.graph_global_layout_cache.is_none() {
        let callees_map = graph_callees_names(data);
        let edges: Vec<(String, String)> = callees_map
            .iter()
            .flat_map(|(from, tos)| tos.iter().map(move |to| (from.clone(), to.clone())))
            .collect();

        // Cap at 500 nodes by degree
        let nodes = if data.func_names.len() > 500 {
            let mut degree: HashMap<&str, usize> = HashMap::new();
            for (from, tos) in &callees_map {
                *degree.entry(from.as_str()).or_default() += tos.len();
                for to in tos {
                    *degree.entry(to.as_str()).or_default() += 1;
                }
            }
            let mut sorted: Vec<&str> = data.func_names.iter().map(|s| s.as_str()).collect();
            sorted.sort_by(|a, b| degree.get(b).unwrap_or(&0).cmp(degree.get(a).unwrap_or(&0)));
            sorted.truncate(500);
            sorted.into_iter().map(|s| s.to_string()).collect()
        } else {
            data.func_names.clone()
        };

        let positions = layout::force_directed_layout(&nodes, &edges, 200);
        app.graph_global_layout_cache = Some(positions);
    }

    let positions = app.graph_global_layout_cache.as_ref().unwrap();
    let selected_node = app.graph_canvas_selected_node.clone().unwrap_or_default();

    // Collect edges between positioned nodes
    let callees_map = graph_callees_names(data);
    let edges: Vec<(String, String)> = callees_map
        .iter()
        .flat_map(|(from, tos)| {
            tos.iter()
                .filter(|to| {
                    positions.contains_key(to.as_str()) && positions.contains_key(from.as_str())
                })
                .map(move |to| (from.clone(), to.clone()))
        })
        .collect();

    // Viewport bounds
    let padding = 8.0;
    let min_x = positions
        .values()
        .map(|p| p.0)
        .fold(f64::INFINITY, f64::min)
        - padding;
    let max_x = positions
        .values()
        .map(|p| p.0)
        .fold(f64::NEG_INFINITY, f64::max)
        + padding;
    let min_y = positions
        .values()
        .map(|p| p.1)
        .fold(f64::INFINITY, f64::min)
        - padding;
    let max_y = positions
        .values()
        .map(|p| p.1)
        .fold(f64::NEG_INFINITY, f64::max)
        + padding;

    // Neighbors of selected node for label display
    let selected_neighbors: HashSet<String> = {
        let mut n = HashSet::new();
        if let Some(callers) = data.callers.get(&selected_node) {
            for (name, _) in callers {
                n.insert(name.clone());
            }
        }
        if let Some(callees) = data.callees.get(&selected_node) {
            for (name, _) in callees {
                n.insert(name.clone());
            }
        }
        n
    };

    let total_nodes = data.func_names.len();
    let shown_nodes = positions.len();
    let title = if shown_nodes < total_nodes {
        format!(" Global ({}/{}) ", shown_nodes, total_nodes)
    } else {
        format!(" Global ({}) ", total_nodes)
    };

    let canvas_border = if app.graph_focus == GraphFocus::Canvas {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };

    let canvas_widget = Canvas::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(canvas_border),
        )
        .x_bounds([min_x, max_x])
        .y_bounds([min_y, max_y])
        .paint(move |ctx| {
            // Draw edges
            for (from, to) in &edges {
                if let (Some(&(x1, y1)), Some(&(x2, y2))) = (positions.get(from), positions.get(to))
                {
                    let touches_selected = *from == selected_node || *to == selected_node;
                    let edge_color = if touches_selected {
                        Color::Cyan
                    } else {
                        Color::Indexed(236) // very dark gray
                    };
                    ctx.draw(&CanvasLine {
                        x1,
                        y1,
                        x2,
                        y2,
                        color: edge_color,
                    });
                }
            }

            // Draw nodes
            ctx.layer();
            for (name, &(x, y)) in positions {
                let cc = data.cc_map.get(name).copied().unwrap_or(0);
                let is_external = data.external_nodes.contains(name);
                let is_selected = *name == selected_node;

                let node_color = if is_external {
                    Color::DarkGray
                } else {
                    cc_color(cc)
                };

                // Small rectangle for each node
                let size = if is_selected { 1.5 } else { 0.8 };
                ctx.draw(&CanvasRect {
                    x: x - size,
                    y: y - size,
                    width: size * 2.0,
                    height: size * 2.0,
                    color: if is_selected {
                        Color::White
                    } else {
                        node_color
                    },
                });

                // Labels only for selected + neighbors
                if is_selected || selected_neighbors.contains(name) {
                    let label = truncate_label(name, 14);
                    let style = if is_selected {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(node_color)
                    };
                    ctx.print(x + size + 0.5, y, Line::from(Span::styled(label, style)));
                }
            }
        });

    f.render_widget(canvas_widget, area);
}
