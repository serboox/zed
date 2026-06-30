use db_client::{DatabaseDriver, QueryResult};
use gpui::{App, Context, EventEmitter, FocusHandle, Focusable, Window, div, px};
use std::collections::HashSet;
use ui::prelude::*;
use ui::{IconButton, IconName, IconSize, Label, LabelSize, h_flex, v_flex};

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

pub enum ExplainPlanEvent {
    Dismissed,
}

pub struct ExplainPlanView {
    focus_handle: FocusHandle,
    roots: Vec<PlanNode>,
    collapsed: HashSet<usize>,
}

impl ExplainPlanView {
    pub fn new(roots: Vec<PlanNode>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            roots,
            collapsed: HashSet::new(),
        }
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

impl Focusable for ExplainPlanView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExplainPlanView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        let body = v_flex().gap_0p5().children(row_elements);

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
            .child(div().id("plan-scroll").flex_1().overflow_y_scroll().child(body))
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
}
