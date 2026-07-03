use crate::store::DatabaseStore;
use db_client::{ConnectionId, DatabaseDriver, QueryResult};
use gpui::{
    App, ClipboardItem, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Window, div, px, relative,
};
use std::collections::HashSet;
use ui::prelude::*;
use ui::{Icon, IconButton, IconName, IconSize, Label, LabelSize, Tooltip, h_flex, v_flex};
use workspace::{Item, item::ItemEvent};

/// One node of a query plan tree. `children` are the sub-operations nested under
/// this node in the engine's EXPLAIN output.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    pub text: String,
    pub children: Vec<PlanNode>,
}

struct RawNode {
    text: String,
    indent: usize,
    children: Vec<usize>,
}

// Leading-whitespace width: spaces count as one, tabs as four. Tree-connector
// characters that some engines print (|, `, +) before the node text also count
// as indentation so sibling levels line up.
fn indent_width(line: &str) -> usize {
    let mut width = 0;
    for ch in line.chars() {
        match ch {
            ' ' => width += 1,
            '\t' => width += 4,
            '|' | '`' | '+' => width += 1,
            _ => break,
        }
    }
    width
}

// Strips leading indentation, tree connectors, and a leading "->" arrow so the
// displayed node text is the operation itself.
fn clean_node_text(line: &str) -> String {
    let trimmed = line.trim_start_matches([' ', '\t', '|', '`', '+']).trim_start();
    // Strip the "->" arrow as a unit; otherwise strip any tree dashes ("|--").
    let without_arrow = trimmed
        .strip_prefix("->")
        .unwrap_or_else(|| trimmed.trim_start_matches('-'));
    without_arrow.trim().to_string()
}

/// Builds a plan forest from EXPLAIN text by leading-indentation depth. A line
/// indented more than the line above becomes its child; equal or less indent
/// closes back up to the matching ancestor.
pub fn parse_plan_tree(text: &str) -> Vec<PlanNode> {
    let mut arena: Vec<RawNode> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = indent_width(line);
        let cleaned = clean_node_text(line);
        if cleaned.is_empty() {
            continue;
        }
        let index = arena.len();
        arena.push(RawNode {
            text: cleaned,
            indent,
            children: Vec::new(),
        });

        while let Some(&top) = stack.last() {
            if arena[top].indent >= indent {
                stack.pop();
            } else {
                break;
            }
        }
        match stack.last() {
            Some(&parent) => arena[parent].children.push(index),
            None => roots.push(index),
        }
        stack.push(index);
    }

    roots.iter().map(|&index| build_node(&arena, index)).collect()
}

fn build_node(arena: &[RawNode], index: usize) -> PlanNode {
    PlanNode {
        text: arena[index].text.clone(),
        children: arena[index]
            .children
            .iter()
            .map(|&child| build_node(arena, child))
            .collect(),
    }
}

/// Returns the EXPLAIN statement that yields a tree-shaped plan for the driver.
pub fn explain_sql_for_driver(driver: DatabaseDriver, sql: &str) -> String {
    let sql = sql.trim().trim_end_matches(';');
    match driver {
        DatabaseDriver::MySQL => format!("EXPLAIN FORMAT=TREE {sql}"),
        DatabaseDriver::SQLite => format!("EXPLAIN QUERY PLAN {sql}"),
        _ => format!("EXPLAIN {sql}"),
    }
}

/// Flattens an EXPLAIN result into newline-joined text. Engines return the plan
/// as one row per line (often a single column), so every non-null cell of every
/// row becomes a line, preserving its leading indentation.
pub fn plan_text_from_result(result: &QueryResult) -> String {
    let mut lines = Vec::new();
    for row in &result.rows {
        for value in row.iter().flatten() {
            lines.push(value.clone());
        }
    }
    lines.join("\n")
}

/// Returns the EXPLAIN statement that yields the engine's native machine-
/// readable plan format, for "Copy Native Format" (feeding external plan
/// analyzers). Falls back to the tree-shaped statement for drivers with no
/// distinct native format (e.g. SQLite's EXPLAIN QUERY PLAN is already the
/// only format it exposes).
pub fn native_explain_sql_for_driver(driver: DatabaseDriver, sql: &str) -> String {
    let trimmed = sql.trim().trim_end_matches(';');
    match driver {
        DatabaseDriver::MySQL => format!("EXPLAIN FORMAT=JSON {trimmed}"),
        DatabaseDriver::PostgreSQL => format!("EXPLAIN (FORMAT JSON) {trimmed}"),
        _ => explain_sql_for_driver(driver, sql),
    }
}

/// A positioned span in the flame-graph layout. `start`/`width` are fractions
/// (0.0..=1.0) of the root's total width, so the caller can place them with
/// `gpui::relative(..)` regardless of the container's actual pixel size.
#[derive(Debug, Clone, PartialEq)]
pub struct FlameSpan {
    pub depth: usize,
    pub start: f64,
    pub width: f64,
    pub text: String,
}

/// A node's proportional weight for flame-graph sizing: the row estimate
/// (`rows=N`) when the engine reports one, else the cost upper bound
/// (`cost=X..Y`), else a uniform weight so engines that report neither (e.g.
/// SQLite's EXPLAIN QUERY PLAN) still lay out as equal-width spans.
fn plan_node_weight(node: &PlanNode) -> f64 {
    extract_number_after(&node.text, "rows=")
        .or_else(|| extract_cost_upper_bound(&node.text))
        .filter(|value| *value > 0.0)
        .unwrap_or(1.0)
}

fn extract_number_after(text: &str, key: &str) -> Option<f64> {
    let start = text.find(key)? + key.len();
    let rest = &text[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    rest[..end].parse::<f64>().ok()
}

fn extract_cost_upper_bound(text: &str) -> Option<f64> {
    let start = text.find("cost=")? + "cost=".len();
    let rest = &text[start..];
    let end = rest.find([' ', ')']).unwrap_or(rest.len());
    let segment = &rest[..end];
    segment.rsplit("..").next().unwrap_or(segment).parse().ok()
}

/// Lays the plan tree out as nested flame spans: each level's siblings split
/// their parent's width proportionally to `plan_node_weight`, so a node's
/// span always fits within its parent's.
pub fn flame_layout(roots: &[PlanNode]) -> Vec<FlameSpan> {
    let mut spans = Vec::new();
    layout_flame_children(roots, 0, 0.0, 1.0, &mut spans);
    spans
}

fn layout_flame_children(
    nodes: &[PlanNode],
    depth: usize,
    start: f64,
    width: f64,
    spans: &mut Vec<FlameSpan>,
) {
    let total_weight: f64 = nodes.iter().map(plan_node_weight).sum();
    if total_weight <= 0.0 || width <= 0.0 {
        return;
    }
    let mut cursor = start;
    for node in nodes {
        let node_width = width * (plan_node_weight(node) / total_weight);
        spans.push(FlameSpan {
            depth,
            start: cursor,
            width: node_width,
            text: node.text.clone(),
        });
        layout_flame_children(&node.children, depth + 1, cursor, node_width, spans);
        cursor += node_width;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlanViewMode {
    Tree,
    Flame,
}

pub enum ExplainPlanEvent {
    Dismissed,
}

/// The live connection a plan was fetched from, kept only so "Copy Native
/// Format" can re-run the query with the driver's machine-readable EXPLAIN
/// syntax. Absent when a plan is rendered from a result with no live
/// connection attached (e.g. an EXPLAIN-shaped query result embedded inline
/// in the results grid) — in that case Copy Native Format is unavailable but
/// the tree/flame views still work from the already-fetched `roots`.
pub struct ExplainQueryContext {
    pub store: Entity<DatabaseStore>,
    pub connection_id: ConnectionId,
    pub database: String,
    pub driver: DatabaseDriver,
    pub sql: String,
}

pub struct ExplainPlanView {
    focus_handle: FocusHandle,
    roots: Vec<PlanNode>,
    collapsed: HashSet<usize>,
    mode: PlanViewMode,
    query_context: Option<ExplainQueryContext>,
}

impl ExplainPlanView {
    pub fn new(
        roots: Vec<PlanNode>,
        query_context: Option<ExplainQueryContext>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            roots,
            collapsed: HashSet::new(),
            mode: PlanViewMode::Tree,
            query_context,
        }
    }

    fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = match self.mode {
            PlanViewMode::Tree => PlanViewMode::Flame,
            PlanViewMode::Flame => PlanViewMode::Tree,
        };
        cx.notify();
    }

    fn copy_native_format(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(context) = self.query_context.as_ref() else {
            return;
        };
        let native_sql = native_explain_sql_for_driver(context.driver, &context.sql);
        let store = context.store.clone();
        let connection_id = context.connection_id;
        let database = context.database.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let task = store.update(cx, |store, cx| {
                store.execute_query(connection_id, database, native_sql, cx)
            });
            let result = task.await?;
            let text = plan_text_from_result(&result);
            cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string(text)))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn render_rows(&self) -> Vec<(usize, usize, String, bool)> {
        // (stable id, depth, text, has_children), skipping collapsed subtrees.
        let mut rows = Vec::new();
        let mut next_id = 0;
        for root in &self.roots {
            self.collect_rows(root, 0, &mut next_id, &mut rows);
        }
        rows
    }

    fn collect_rows(
        &self,
        node: &PlanNode,
        depth: usize,
        next_id: &mut usize,
        rows: &mut Vec<(usize, usize, String, bool)>,
    ) {
        let id = *next_id;
        *next_id += 1;
        let has_children = !node.children.is_empty();
        rows.push((id, depth, node.text.clone(), has_children));
        if has_children && !self.collapsed.contains(&id) {
            for child in &node.children {
                self.collect_rows(child, depth + 1, next_id, rows);
            }
        } else if has_children {
            // Keep id counter consistent by skipping the collapsed subtree's ids.
            for child in &node.children {
                Self::skip_ids(child, next_id);
            }
        }
    }

    fn skip_ids(node: &PlanNode, next_id: &mut usize) {
        *next_id += 1;
        for child in &node.children {
            Self::skip_ids(child, next_id);
        }
    }

    fn toggle(&mut self, id: usize, cx: &mut Context<Self>) {
        if !self.collapsed.insert(id) {
            self.collapsed.remove(&id);
        }
        cx.notify();
    }
}

impl EventEmitter<ExplainPlanEvent> for ExplainPlanView {}

impl EventEmitter<DismissEvent> for ExplainPlanView {}

impl Item for ExplainPlanView {
    type Event = DismissEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Query Plan".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }

    fn to_item_events(_event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(ItemEvent::CloseItem);
    }
}

impl Focusable for ExplainPlanView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ExplainPlanView {
    fn render_tree_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.render_rows();
        let mut row_elements = Vec::with_capacity(rows.len());
        for (id, depth, text, has_children) in rows {
            let collapsed = self.collapsed.contains(&id);
            let caret = if has_children {
                let icon = if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                };
                Some(
                    IconButton::new(("plan-node", id), icon)
                        .icon_size(IconSize::XSmall)
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle(id, cx))),
                )
            } else {
                None
            };
            let indent = if caret.is_some() {
                depth as f32 * 16.0
            } else {
                depth as f32 * 16.0 + 20.0
            };
            row_elements.push(
                h_flex()
                    .pl(px(indent))
                    .gap_1()
                    .children(caret)
                    .child(Label::new(text).size(LabelSize::Small)),
            );
        }
        v_flex()
            .id("plan-tree-view")
            .debug_selector(|| "EXPLAIN_TREE_VIEW".to_string())
            .gap_0p5()
            .children(row_elements)
    }

    fn render_flame_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        const ROW_HEIGHT: f32 = 24.0;
        let spans = flame_layout(&self.roots);
        let depth_count = spans.iter().map(|span| span.depth + 1).max().unwrap_or(0);
        let base = cx.theme().colors().editor_background;
        let accent = cx.theme().colors().text_accent;

        let mut container = div()
            .id("plan-flame-view")
            .debug_selector(|| "EXPLAIN_FLAME_VIEW".to_string())
            .relative()
            .w_full()
            .h(px(depth_count as f32 * ROW_HEIGHT));
        for (index, span) in spans.iter().enumerate() {
            let opacity = 0.15 + 0.5 * (span.depth as f32 * 0.15).min(1.0);
            container = container.child(
                div()
                    .id(("flame-span", index))
                    .debug_selector(move || format!("FLAME-{}-{}", span.depth, index))
                    .absolute()
                    .top(px(span.depth as f32 * ROW_HEIGHT))
                    .left(relative(span.start as f32))
                    .w(relative(span.width as f32))
                    .h(px(ROW_HEIGHT - 2.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(base)
                    .bg(base.blend(accent.opacity(opacity)))
                    .px_1()
                    .child(Label::new(span.text.clone()).size(LabelSize::XSmall)),
            );
        }
        div()
            .id("flame-scroll")
            .flex_1()
            .overflow_scroll()
            .child(container)
    }
}

impl Render for ExplainPlanView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.mode;
        let toolbar = h_flex()
            .gap_1()
            .child(
                div()
                    .id("plan-mode-tree-hitbox")
                    .debug_selector(|| "plan-mode-tree".to_string())
                    .child(
                        IconButton::new("plan-mode-tree", IconName::ListTree)
                            .icon_size(IconSize::XSmall)
                            .toggle_state(mode == PlanViewMode::Tree)
                            .tooltip(Tooltip::text("Tree View"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.mode != PlanViewMode::Tree {
                                    this.toggle_mode(cx);
                                }
                            })),
                    ),
            )
            .child(
                div()
                    .id("plan-mode-flame-hitbox")
                    .debug_selector(|| "plan-mode-flame".to_string())
                    .child(
                        IconButton::new("plan-mode-flame", IconName::Flame)
                            .icon_size(IconSize::XSmall)
                            .toggle_state(mode == PlanViewMode::Flame)
                            .tooltip(Tooltip::text("Flame View"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.mode != PlanViewMode::Flame {
                                    this.toggle_mode(cx);
                                }
                            })),
                    ),
            )
            .child(
                div()
                    .id("plan-copy-native-hitbox")
                    .debug_selector(|| "plan-copy-native".to_string())
                    .child(
                        IconButton::new("plan-copy-native", IconName::Copy)
                            .icon_size(IconSize::XSmall)
                            .disabled(self.query_context.is_none())
                            .tooltip(Tooltip::text("Copy Native Format"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.copy_native_format(window, cx);
                            })),
                    ),
            );

        let body: AnyElement = match mode {
            PlanViewMode::Tree => div()
                .id("plan-scroll")
                .flex_1()
                .overflow_y_scroll()
                .child(self.render_tree_body(cx))
                .into_any_element(),
            PlanViewMode::Flame => self.render_flame_body(cx).into_any_element(),
        };

        v_flex()
            .key_context("ExplainPlan")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .size_full()
            .p_3()
            .gap_2()
            .child(crate::widgets::dialog_header(
                "Query Plan",
                "close-explain",
                cx.listener(|_, _, _, cx| cx.emit(ExplainPlanEvent::Dismissed)),
            ))
            .child(toolbar)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_builds_hierarchy_from_indentation() {
        let text = "Limit\n  ->  Sort\n        ->  Seq Scan on t";
        let roots = parse_plan_tree(text);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].text, "Limit");
        assert_eq!(roots[0].children.len(), 1);
        assert_eq!(roots[0].children[0].text, "Sort");
        assert_eq!(roots[0].children[0].children[0].text, "Seq Scan on t");
    }

    #[test]
    fn parse_handles_siblings_at_same_depth() {
        let text = "Nested Loop\n  ->  Scan a\n  ->  Scan b";
        let roots = parse_plan_tree(text);
        assert_eq!(roots[0].children.len(), 2);
        assert_eq!(roots[0].children[0].text, "Scan a");
        assert_eq!(roots[0].children[1].text, "Scan b");
    }

    #[test]
    fn parse_single_node() {
        let roots = parse_plan_tree("Seq Scan on users");
        assert_eq!(roots.len(), 1);
        assert!(roots[0].children.is_empty());
        assert_eq!(roots[0].text, "Seq Scan on users");
    }

    #[test]
    fn parse_empty_input_is_empty() {
        assert!(parse_plan_tree("").is_empty());
        assert!(parse_plan_tree("   \n\n").is_empty());
    }

    #[test]
    fn explain_sql_is_driver_specific() {
        assert_eq!(
            explain_sql_for_driver(DatabaseDriver::MySQL, "SELECT 1;"),
            "EXPLAIN FORMAT=TREE SELECT 1"
        );
        assert_eq!(
            explain_sql_for_driver(DatabaseDriver::SQLite, "SELECT 1"),
            "EXPLAIN QUERY PLAN SELECT 1"
        );
        assert_eq!(
            explain_sql_for_driver(DatabaseDriver::PostgreSQL, "SELECT 1"),
            "EXPLAIN SELECT 1"
        );
    }

    #[test]
    fn native_explain_sql_uses_json_format_where_supported() {
        assert_eq!(
            native_explain_sql_for_driver(DatabaseDriver::MySQL, "SELECT 1;"),
            "EXPLAIN FORMAT=JSON SELECT 1"
        );
        assert_eq!(
            native_explain_sql_for_driver(DatabaseDriver::PostgreSQL, "SELECT 1"),
            "EXPLAIN (FORMAT JSON) SELECT 1"
        );
        // SQLite exposes no distinct machine-readable format, so it falls back
        // to the same statement the tree view already uses.
        assert_eq!(
            native_explain_sql_for_driver(DatabaseDriver::SQLite, "SELECT 1"),
            "EXPLAIN QUERY PLAN SELECT 1"
        );
    }

    #[test]
    fn plan_node_weight_prefers_rows_over_cost() {
        let node = PlanNode {
            text: "Seq Scan on t  (cost=0.00..12.50 rows=5 width=8)".to_string(),
            children: Vec::new(),
        };
        assert_eq!(plan_node_weight(&node), 5.0);
    }

    #[test]
    fn plan_node_weight_falls_back_to_cost_upper_bound() {
        let node = PlanNode {
            text: "Seq Scan on t  (cost=0.00..12.50 width=8)".to_string(),
            children: Vec::new(),
        };
        assert_eq!(plan_node_weight(&node), 12.50);
    }

    #[test]
    fn plan_node_weight_falls_back_to_uniform_when_no_metric_present() {
        let node = PlanNode {
            text: "SCAN t".to_string(),
            children: Vec::new(),
        };
        assert_eq!(plan_node_weight(&node), 1.0);
    }

    #[test]
    fn flame_layout_sizes_spans_proportionally_to_row_estimates() {
        let roots = vec![PlanNode {
            text: "Root (rows=100)".to_string(),
            children: vec![
                PlanNode {
                    text: "Child A (rows=40)".to_string(),
                    children: Vec::new(),
                },
                PlanNode {
                    text: "Child B (rows=60)".to_string(),
                    children: Vec::new(),
                },
            ],
        }];
        let spans = flame_layout(&roots);
        assert_eq!(spans.len(), 3);

        assert_eq!(spans[0].depth, 0);
        assert!((spans[0].start - 0.0).abs() < 1e-9);
        assert!((spans[0].width - 1.0).abs() < 1e-9);

        assert_eq!(spans[1].depth, 1);
        assert!((spans[1].start - 0.0).abs() < 1e-9);
        assert!((spans[1].width - 0.4).abs() < 1e-9);

        assert_eq!(spans[2].depth, 1);
        assert!((spans[2].start - 0.4).abs() < 1e-9);
        assert!((spans[2].width - 0.6).abs() < 1e-9);
    }

    fn sample_roots() -> Vec<PlanNode> {
        vec![PlanNode {
            text: "Root (rows=100)".to_string(),
            children: vec![
                PlanNode {
                    text: "Child A (rows=40)".to_string(),
                    children: Vec::new(),
                },
                PlanNode {
                    text: "Child B (rows=60)".to_string(),
                    children: Vec::new(),
                },
            ],
        }]
    }

    fn init_test(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    struct ExplainMockProvider {
        queries: std::sync::Mutex<Vec<String>>,
    }

    impl ExplainMockProvider {
        fn new() -> Self {
            Self {
                queries: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl db_client::provider::DbProvider for ExplainMockProvider {
        async fn ping(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_databases(&self) -> anyhow::Result<Vec<db_client::schema::DatabaseInfo>> {
            Ok(Vec::new())
        }
        async fn list_tables(
            &self,
            _database: &str,
        ) -> anyhow::Result<Vec<db_client::schema::TableInfo>> {
            Ok(Vec::new())
        }
        async fn describe_table(
            &self,
            _database: &str,
            _table: &str,
        ) -> anyhow::Result<Vec<db_client::schema::ColumnInfo>> {
            Ok(Vec::new())
        }
        async fn execute_query(&self, _database: &str, sql: &str) -> anyhow::Result<QueryResult> {
            self.queries.lock().unwrap().push(sql.to_string());
            Ok(QueryResult {
                columns: vec!["QUERY PLAN".to_string()],
                rows: vec![vec![Some(r#"[{"Plan": {"Node Type": "Seq Scan"}}]"#.to_string())]],
                rows_affected: 0,
                execution_time_ms: 1,
            })
        }
        async fn get_table_ddl(&self, _database: &str, _table: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    #[gpui::test]
    fn toggling_to_flame_view_renders_proportional_spans(cx: &mut gpui::TestAppContext) {
        init_test(cx);
        let store = cx.new(DatabaseStore::new);
        let window = cx.add_window(|window, cx| {
            ExplainPlanView::new(sample_roots(), None, window, cx)
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        let _ = store;

        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("EXPLAIN_TREE_VIEW").is_some(),
            "the view opens in tree mode by default"
        );
        assert!(cx.debug_bounds("EXPLAIN_FLAME_VIEW").is_none());

        let target = cx
            .debug_bounds("plan-mode-flame")
            .map(|bounds| bounds.center())
            .expect("flame toggle button should render");
        cx.simulate_click(target, gpui::Modifiers::none());
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("EXPLAIN_FLAME_VIEW").is_some(),
            "clicking the flame toggle must switch to the flame view"
        );
        assert!(cx.debug_bounds("EXPLAIN_TREE_VIEW").is_none());

        let root_bounds = cx
            .debug_bounds("FLAME-0-0")
            .expect("the root span should render");
        let child_a_bounds = cx
            .debug_bounds("FLAME-1-1")
            .expect("child A's span should render");
        let child_b_bounds = cx
            .debug_bounds("FLAME-1-2")
            .expect("child B's span should render");
        // Root (rows=100) spans the full width; Child A (rows=40) and Child B
        // (rows=60) split it 40/60, so B must be the wider of the two.
        assert!(child_b_bounds.size.width > child_a_bounds.size.width);
        assert!(child_a_bounds.size.width < root_bounds.size.width);
        assert!(child_b_bounds.size.width < root_bounds.size.width);
    }

    #[gpui::test]
    fn toggling_back_to_tree_view_still_works(cx: &mut gpui::TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| {
            ExplainPlanView::new(sample_roots(), None, window, cx)
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);

        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let target = cx
            .debug_bounds("plan-mode-flame")
            .map(|bounds| bounds.center())
            .expect("flame toggle button should render");
        cx.simulate_click(target, gpui::Modifiers::none());
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("EXPLAIN_FLAME_VIEW").is_some());

        let target = cx
            .debug_bounds("plan-mode-tree")
            .map(|bounds| bounds.center())
            .expect("tree toggle button should render");
        cx.simulate_click(target, gpui::Modifiers::none());
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("EXPLAIN_TREE_VIEW").is_some(),
            "toggling back to tree mode must render the tree view again"
        );
        assert!(cx.debug_bounds("EXPLAIN_FLAME_VIEW").is_none());
    }

    #[gpui::test]
    async fn copy_native_format_issues_the_native_query_and_writes_the_clipboard(
        cx: &mut gpui::TestAppContext,
    ) {
        init_test(cx);
        let config = db_client::ConnectionConfig {
            label: "explain".to_string(),
            database: Some("scratch".to_string()),
            driver: DatabaseDriver::PostgreSQL,
            auto_connect: false,
            ..Default::default()
        };
        let connection_id = config.id;
        let provider = std::sync::Arc::new(ExplainMockProvider::new());

        let store = cx.new(DatabaseStore::new);
        store.update(cx, |store, cx| {
            store.add_connected_for_test(config, provider.clone(), cx);
        });

        let window = cx.add_window(|window, cx| {
            ExplainPlanView::new(
                sample_roots(),
                Some(ExplainQueryContext {
                    store: store.clone(),
                    connection_id,
                    database: "scratch".to_string(),
                    driver: DatabaseDriver::PostgreSQL,
                    sql: "SELECT 1;".to_string(),
                }),
                window,
                cx,
            )
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let target = cx
            .debug_bounds("plan-copy-native")
            .map(|bounds| bounds.center())
            .expect("copy-native button should render");
        cx.simulate_click(target, gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            provider.queries.lock().unwrap().as_slice(),
            ["EXPLAIN (FORMAT JSON) SELECT 1".to_string()],
            "Copy Native Format must re-run the query with the driver's native format"
        );
        let clipboard = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("the native plan text should be on the clipboard");
        assert!(clipboard.contains(r#""Node Type": "Seq Scan""#));
    }
}
