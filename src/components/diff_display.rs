use asyncgit::{
	sync::diff::DiffLinePosition, DiffLine, DiffLineType, FileDiff,
};
use std::cmp;

pub(super) const MIN_SIDE_BY_SIDE_WIDTH: u16 = 100;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DiffViewMode {
	Unified,
	SideBySide,
}

#[derive(Clone, Copy)]
pub(super) struct SideCell<'a> {
	pub line: &'a DiffLine,
	raw_index: usize,
}

pub(super) struct SideBySideLine<'a> {
	pub left: Option<SideCell<'a>>,
	pub right: Option<SideCell<'a>>,
	raw_indices: Vec<usize>,
	hunk_index: Option<usize>,
}

pub(super) enum SideBySideDisplayLine<'a> {
	Header {
		line: &'a DiffLine,
		raw_index: usize,
		hunk_index: usize,
	},
	Row(SideBySideLine<'a>),
}

impl SideBySideDisplayLine<'_> {
	pub(super) const fn hunk_index(&self) -> Option<usize> {
		match self {
			Self::Header { hunk_index, .. } => Some(*hunk_index),
			Self::Row(row) => row.hunk_index,
		}
	}

	pub(super) fn raw_diff_indices(&self) -> Vec<usize> {
		match self {
			Self::Header { raw_index, .. } => vec![*raw_index],
			Self::Row(row) => row.raw_indices.clone(),
		}
	}
}

pub(super) fn build_side_by_side_lines(
	diff: &FileDiff,
) -> Vec<SideBySideDisplayLine<'_>> {
	let mut output = Vec::new();
	let mut raw_index = 0_usize;

	for (hunk_index, hunk) in diff.hunks.iter().enumerate() {
		let mut hunk_line_index = 0_usize;

		while hunk_line_index < hunk.lines.len() {
			let line = &hunk.lines[hunk_line_index];

			if line.line_type == DiffLineType::Header {
				output.push(SideBySideDisplayLine::Header {
					line,
					raw_index,
					hunk_index,
				});
				hunk_line_index += 1;
				raw_index += 1;
				continue;
			}

			if line.line_type == DiffLineType::Delete {
				let delete_start = hunk_line_index;
				while hunk_line_index < hunk.lines.len()
					&& hunk.lines[hunk_line_index].line_type
						== DiffLineType::Delete
				{
					hunk_line_index += 1;
				}

				let add_start = hunk_line_index;
				while hunk_line_index < hunk.lines.len()
					&& hunk.lines[hunk_line_index].line_type
						== DiffLineType::Add
				{
					hunk_line_index += 1;
				}

				let delete_count = add_start - delete_start;
				let add_count = hunk_line_index - add_start;
				let row_count = cmp::max(delete_count, add_count);

				for row in 0..row_count {
					let left = (row < delete_count).then(|| {
						let index = delete_start + row;
						SideCell {
							line: &hunk.lines[index],
							raw_index: raw_index
								+ (index - delete_start),
						}
					});
					let right = (row < add_count).then(|| {
						let index = add_start + row;
						SideCell {
							line: &hunk.lines[index],
							raw_index: raw_index + delete_count + row,
						}
					});
					output.push(SideBySideDisplayLine::Row(
						side_by_side_row(
							left,
							right,
							Some(hunk_index),
						),
					));
				}

				raw_index += delete_count + add_count;
				continue;
			}

			let cell = SideCell { line, raw_index };
			match line.line_type {
				DiffLineType::Add => {
					output.push(SideBySideDisplayLine::Row(
						side_by_side_row(
							None,
							Some(cell),
							Some(hunk_index),
						),
					));
				}
				_ => {
					output.push(SideBySideDisplayLine::Row(
						side_by_side_row(
							Some(cell),
							Some(cell),
							Some(hunk_index),
						),
					));
				}
			}
			hunk_line_index += 1;
			raw_index += 1;
		}
	}

	output
}

pub(super) fn side_by_side_copy_line(
	line: &SideBySideDisplayLine<'_>,
) -> String {
	match line {
		SideBySideDisplayLine::Header { line, .. } => line
			.content
			.trim_matches(|c| c == '\n' || c == '\r')
			.to_string(),
		SideBySideDisplayLine::Row(row) => {
			let left = row.left.map_or("", |cell| {
				cell.line
					.content
					.trim_matches(|c| c == '\n' || c == '\r')
			});
			let right = row.right.map_or("", |cell| {
				cell.line
					.content
					.trim_matches(|c| c == '\n' || c == '\r')
			});
			format!("{left}\t{right}")
		}
	}
}

pub(super) fn side_by_side_hunk_range(
	diff: &FileDiff,
	hunk_index: usize,
) -> Option<(usize, usize)> {
	let mut start = None;
	let mut end = None;

	for (display_index, line) in
		build_side_by_side_lines(diff).iter().enumerate()
	{
		if line.hunk_index() == Some(hunk_index) {
			start.get_or_insert(display_index);
			end = Some(display_index.saturating_add(1));
		}
	}

	start.zip(end)
}

pub(super) fn side_by_side_selected_positions<F>(
	diff: &FileDiff,
	contains: F,
) -> Vec<DiffLinePosition>
where
	F: Fn(usize) -> bool,
{
	let raw_lines: Vec<&DiffLine> = diff
		.hunks
		.iter()
		.flat_map(|hunk| hunk.lines.iter())
		.collect();

	build_side_by_side_lines(diff)
		.into_iter()
		.enumerate()
		.filter(|(i, _)| contains(*i))
		.flat_map(|(_, line)| line.raw_diff_indices())
		.filter_map(|raw_index| raw_lines.get(raw_index))
		.filter_map(|line| {
			if matches!(
				line.line_type,
				DiffLineType::Add | DiffLineType::Delete
			) {
				Some(line.position)
			} else {
				None
			}
		})
		.collect()
}

fn side_by_side_row<'a>(
	left: Option<SideCell<'a>>,
	right: Option<SideCell<'a>>,
	hunk_index: Option<usize>,
) -> SideBySideLine<'a> {
	let raw_indices = [left, right]
		.into_iter()
		.flatten()
		.map(|cell| cell.raw_index)
		.collect();

	SideBySideLine {
		left,
		right,
		raw_indices,
		hunk_index,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use asyncgit::sync::diff::Hunk;

	fn test_diff_line(
		content: &str,
		line_type: DiffLineType,
		old_lineno: Option<u32>,
		new_lineno: Option<u32>,
	) -> DiffLine {
		DiffLine {
			content: content.into(),
			line_type,
			position: DiffLinePosition {
				old_lineno,
				new_lineno,
			},
		}
	}

	fn test_file_diff(lines: Vec<DiffLine>) -> FileDiff {
		FileDiff {
			lines: lines.len(),
			hunks: vec![Hunk {
				header_hash: 0,
				lines,
			}],
			..Default::default()
		}
	}

	fn assert_side_rows(
		diff: &FileDiff,
		expected: &[(Option<&str>, Option<&str>)],
	) {
		let display_lines = build_side_by_side_lines(diff);
		assert_eq!(display_lines.len(), expected.len());

		for (line, (expected_left, expected_right)) in
			display_lines.iter().zip(expected)
		{
			let SideBySideDisplayLine::Row(row) = line else {
				panic!("expected side-by-side row");
			};

			assert_eq!(
				row.left.map(|cell| cell.line.content.as_ref()),
				*expected_left
			);
			assert_eq!(
				row.right.map(|cell| cell.line.content.as_ref()),
				*expected_right
			);
		}
	}

	#[test]
	fn side_by_side_context_lines_render_on_both_sides() {
		let diff = test_file_diff(vec![
			test_diff_line(
				"foo",
				DiffLineType::None,
				Some(1),
				Some(1),
			),
			test_diff_line(
				"bar",
				DiffLineType::None,
				Some(2),
				Some(2),
			),
		]);

		assert_side_rows(
			&diff,
			&[(Some("foo"), Some("foo")), (Some("bar"), Some("bar"))],
		);
	}

	#[test]
	fn side_by_side_add_only_renders_on_right() {
		let diff = test_file_diff(vec![
			test_diff_line("foo", DiffLineType::Add, None, Some(1)),
			test_diff_line("bar", DiffLineType::Add, None, Some(2)),
		]);

		assert_side_rows(
			&diff,
			&[(None, Some("foo")), (None, Some("bar"))],
		);
	}

	#[test]
	fn side_by_side_delete_only_renders_on_left() {
		let diff = test_file_diff(vec![
			test_diff_line(
				"foo",
				DiffLineType::Delete,
				Some(1),
				None,
			),
			test_diff_line(
				"bar",
				DiffLineType::Delete,
				Some(2),
				None,
			),
		]);

		assert_side_rows(
			&diff,
			&[(Some("foo"), None), (Some("bar"), None)],
		);
	}

	#[test]
	fn side_by_side_pairs_one_to_one_replacement() {
		let diff = test_file_diff(vec![
			test_diff_line(
				"foo",
				DiffLineType::Delete,
				Some(1),
				None,
			),
			test_diff_line("bar", DiffLineType::Add, None, Some(1)),
		]);

		assert_side_rows(&diff, &[(Some("foo"), Some("bar"))]);
	}

	#[test]
	fn side_by_side_pairs_many_to_one_replacement() {
		let diff = test_file_diff(vec![
			test_diff_line(
				"foo",
				DiffLineType::Delete,
				Some(1),
				None,
			),
			test_diff_line(
				"bar",
				DiffLineType::Delete,
				Some(2),
				None,
			),
			test_diff_line("baz", DiffLineType::Add, None, Some(1)),
		]);

		assert_side_rows(
			&diff,
			&[(Some("foo"), Some("baz")), (Some("bar"), None)],
		);
	}

	#[test]
	fn side_by_side_pairs_one_to_many_replacement() {
		let diff = test_file_diff(vec![
			test_diff_line(
				"foo",
				DiffLineType::Delete,
				Some(1),
				None,
			),
			test_diff_line("bar", DiffLineType::Add, None, Some(1)),
			test_diff_line("baz", DiffLineType::Add, None, Some(2)),
		]);

		assert_side_rows(
			&diff,
			&[(Some("foo"), Some("bar")), (None, Some("baz"))],
		);
	}

	#[test]
	fn side_by_side_keeps_hunk_headers_full_width() {
		let diff = test_file_diff(vec![
			test_diff_line(
				"@@ -1,2 +1,2 @@",
				DiffLineType::Header,
				None,
				None,
			),
			test_diff_line(
				"foo",
				DiffLineType::Delete,
				Some(1),
				None,
			),
			test_diff_line("bar", DiffLineType::Add, None, Some(1)),
		]);

		let display_lines = build_side_by_side_lines(&diff);
		assert!(matches!(
			display_lines.first(),
			Some(SideBySideDisplayLine::Header { .. })
		));
		assert_eq!(display_lines[1].raw_diff_indices(), vec![1, 2]);
	}
}
