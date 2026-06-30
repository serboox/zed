use db_client::schema::QueryResult;
use gpui::{Context, EventEmitter, FocusHandle, Focusable, Window, prelude::*};
use std::collections::{HashMap, VecDeque};
use ui::{Divider, Tooltip, prelude::*};

const RENDERED_ROW_LIMIT: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowDiffKind {
    Added,
    Removed,
    Changed,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowDiff {
    pub kind: RowDiffKind,
    pub left_row: Option<usize>,
    pub right_row: Option<usize>,
    pub changed_columns: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffResult {
    pub columns: Vec<String>,
    pub rows: Vec<RowDiff>,
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub columns_aligned: bool,
}

struct AlignedColumns {
    names: Vec<String>,
    left_index: Vec<usize>,
    right_index: Vec<usize>,
    aligned: bool,
}

fn align_columns(left: &QueryResult, right: &QueryResult) -> AlignedColumns {
    if left.columns == right.columns {
        let count = left.columns.len();
        return AlignedColumns {
            names: left.columns.clone(),
            left_index: (0..count).collect(),
            right_index: (0..count).collect(),
            aligned: true,
        };
    }

    let mut names = Vec::new();
    let mut left_index = Vec::new();
    let mut right_index = Vec::new();
    for (left_position, name) in left.columns.iter().enumerate() {
        if let Some(right_position) = right.columns.iter().position(|other| other == name) {
            names.push(name.clone());
            left_index.push(left_position);
            right_index.push(right_position);
        }
    }
    AlignedColumns {
        names,
        left_index,
        right_index,
        aligned: false,
    }
}

fn project_row(row: &[Option<String>], index_map: &[usize]) -> Vec<Option<String>> {
    index_map
        .iter()
        .map(|&index| row.get(index).cloned().flatten())
        .collect()
}

fn cells_equal(left: &Option<String>, right: &Option<String>, tolerance: Option<f64>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left_value), Some(right_value)) => {
            if left_value == right_value {
                return true;
            }
            if let Some(epsilon) = tolerance
                && let (Ok(left_number), Ok(right_number)) =
                    (left_value.trim().parse::<f64>(), right_value.trim().parse::<f64>())
            {
                return (left_number - right_number).abs() <= epsilon;
            }
            false
        }
        _ => false,
    }
}

fn key_of(projected: &[Option<String>], key_columns: &[usize]) -> Vec<Option<String>> {
    key_columns
        .iter()
        .map(|&index| projected.get(index).cloned().flatten())
        .collect()
}

/// Compares two result sets. With `key_columns` (indices into the aligned
/// column set) rows are matched by key and differing rows are reported as
/// `Changed`; without a key, rows are matched as a multiset of values and only
/// `Added`/`Removed`/`Unchanged` are produced. `tolerance`, when set, treats two
/// numeric cells within `epsilon` as equal.
pub fn compute_diff(
    left: &QueryResult,
    right: &QueryResult,
    key_columns: Option<&[usize]>,
    tolerance: Option<f64>,
) -> DiffResult {
    let columns = align_columns(left, right);
    let left_rows: Vec<Vec<Option<String>>> = left
        .rows
        .iter()
        .map(|row| project_row(row, &columns.left_index))
        .collect();
    let right_rows: Vec<Vec<Option<String>>> = right
        .rows
        .iter()
        .map(|row| project_row(row, &columns.right_index))
        .collect();

    let mut rows = Vec::new();
    let (mut added, mut removed, mut changed, mut unchanged) = (0usize, 0usize, 0usize, 0usize);

    match key_columns {
        Some(keys) => {
            let mut right_by_key: HashMap<Vec<Option<String>>, VecDeque<usize>> = HashMap::new();
            for (index, projected) in right_rows.iter().enumerate() {
                right_by_key
                    .entry(key_of(projected, keys))
                    .or_default()
                    .push_back(index);
            }

            let mut matched_right = vec![false; right_rows.len()];
            for (left_index, left_projected) in left_rows.iter().enumerate() {
                let key = key_of(left_projected, keys);
                if let Some(queue) = right_by_key.get_mut(&key)
                    && let Some(right_index) = queue.pop_front()
                {
                    matched_right[right_index] = true;
                    let right_projected = &right_rows[right_index];
                    let mut changed_columns = Vec::new();
                    for column in 0..columns.names.len() {
                        let left_cell = left_projected.get(column).cloned().flatten();
                        let right_cell = right_projected.get(column).cloned().flatten();
                        if !cells_equal(&left_cell, &right_cell, tolerance) {
                            changed_columns.push(column);
                        }
                    }
                    if changed_columns.is_empty() {
                        unchanged += 1;
                        rows.push(RowDiff {
                            kind: RowDiffKind::Unchanged,
                            left_row: Some(left_index),
                            right_row: Some(right_index),
                            changed_columns,
                        });
                    } else {
                        changed += 1;
                        rows.push(RowDiff {
                            kind: RowDiffKind::Changed,
                            left_row: Some(left_index),
                            right_row: Some(right_index),
                            changed_columns,
                        });
                    }
                } else {
                    removed += 1;
                    rows.push(RowDiff {
                        kind: RowDiffKind::Removed,
                        left_row: Some(left_index),
                        right_row: None,
                        changed_columns: Vec::new(),
                    });
                }
            }

            for (right_index, matched) in matched_right.iter().enumerate() {
                if !matched {
                    added += 1;
                    rows.push(RowDiff {
                        kind: RowDiffKind::Added,
                        left_row: None,
                        right_row: Some(right_index),
                        changed_columns: Vec::new(),
                    });
                }
            }
        }
        None => {
            let mut right_by_value: HashMap<Vec<Option<String>>, VecDeque<usize>> = HashMap::new();
            for (index, projected) in right_rows.iter().enumerate() {
                right_by_value
                    .entry(projected.clone())
                    .or_default()
                    .push_back(index);
            }

            let mut matched_right = vec![false; right_rows.len()];
            for (left_index, left_projected) in left_rows.iter().enumerate() {
                if let Some(queue) = right_by_value.get_mut(left_projected)
                    && let Some(right_index) = queue.pop_front()
                {
                    matched_right[right_index] = true;
                    unchanged += 1;
                    rows.push(RowDiff {
                        kind: RowDiffKind::Unchanged,
                        left_row: Some(left_index),
                        right_row: Some(right_index),
                        changed_columns: Vec::new(),
                    });
                } else {
                    removed += 1;
                    rows.push(RowDiff {
                        kind: RowDiffKind::Removed,
                        left_row: Some(left_index),
                        right_row: None,
                        changed_columns: Vec::new(),
                    });
                }
            }

            for (right_index, matched) in matched_right.iter().enumerate() {
                if !matched {
                    added += 1;
                    rows.push(RowDiff {
                        kind: RowDiffKind::Added,
                        left_row: None,
                        right_row: Some(right_index),
                        changed_columns: Vec::new(),
                    });
                }
            }
        }
    }

    DiffResult {
        columns: columns.names,
        rows,
        added,
        removed,
        changed,
        unchanged,
        columns_aligned: columns.aligned,
    }
}

pub enum CompareDataEvent {
    Dismissed,
}

pub struct CompareDataView {
    focus_handle: FocusHandle,
    left: QueryResult,
    right: QueryResult,
    diff: DiffResult,
}

impl CompareDataView {
    pub fn new(
        left: QueryResult,
        right: QueryResult,
        key_columns: Option<Vec<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let diff = compute_diff(&left, &right, key_columns.as_deref(), None);
        Self {
            focus_handle: cx.focus_handle(),
            left,
            right,
            diff,
        }
    }

    fn cell_text(&self, row: &RowDiff, column: usize) -> Option<String> {
        let source = match row.kind {
            RowDiffKind::Removed => self
                .left
                .rows
                .get(row.left_row?)
                .map(|values| (values, &self.left.columns)),
            _ => row
                .right_row
                .and_then(|index| self.right.rows.get(index))
                .map(|values| (values, &self.right.columns))
                .or_else(|| {
                    self.left
                        .rows
                        .get(row.left_row?)
                        .map(|values| (values, &self.left.columns))
                }),
        }?;
        let (values, names) = source;
        let name = self.diff.columns.get(column)?;
        let index = names.iter().position(|candidate| candidate == name)?;
        values.get(index).cloned().flatten()
    }
}

impl EventEmitter<CompareDataEvent> for CompareDataView {}

impl Focusable for CompareDataView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CompareDataView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = cx.theme().status();
        let created_bg = status.created_background;
        let deleted_bg = status.deleted_background;
        let modified_bg = status.modified_background;

        let header = h_flex()
            .gap_3()
            .child(Label::new(format!("+{}", self.diff.added)).color(Color::Created))
            .child(Label::new(format!("-{}", self.diff.removed)).color(Color::Deleted))
            .child(Label::new(format!("~{}", self.diff.changed)).color(Color::Modified))
            .child(
                Label::new(format!("={}", self.diff.unchanged))
                    .color(Color::Muted),
            );

        let column_header = h_flex().gap_2().px_1().children(
            self.diff
                .columns
                .iter()
                .map(|name| {
                    div()
                        .w(px(140.))
                        .child(Label::new(name.clone()).size(LabelSize::Small).color(Color::Muted))
                })
                .collect::<Vec<_>>(),
        );

        let rows: Vec<_> = self
            .diff
            .rows
            .iter()
            .take(RENDERED_ROW_LIMIT)
            .map(|row| {
                let (row_bg, marker) = match row.kind {
                    RowDiffKind::Added => (created_bg, "+"),
                    RowDiffKind::Removed => (deleted_bg, "-"),
                    RowDiffKind::Changed => (modified_bg, "~"),
                    RowDiffKind::Unchanged => (gpui::transparent_black(), " "),
                };
                let marker_color = match row.kind {
                    RowDiffKind::Added => Color::Created,
                    RowDiffKind::Removed => Color::Deleted,
                    RowDiffKind::Changed => Color::Modified,
                    RowDiffKind::Unchanged => Color::Muted,
                };
                let cells: Vec<_> = (0..self.diff.columns.len())
                    .map(|column| {
                        let text = self.cell_text(row, column).unwrap_or_else(|| "NULL".into());
                        let is_changed = row.changed_columns.contains(&column);
                        div()
                            .w(px(140.))
                            .when(is_changed, |cell| cell.font_weight(gpui::FontWeight::BOLD))
                            .child(
                                Label::new(text)
                                    .size(LabelSize::Small)
                                    .when(is_changed, |label| label.color(Color::Modified)),
                            )
                            .into_any_element()
                    })
                    .collect();
                h_flex()
                    .w_full()
                    .gap_2()
                    .px_1()
                    .bg(row_bg)
                    .child(
                        div()
                            .w(px(14.))
                            .child(Label::new(marker).size(LabelSize::Small).color(marker_color)),
                    )
                    .children(cells)
                    .into_any_element()
            })
            .collect();

        let truncated = self.diff.rows.len() > RENDERED_ROW_LIMIT;

        v_flex()
            .key_context("CompareData")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(px(760.))
            .max_h(px(560.))
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(Label::new("Compare Data").size(LabelSize::Large))
                            .child(header),
                    )
                    .child(
                        IconButton::new("close-compare", IconName::Close)
                            .tooltip(Tooltip::text("Close"))
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(CompareDataEvent::Dismissed)),
                            ),
                    ),
            )
            .when(!self.diff.columns_aligned, |column| {
                column.child(
                    Label::new("Comparing shared columns only (column sets differ)")
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .child(Divider::horizontal())
            .child(column_header)
            .child(
                v_flex()
                    .id("compare-rows")
                    .gap_0p5()
                    .max_h(px(400.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .when(truncated, |column| {
                column.child(
                    Label::new(format!(
                        "Showing first {RENDERED_ROW_LIMIT} of {} rows",
                        self.diff.rows.len()
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(columns: &[&str], rows: Vec<Vec<Option<&str>>>) -> QueryResult {
        QueryResult {
            columns: columns.iter().map(|name| name.to_string()).collect(),
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(|cell| cell.map(|value| value.to_string())).collect())
                .collect(),
            rows_affected: 0,
            execution_time_ms: 0,
        }
    }

    fn kinds(diff: &DiffResult) -> Vec<RowDiffKind> {
        diff.rows.iter().map(|row| row.kind).collect()
    }

    #[test]
    fn keyed_diff_detects_added_removed_changed() {
        let left = result(
            &["id", "name"],
            vec![
                vec![Some("1"), Some("alice")],
                vec![Some("2"), Some("bob")],
                vec![Some("3"), Some("carol")],
            ],
        );
        let right = result(
            &["id", "name"],
            vec![
                vec![Some("1"), Some("alice")],
                vec![Some("2"), Some("robert")],
                vec![Some("4"), Some("dave")],
            ],
        );

        let diff = compute_diff(&left, &right, Some(&[0]), None);
        assert_eq!(diff.unchanged, 1);
        assert_eq!(diff.changed, 1);
        assert_eq!(diff.removed, 1);
        assert_eq!(diff.added, 1);

        let changed = diff
            .rows
            .iter()
            .find(|row| row.kind == RowDiffKind::Changed)
            .expect("a changed row");
        assert_eq!(changed.changed_columns, vec![1]);
    }

    #[test]
    fn keyless_diff_matches_on_full_row() {
        let left = result(&["a"], vec![vec![Some("x")], vec![Some("y")]]);
        let right = result(&["a"], vec![vec![Some("y")], vec![Some("z")]]);

        let diff = compute_diff(&left, &right, None, None);
        assert_eq!(kinds(&diff).iter().filter(|k| **k == RowDiffKind::Unchanged).count(), 1);
        assert_eq!(diff.removed, 1);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.changed, 0);
    }

    #[test]
    fn tolerance_treats_close_numbers_as_equal() {
        let left = result(&["id", "amount"], vec![vec![Some("1"), Some("10.00")]]);
        let right = result(&["id", "amount"], vec![vec![Some("1"), Some("10.004")]]);

        let strict = compute_diff(&left, &right, Some(&[0]), None);
        assert_eq!(strict.changed, 1);

        let lenient = compute_diff(&left, &right, Some(&[0]), Some(0.01));
        assert_eq!(lenient.changed, 0);
        assert_eq!(lenient.unchanged, 1);
    }

    #[test]
    fn mismatched_columns_compare_shared_only() {
        let left = result(&["id", "name", "extra"], vec![vec![Some("1"), Some("a"), Some("L")]]);
        let right = result(&["id", "name", "other"], vec![vec![Some("1"), Some("a"), Some("R")]]);

        let diff = compute_diff(&left, &right, Some(&[0]), None);
        assert!(!diff.columns_aligned);
        assert_eq!(diff.columns, vec!["id".to_string(), "name".to_string()]);
        assert_eq!(diff.unchanged, 1);
        assert_eq!(diff.changed, 0);
    }

    #[test]
    fn null_cells_compare_equal() {
        let left = result(&["id", "note"], vec![vec![Some("1"), None]]);
        let right = result(&["id", "note"], vec![vec![Some("1"), None]]);

        let diff = compute_diff(&left, &right, Some(&[0]), None);
        assert_eq!(diff.unchanged, 1);
        assert_eq!(diff.changed, 0);
    }
}
