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
    let trimmed = line
        .trim_start_matches([' ', '\t', '|', '`', '+'])
        .trim_start();
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

    roots
        .iter()
        .map(|&index| build_node(&arena, index))
        .collect()
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
        DatabaseDriver::ClickHouse => format!("EXPLAIN {sql}"),
        _ => format!("EXPLAIN {sql}"),
    }
}

/// Whether `driver` has a real EXPLAIN/query-plan concept that
/// `explain_sql_for_driver` can express as a runnable statement. CQL has no
/// query planner at all, MongoDB's explain is a `.explain()` cursor call
/// (nothing like `EXPLAIN <text>`), and Redis's command protocol has no
/// query-plan concept — "Explain Query" is unavailable for all three rather
/// than sending them a statement that can only fail.
pub fn supports_explain_plan(driver: DatabaseDriver) -> bool {
    matches!(
        driver,
        DatabaseDriver::MySQL
            | DatabaseDriver::PostgreSQL
            | DatabaseDriver::SQLite
            | DatabaseDriver::ClickHouse
    )
}

/// Returns the EXPLAIN ANALYZE statement for drivers that actually run the
/// query and report real execution statistics, or `None` otherwise. Verified
/// against each engine's docs: MySQL and PostgreSQL both support `EXPLAIN
/// ANALYZE`. ClickHouse's EXPLAIN never executes the query (no ANALYZE
/// variant exists), and SQLite's EXPLAIN QUERY PLAN is estimate-only.
pub fn explain_analyze_sql_for_driver(driver: DatabaseDriver, sql: &str) -> Option<String> {
    let sql = sql.trim().trim_end_matches(';');
    match driver {
        DatabaseDriver::MySQL | DatabaseDriver::PostgreSQL => {
            Some(format!("EXPLAIN ANALYZE {sql}"))
        }
        _ => None,
    }
}

/// Whether `driver` supports [`explain_analyze_sql_for_driver`].
pub fn supports_explain_analyze(driver: DatabaseDriver) -> bool {
    matches!(driver, DatabaseDriver::MySQL | DatabaseDriver::PostgreSQL)
}

/// Whether `sql` is itself an `EXPLAIN ANALYZE` statement a user typed
/// directly into a console, so an inline-detected plan result can still show
/// the "Analyze" title even without a driver-level `ExplainQueryContext`.
pub fn sql_requests_analyze(sql: &str) -> bool {
    let upper = sql.trim_start().to_uppercase();
    upper.starts_with("EXPLAIN") && upper.contains("ANALYZE")
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

/// The native-format counterpart of [`explain_analyze_sql_for_driver`], for
/// "Copy Native Format" on an analyzed plan. PostgreSQL exposes a distinct
/// `FORMAT JSON` that still runs the query (`ANALYZE, FORMAT JSON`). MySQL's
/// `EXPLAIN ANALYZE` only supports the TREE format — there is no JSON
/// variant to fall back to — so its native format is the same tree text the
/// view already parsed.
pub fn native_explain_analyze_sql_for_driver(driver: DatabaseDriver, sql: &str) -> Option<String> {
    let trimmed = sql.trim().trim_end_matches(';');
    match driver {
        DatabaseDriver::PostgreSQL => Some(format!("EXPLAIN (ANALYZE, FORMAT JSON) {trimmed}")),
        DatabaseDriver::MySQL => Some(format!("EXPLAIN ANALYZE {trimmed}")),
        _ => None,
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
    parse_finite_metric(segment.rsplit("..").next().unwrap_or(segment))
}

fn extract_actual_time_upper_bound(text: &str) -> Option<f64> {
    let start = text.find("actual time=")? + "actual time=".len();
    let rest = &text[start..];
    let end = rest.find([' ', ')']).unwrap_or(rest.len());
    let segment = &rest[..end];
    parse_finite_metric(segment.rsplit("..").next().unwrap_or(segment))
}

// `f64::parse` accepts "inf"/"nan", so a malformed plan line (e.g.
// `cost=0.00..inf`) would otherwise yield a non-finite metric that turns the
// heat ratio into `inf/inf == NaN` and poisons the color/opacity math. Reject
// non-finite values here so both the heat tint and the flame-width weight only
// ever see real numbers.
fn parse_finite_metric(segment: &str) -> Option<f64> {
    segment
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

/// A node's heat metric for cost coloring: the measured execution time
/// (`actual time=A..B`) when the plan was run with ANALYZE, else the estimated
/// cost upper bound (`cost=X..Y`). `None` when the engine reports neither.
fn plan_node_heat(text: &str) -> Option<f64> {
    extract_actual_time_upper_bound(text)
        .or_else(|| extract_cost_upper_bound(text))
        .filter(|value| *value > 0.0)
}

/// The largest heat metric across the whole plan forest, used to normalize each
/// node's tint. `None` when no node reports a cost/time metric.
fn max_plan_heat(roots: &[PlanNode]) -> Option<f64> {
    fn walk(node: &PlanNode, max: &mut Option<f64>) {
        if let Some(value) = plan_node_heat(&node.text) {
            *max = Some(max.map_or(value, |current: f64| current.max(value)));
        }
        for child in &node.children {
            walk(child, max);
        }
    }
    let mut max = None;
    for root in roots {
        walk(root, &mut max);
    }
    max
}

/// Normalized 0.0..=1.0 heat for one node given the forest maximum, so the
/// hottest node maps to 1.0 and a node without a metric maps to 0.0.
fn heat_fraction(text: &str, max: Option<f64>) -> f32 {
    match (plan_node_heat(text), max) {
        (Some(value), Some(max)) if max > 0.0 => (value / max).clamp(0.0, 1.0) as f32,
        _ => 0.0,
    }
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
    is_analyze: bool,
    selected_row: Option<usize>,
}

impl ExplainPlanView {
    pub fn new(
        roots: Vec<PlanNode>,
        query_context: Option<ExplainQueryContext>,
        is_analyze: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            roots,
            collapsed: HashSet::new(),
            mode: PlanViewMode::Tree,
            query_context,
            is_analyze,
            selected_row: None,
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
        let native_sql = if self.is_analyze {
            native_explain_analyze_sql_for_driver(context.driver, &context.sql)
                .unwrap_or_else(|| native_explain_sql_for_driver(context.driver, &context.sql))
        } else {
            native_explain_sql_for_driver(context.driver, &context.sql)
        };
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

    fn select_row(&mut self, id: usize, cx: &mut Context<Self>) {
        self.selected_row = Some(id);
        cx.notify();
    }
}

impl EventEmitter<DismissEvent> for ExplainPlanView {}

impl Item for ExplainPlanView {
    type Event = DismissEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        if self.is_analyze {
            "Query Plan (Analyze)".into()
        } else {
            "Query Plan".into()
        }
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
        let heat = cx.theme().status().error;
        let hover_bg = cx.theme().colors().element_hover;
        let selected_bg = cx.theme().colors().element_selected;
        let max_heat = max_plan_heat(&self.roots);
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
            let is_selected = self.selected_row == Some(id);
            // Translucent red proportional to cost so it composites over the
            // panel background; a zero-cost row stays fully transparent.
            let heat_bg = heat.opacity(0.35 * heat_fraction(&text, max_heat));
            row_elements.push(
                h_flex()
                    .id(("plan-row", id))
                    .debug_selector(move || format!("plan-row-{id}"))
                    .w_full()
                    .pl(px(indent))
                    .gap_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(is_selected, |row| row.bg(selected_bg))
                    .when(!is_selected, |row| {
                        row.bg(heat_bg).hover(|row| row.bg(hover_bg))
                    })
                    .children(caret)
                    .child(Label::new(text).size(LabelSize::Small))
                    .on_click(cx.listener(move |this, _, _, cx| this.select_row(id, cx))),
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
        let border = cx.theme().colors().border;
        let heat = cx.theme().status().error;
        let max_heat = max_plan_heat(&self.roots);

        let mut container = div()
            .id("plan-flame-view")
            .debug_selector(|| "EXPLAIN_FLAME_VIEW".to_string())
            .relative()
            .w_full()
            .h(px(depth_count as f32 * ROW_HEIGHT));
        for (index, span) in spans.iter().enumerate() {
            // Opaque box tinted red by cost; even the coldest node keeps a faint
            // tint so its outline stays legible.
            let alpha = 0.12 + 0.55 * heat_fraction(&span.text, max_heat);
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
                    .border_color(border)
                    .bg(base.blend(heat.opacity(alpha)))
                    .px_1()
                    .tooltip(Tooltip::text(span.text.clone()))
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
            .id("explain-plan-toolbar")
            .debug_selector(|| "EXPLAIN_PLAN_TOOLBAR".to_string())
            .w_full()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
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
            .debug_selector(|| "EXPLAIN_PLAN_ROOT".to_string())
            .size_full()
            .bg(cx.theme().colors().editor_background)
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
        assert_eq!(
            explain_sql_for_driver(DatabaseDriver::ClickHouse, "SELECT 1"),
            "EXPLAIN SELECT 1"
        );
    }

    // Cassandra/MongoDB/Redis have no EXPLAIN/query-plan concept expressible
    // as a runnable statement -- before this fix, invoking "Explain Query"
    // against one of them fell through to the generic `EXPLAIN <text>`
    // fallback and sent a statement that could only error.
    #[test]
    fn only_drivers_with_a_real_query_plan_concept_support_explain() {
        for driver in [
            DatabaseDriver::MySQL,
            DatabaseDriver::PostgreSQL,
            DatabaseDriver::SQLite,
            DatabaseDriver::ClickHouse,
        ] {
            assert!(
                supports_explain_plan(driver),
                "{driver:?} should support Explain Query"
            );
        }
        for driver in [
            DatabaseDriver::Cassandra,
            DatabaseDriver::MongoDB,
            DatabaseDriver::Redis,
            DatabaseDriver::Aerospike,
        ] {
            assert!(
                !supports_explain_plan(driver),
                "{driver:?} should not support Explain Query"
            );
        }
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
    fn explain_analyze_sql_is_only_offered_for_engines_that_actually_run_the_query() {
        assert_eq!(
            explain_analyze_sql_for_driver(DatabaseDriver::MySQL, "SELECT 1;"),
            Some("EXPLAIN ANALYZE SELECT 1".to_string())
        );
        assert_eq!(
            explain_analyze_sql_for_driver(DatabaseDriver::PostgreSQL, "SELECT 1"),
            Some("EXPLAIN ANALYZE SELECT 1".to_string())
        );
        // ClickHouse's EXPLAIN never executes the query (no ANALYZE variant),
        // and SQLite's EXPLAIN QUERY PLAN is estimate-only.
        assert_eq!(
            explain_analyze_sql_for_driver(DatabaseDriver::ClickHouse, "SELECT 1"),
            None
        );
        assert_eq!(
            explain_analyze_sql_for_driver(DatabaseDriver::SQLite, "SELECT 1"),
            None
        );

        assert!(supports_explain_analyze(DatabaseDriver::MySQL));
        assert!(supports_explain_analyze(DatabaseDriver::PostgreSQL));
        assert!(!supports_explain_analyze(DatabaseDriver::ClickHouse));
        assert!(!supports_explain_analyze(DatabaseDriver::SQLite));
    }

    #[test]
    fn native_explain_analyze_sql_falls_back_to_tree_text_where_no_json_variant_exists() {
        // PostgreSQL has a distinct FORMAT JSON that still runs the query.
        assert_eq!(
            native_explain_analyze_sql_for_driver(DatabaseDriver::PostgreSQL, "SELECT 1;"),
            Some("EXPLAIN (ANALYZE, FORMAT JSON) SELECT 1".to_string())
        );
        // MySQL's EXPLAIN ANALYZE only supports the TREE format.
        assert_eq!(
            native_explain_analyze_sql_for_driver(DatabaseDriver::MySQL, "SELECT 1"),
            Some("EXPLAIN ANALYZE SELECT 1".to_string())
        );
        assert_eq!(
            native_explain_analyze_sql_for_driver(DatabaseDriver::ClickHouse, "SELECT 1"),
            None
        );
    }

    #[test]
    fn sql_requests_analyze_detects_the_keyword_after_explain() {
        assert!(sql_requests_analyze("EXPLAIN ANALYZE SELECT 1"));
        assert!(sql_requests_analyze("  explain analyze select 1"));
        assert!(sql_requests_analyze("EXPLAIN FORMAT=TREE ANALYZE SELECT 1"));
        assert!(!sql_requests_analyze("EXPLAIN SELECT 1"));
        assert!(!sql_requests_analyze("SELECT 1"));
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
                rows: vec![vec![Some(
                    r#"[{"Plan": {"Node Type": "Seq Scan"}}]"#.to_string(),
                )]],
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
        let window = cx
            .add_window(|window, cx| ExplainPlanView::new(sample_roots(), None, false, window, cx));
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
        let window = cx
            .add_window(|window, cx| ExplainPlanView::new(sample_roots(), None, false, window, cx));
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
                false,
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

    #[gpui::test]
    async fn analyze_view_labels_its_tab_and_copies_the_analyze_native_format(
        cx: &mut gpui::TestAppContext,
    ) {
        init_test(cx);
        let config = db_client::ConnectionConfig {
            label: "explain".to_string(),
            database: Some("scratch".to_string()),
            driver: DatabaseDriver::MySQL,
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
                    driver: DatabaseDriver::MySQL,
                    sql: "SELECT 1;".to_string(),
                }),
                true,
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

        let tab_title = window
            .read_with(&cx, |view, cx| view.tab_content_text(0, cx))
            .expect("view should still be alive");
        assert_eq!(tab_title.as_ref(), "Query Plan (Analyze)");

        let target = cx
            .debug_bounds("plan-copy-native")
            .map(|bounds| bounds.center())
            .expect("copy-native button should render");
        cx.simulate_click(target, gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            provider.queries.lock().unwrap().as_slice(),
            ["EXPLAIN ANALYZE SELECT 1".to_string()],
            "Copy Native Format must re-run the query with MySQL's analyze-tree statement"
        );
    }

    #[test]
    fn plan_node_heat_prefers_actual_time_over_estimated_cost() {
        let text =
            "-> Seq Scan on t  (cost=0.00..12.50 rows=5) (actual time=0.100..3.250 rows=5 loops=1)";
        assert_eq!(plan_node_heat(text), Some(3.25));
    }

    #[test]
    fn plan_node_heat_falls_back_to_cost_upper_bound() {
        let text = "Seq Scan on t  (cost=0.00..42.00 rows=5 width=8)";
        assert_eq!(plan_node_heat(text), Some(42.0));
    }

    #[test]
    fn plan_node_heat_is_none_without_metrics() {
        assert_eq!(plan_node_heat("SCAN t"), None);
    }

    #[test]
    fn heat_metrics_reject_non_finite_values() {
        // `f64::parse` accepts "inf", so without a guard these lines would
        // yield Some(inf); inf/inf then turns the normalized fraction into NaN
        // and corrupts the opacity/blend color math.
        let inf_cost = "Seq Scan (cost=0.00..inf rows=1)";
        let inf_time = "Seq Scan (actual time=0.001..inf rows=1 loops=1)";
        assert_eq!(plan_node_heat(inf_cost), None);
        assert_eq!(plan_node_heat(inf_time), None);

        let roots = vec![PlanNode {
            text: inf_cost.to_string(),
            children: Vec::new(),
        }];
        let max = max_plan_heat(&roots);
        assert_eq!(max, None);
        let fraction = heat_fraction(inf_cost, max);
        assert!(
            fraction.is_finite(),
            "a non-finite metric must not leak a NaN heat fraction into the color math"
        );
        assert_eq!(fraction, 0.0);
    }

    #[test]
    fn heat_fraction_normalizes_against_the_forest_maximum() {
        let roots = vec![PlanNode {
            text: "Root (cost=0.00..100.00)".to_string(),
            children: vec![PlanNode {
                text: "Child (cost=0.00..25.00)".to_string(),
                children: Vec::new(),
            }],
        }];
        let max = max_plan_heat(&roots);
        assert_eq!(max, Some(100.0));
        assert!((heat_fraction("Root (cost=0.00..100.00)", max) - 1.0).abs() < 1e-6);
        assert!((heat_fraction("Child (cost=0.00..25.00)", max) - 0.25).abs() < 1e-6);
        assert!(heat_fraction("no metric", max).abs() < 1e-6);
    }

    #[gpui::test]
    fn plan_renders_as_a_flat_panel_without_a_dialog_header(cx: &mut gpui::TestAppContext) {
        init_test(cx);
        let window = cx
            .add_window(|window, cx| ExplainPlanView::new(sample_roots(), None, false, window, cx));
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let root = cx
            .debug_bounds("EXPLAIN_PLAN_ROOT")
            .expect("the plan panel root should render");
        let toolbar = cx
            .debug_bounds("EXPLAIN_PLAN_TOOLBAR")
            .expect("the toolbar should render");
        // The toolbar is the panel's first child: with the dialog header gone,
        // no large title/close row sits above it, so it starts flush with the
        // panel top (only a hairline border may separate them).
        let offset = f32::from(toolbar.origin.y) - f32::from(root.origin.y);
        assert!(
            offset < 8.0,
            "toolbar should sit flush at the panel top, got offset {offset}"
        );
    }

    #[gpui::test]
    fn clicking_a_tree_row_selects_it(cx: &mut gpui::TestAppContext) {
        init_test(cx);
        let window = cx
            .add_window(|window, cx| ExplainPlanView::new(sample_roots(), None, false, window, cx));
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        assert_eq!(
            window
                .read_with(&cx, |view, _| view.selected_row)
                .expect("view should be alive"),
            None,
            "no row is selected before any interaction"
        );

        // Child A is the second row (id 1) and is a leaf, so its center lands on
        // the row body rather than on a disclosure caret.
        let target = cx
            .debug_bounds("plan-row-1")
            .map(|bounds| bounds.center())
            .expect("the second plan row should render");
        cx.simulate_click(target, gpui::Modifiers::none());
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        assert_eq!(
            window
                .read_with(&cx, |view, _| view.selected_row)
                .expect("view should be alive"),
            Some(1),
            "clicking a plan row must select it"
        );
    }
}
