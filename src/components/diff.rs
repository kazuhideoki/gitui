use super::{
	diff_display::{
		build_side_by_side_lines, side_by_side_copy_line,
		side_by_side_hunk_range, side_by_side_selected_positions,
		DiffViewMode, SideBySideDisplayLine, SideBySideLine,
		SideCell, MIN_SIDE_BY_SIDE_WIDTH,
	},
	utils::scroll_horizontal::HorizontalScroll,
	utils::scroll_vertical::VerticalScroll,
	CommandBlocking, Direction, DrawableComponent,
	HorizontalScrollType, ScrollType,
};
use crate::{
	app::Environment,
	components::{CommandInfo, Component, EventState},
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	queue::{
		Action, ExternalEditorOpen, InternalEvent, NeedsUpdate,
		Queue, ResetItem,
	},
	string_utils::tabs_to_spaces,
	string_utils::trim_offset,
	strings, try_or_popup,
	ui::{
		diff_syntax::{
			cache_highlighted_diff, cached_highlighted_diff,
			highlighted_line_to_spans,
			highlighted_spans_for_side_cell,
			highlighted_spans_for_unified_line,
			merge_syntax_and_diff_style, trim_highlighted_spans,
			AsyncDiffSyntaxJob, HighlightStatus, HighlightedDiff,
			HighlightedDiffKey, HighlightedLine,
		},
		style::SharedTheme,
	},
	AsyncAppNotification, AsyncNotification,
	DiffSyntaxHighlightProgress,
};
use anyhow::Result;
use asyncgit::{
	asyncjob::AsyncSingleJob,
	hash,
	sync::{self, diff::DiffLinePosition, RepoPathRef},
	DiffLine, DiffLineType, DiffParams, FileDiff, LineStats,
	ProgressPercent,
};
use bytesize::ByteSize;
use crossterm::event::Event;
use ratatui::{
	layout::Rect,
	style::Style,
	symbols,
	text::{Line, Span},
	widgets::{Block, Borders, Paragraph},
	Frame,
};
use std::{borrow::Cow, cell::Cell, cmp, path::Path};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
struct Current {
	path: String,
	is_stage: bool,
	hash: u64,
}

///
#[derive(Clone, Copy)]
enum Selection {
	Single(usize),
	Multiple(usize, usize),
}

impl Selection {
	const fn get_start(&self) -> usize {
		match self {
			Self::Single(start) | Self::Multiple(start, _) => *start,
		}
	}

	const fn get_end(&self) -> usize {
		match self {
			Self::Single(end) | Self::Multiple(_, end) => *end,
		}
	}

	fn get_top(&self) -> usize {
		match self {
			Self::Single(start) => *start,
			Self::Multiple(start, end) => cmp::min(*start, *end),
		}
	}

	fn get_bottom(&self) -> usize {
		match self {
			Self::Single(start) => *start,
			Self::Multiple(start, end) => cmp::max(*start, *end),
		}
	}

	fn modify(&mut self, direction: Direction, max: usize) {
		let start = self.get_start();
		let old_end = self.get_end();

		*self = match direction {
			Direction::Up => {
				Self::Multiple(start, old_end.saturating_sub(1))
			}

			Direction::Down => {
				Self::Multiple(start, cmp::min(old_end + 1, max))
			}
		};
	}

	fn contains(&self, index: usize) -> bool {
		match self {
			Self::Single(start) => index == *start,
			Self::Multiple(start, end) => {
				if start <= end {
					*start <= index && index <= *end
				} else {
					*end <= index && index <= *start
				}
			}
		}
	}
}

///
pub struct DiffComponent {
	repo: RepoPathRef,
	diff: Option<FileDiff>,
	longest_line: usize,
	pending: bool,
	selection: Selection,
	selected_hunk: Option<usize>,
	current_size: Cell<(u16, u16)>,
	focused: bool,
	current: Current,
	vertical_scroll: VerticalScroll,
	horizontal_scroll: HorizontalScroll,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	is_immutable: bool,
	options: SharedOptions,
	view_mode: DiffViewMode,
	render_view_mode_override: Cell<Option<DiffViewMode>>,
	syntax_cache: Option<HighlightedDiff>,
	syntax_job: AsyncSingleJob<AsyncDiffSyntaxJob>,
	syntax_progress: Option<ProgressPercent>,
	current_diff_params: Option<DiffParams>,
}

impl DiffComponent {
	///
	pub fn new(env: &Environment, is_immutable: bool) -> Self {
		Self {
			focused: false,
			queue: env.queue.clone(),
			current: Current::default(),
			pending: false,
			selected_hunk: None,
			diff: None,
			longest_line: 0,
			current_size: Cell::new((0, 0)),
			selection: Selection::Single(0),
			vertical_scroll: VerticalScroll::new(),
			horizontal_scroll: HorizontalScroll::new(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			is_immutable,
			repo: env.repo.clone(),
			options: env.options.clone(),
			view_mode: DiffViewMode::Unified,
			render_view_mode_override: Cell::new(None),
			syntax_cache: None,
			syntax_job: AsyncSingleJob::new(env.sender_app.clone()),
			syntax_progress: None,
			current_diff_params: None,
		}
	}
	///
	fn can_scroll(&self) -> bool {
		self.lines_count() > 1
	}
	///
	pub fn current(&self) -> (String, bool) {
		(self.current.path.clone(), self.current.is_stage)
	}
	///
	const fn can_edit_file(&self) -> bool {
		!self.current.path.is_empty()
	}
	///
	pub fn clear(&mut self, pending: bool) {
		self.current = Current::default();
		self.diff = None;
		self.longest_line = 0;
		self.vertical_scroll.reset();
		self.horizontal_scroll.reset();
		self.selection = Selection::Single(0);
		self.selected_hunk = None;
		self.pending = pending;
		self.syntax_cache = None;
		self.syntax_progress = None;
		self.current_diff_params = None;
		self.syntax_job.cancel();
	}
	///
	pub fn update(
		&mut self,
		path: String,
		is_stage: bool,
		diff: FileDiff,
		diff_params: DiffParams,
	) {
		self.pending = false;

		let hash = hash(&diff);
		let highlight_key =
			self.highlight_key(path.clone(), hash, &diff_params);

		if self.current.hash != hash
			|| self.current.path != path
			|| self.current.is_stage != is_stage
		{
			let reset_selection = self.current.path != path;

			self.current = Current {
				path,
				is_stage,
				hash,
			};

			self.diff = Some(diff);

			self.longest_line = self
				.diff
				.iter()
				.flat_map(|diff| diff.hunks.iter())
				.flat_map(|hunk| hunk.lines.iter())
				.map(|line| {
					let converted_content = tabs_to_spaces(
						line.content.as_ref().to_string(),
					);

					converted_content.len()
				})
				.max()
				.map_or(0, |len| {
					// Each hunk uses a 1-character wide vertical bar to its left to indicate
					// selection.
					len + 1
				});

			if reset_selection {
				self.vertical_scroll.reset();
				self.selection = Selection::Single(0);
				self.update_selection(0);
			} else {
				let old_selection = match self.selection {
					Selection::Single(line) => line,
					Selection::Multiple(start, _) => start,
				};
				self.update_selection(old_selection);
			}
		}

		self.current_diff_params = Some(diff_params.clone());
		let has_hunks = self
			.diff
			.as_ref()
			.is_some_and(|diff| !diff.hunks.is_empty());
		self.request_syntax_highlight(
			highlight_key,
			diff_params,
			has_hunks,
		);
	}

	pub fn update_async(&mut self, ev: AsyncNotification) {
		if let AsyncNotification::App(
			AsyncAppNotification::DiffSyntaxHighlighting(progress),
		) = ev
		{
			match progress {
				DiffSyntaxHighlightProgress::Progress => {
					self.syntax_progress = self.syntax_job.progress();
				}
				DiffSyntaxHighlightProgress::Done => {
					self.syntax_progress = None;
					if let Some(job) = self.syntax_job.take_last() {
						if let Some(highlighted) = job.result() {
							cache_highlighted_diff(
								highlighted.clone(),
							);
							if self
								.current_highlight_key()
								.is_some_and(|key| {
									key == highlighted.key
								}) {
								self.syntax_cache = Some(highlighted);
							}
						}
					}
				}
			}
		}
	}

	pub fn any_work_pending(&self) -> bool {
		self.syntax_job.is_pending()
	}

	fn highlight_key(
		&self,
		path: String,
		diff_hash: u64,
		diff_params: &DiffParams,
	) -> HighlightedDiffKey {
		HighlightedDiffKey::new(
			path,
			diff_hash,
			diff_params,
			self.theme.get_syntax(),
			2,
		)
	}

	fn current_highlight_key(&self) -> Option<HighlightedDiffKey> {
		let params = self.current_diff_params.as_ref()?;
		Some(self.highlight_key(
			self.current.path.clone(),
			self.current.hash,
			params,
		))
	}

	fn syntax_cache_matches(&self, key: &HighlightedDiffKey) -> bool {
		self.syntax_cache.as_ref().is_some_and(|cache| {
			cache.key == *key
				&& matches!(
					cache.status,
					HighlightStatus::Ready
						| HighlightStatus::Loading
						| HighlightStatus::Failed(_)
						| HighlightStatus::Skipped(_)
				)
		})
	}

	fn request_syntax_highlight(
		&mut self,
		key: HighlightedDiffKey,
		diff_params: DiffParams,
		has_hunks: bool,
	) {
		if !self.options.borrow().diff_syntax_highlight()
			|| !has_hunks
			|| self.syntax_cache_matches(&key)
		{
			return;
		}

		if let Some(highlighted) = cached_highlighted_diff(&key) {
			self.syntax_progress = None;
			self.syntax_cache = Some(highlighted);
			return;
		}

		self.syntax_progress = Some(ProgressPercent::empty());
		self.syntax_cache = Some(HighlightedDiff {
			key: key.clone(),
			old: None,
			new: None,
			status: HighlightStatus::Loading,
		});
		let options = self.options.borrow();
		self.syntax_job.spawn(AsyncDiffSyntaxJob::new(
			key,
			self.repo.borrow().clone(),
			diff_params.diff_type,
			options.diff_syntax_max_file_bytes(),
			options.diff_syntax_max_file_lines(),
		));
	}

	fn move_selection(&mut self, move_type: ScrollType) {
		if let Some(diff) = &self.diff {
			let max = diff.lines.saturating_sub(1);

			let new_start = match move_type {
				ScrollType::Down => {
					self.selection.get_bottom().saturating_add(1)
				}
				ScrollType::Up => {
					self.selection.get_top().saturating_sub(1)
				}
				ScrollType::Home => 0,
				ScrollType::End => max,
				ScrollType::PageDown => {
					self.selection.get_bottom().saturating_add(
						self.current_size.get().1.saturating_sub(1)
							as usize,
					)
				}
				ScrollType::PageUp => {
					self.selection.get_top().saturating_sub(
						self.current_size.get().1.saturating_sub(1)
							as usize,
					)
				}
			};

			self.update_selection(new_start);
		}
	}

	fn update_selection(&mut self, new_start: usize) {
		if let Some(diff) = &self.diff {
			let max = self.lines_count().saturating_sub(1);
			let new_start = cmp::min(max, new_start);
			self.selection = Selection::Single(new_start);
			self.selected_hunk =
				self.find_selected_hunk_for_mode(diff, new_start);
		}
	}

	fn lines_count(&self) -> usize {
		self.diff.as_ref().map_or(0, |diff| {
			if self.side_by_side_model_active() {
				build_side_by_side_lines(diff).len()
			} else {
				diff.lines
			}
		})
	}

	fn max_scroll_right(&self) -> usize {
		self.longest_line.saturating_sub(
			if self.side_by_side_render_active() {
				usize::from(self.side_cell_width())
			} else {
				self.current_size.get().0.into()
			},
		)
	}

	fn modify_selection(&mut self, direction: Direction) {
		if self.diff.is_some() {
			self.selection.modify(
				direction,
				self.lines_count().saturating_sub(1),
			);
		}
	}

	fn effective_view_mode(&self) -> DiffViewMode {
		self.render_view_mode_override
			.get()
			.unwrap_or(self.view_mode)
	}

	fn side_by_side_model_active(&self) -> bool {
		self.effective_view_mode() == DiffViewMode::SideBySide
	}

	fn side_by_side_render_active(&self) -> bool {
		self.effective_view_mode() == DiffViewMode::SideBySide
			&& self.current_size.get().0 >= MIN_SIDE_BY_SIDE_WIDTH
	}

	fn side_cell_width(&self) -> u16 {
		let content_width =
			self.current_size.get().0.saturating_sub(2);
		content_width / 2
	}

	fn toggle_view_mode(&mut self) {
		self.view_mode = match self.view_mode {
			DiffViewMode::Unified => DiffViewMode::SideBySide,
			DiffViewMode::SideBySide => DiffViewMode::Unified,
		};

		if let Some(diff) = &self.diff {
			let target = self
				.selected_hunk
				.and_then(|hunk| {
					self.hunk_range_for_mode(diff, hunk)
						.map(|(start, _)| start)
				})
				.unwrap_or(0);
			self.update_selection(target);
			self.vertical_scroll.move_area_to_visible(
				usize::from(self.current_size.get().1),
				target,
				target,
			);
		}
	}

	fn change_context_lines(&self, increase: bool) {
		self.options.borrow_mut().diff_context_change(increase);
		self.queue.push(InternalEvent::Update(
			NeedsUpdate::DIFF | NeedsUpdate::COMMANDS,
		));
	}

	fn copy_selection(&self) {
		if let Some(diff) = &self.diff {
			if self.side_by_side_model_active() {
				let lines_to_copy: Vec<String> =
					build_side_by_side_lines(diff)
						.into_iter()
						.enumerate()
						.filter_map(|(i, line)| {
							if self.selection.contains(i) {
								Some(side_by_side_copy_line(&line))
							} else {
								None
							}
						})
						.collect();

				try_or_popup!(
					self,
					"copy to clipboard error:",
					crate::clipboard::copy_string(
						&lines_to_copy.join("\n")
					)
				);
				return;
			}

			let lines_to_copy: Vec<&str> =
				diff.hunks
					.iter()
					.flat_map(|hunk| hunk.lines.iter())
					.enumerate()
					.filter_map(|(i, line)| {
						if self.selection.contains(i) {
							Some(line.content.trim_matches(|c| {
								c == '\n' || c == '\r'
							}))
						} else {
							None
						}
					})
					.collect();

			try_or_popup!(
				self,
				"copy to clipboard error:",
				crate::clipboard::copy_string(
					&lines_to_copy.join("\n")
				)
			);
		}
	}

	fn find_selected_hunk(
		diff: &FileDiff,
		line_selected: usize,
	) -> Option<usize> {
		let mut line_cursor = 0_usize;
		for (i, hunk) in diff.hunks.iter().enumerate() {
			let hunk_len = hunk.lines.len();
			let hunk_min = line_cursor;
			let hunk_max = line_cursor + hunk_len;

			let hunk_selected =
				hunk_min <= line_selected && hunk_max > line_selected;

			if hunk_selected {
				return Some(i);
			}

			line_cursor += hunk_len;
		}

		None
	}

	fn find_selected_hunk_for_mode(
		&self,
		diff: &FileDiff,
		line_selected: usize,
	) -> Option<usize> {
		if self.side_by_side_model_active() {
			build_side_by_side_lines(diff)
				.get(line_selected)
				.and_then(SideBySideDisplayLine::hunk_index)
		} else {
			Self::find_selected_hunk(diff, line_selected)
		}
	}

	fn get_text(&self, width: u16, height: u16) -> Vec<Line<'_>> {
		if let Some(diff) = &self.diff {
			let highlighted = self
				.syntax_cache
				.as_ref()
				.filter(|cache| cache.is_ready());
			return if diff.hunks.is_empty() {
				self.get_text_binary(diff)
			} else if self.side_by_side_render_active() {
				self.get_text_side_by_side(
					diff,
					width,
					height,
					highlighted,
				)
			} else {
				let mut res: Vec<Line> = Vec::new();

				let min = self.vertical_scroll.get_top();
				let max = min + height as usize;

				let mut line_cursor = 0_usize;
				let mut lines_added = 0_usize;

				for (i, hunk) in diff.hunks.iter().enumerate() {
					let hunk_selected = self.focused()
						&& self.selected_hunk.is_some_and(|s| s == i);

					if lines_added >= height as usize {
						break;
					}

					let hunk_len = hunk.lines.len();
					let hunk_min = line_cursor;
					let hunk_max = line_cursor + hunk_len;

					if Self::hunk_visible(
						hunk_min, hunk_max, min, max,
					) {
						for (i, line) in hunk.lines.iter().enumerate()
						{
							if line_cursor >= min
								&& line_cursor <= max
							{
								res.push(Self::get_line_to_add(
									width,
									line,
									self.focused()
										&& self
											.selection
											.contains(line_cursor),
									hunk_selected,
									i == hunk_len - 1,
									&self.theme,
									self.horizontal_scroll
										.get_right(),
									highlighted,
								));
								lines_added += 1;
							}

							line_cursor += 1;
						}
					} else {
						line_cursor += hunk_len;
					}
				}

				res
			};
		}

		vec![]
	}

	fn get_text_side_by_side<'a>(
		&self,
		diff: &'a FileDiff,
		width: u16,
		height: u16,
		highlighted: Option<&'a HighlightedDiff>,
	) -> Vec<Line<'a>> {
		let display_lines = build_side_by_side_lines(diff);
		let min = self.vertical_scroll.get_top();
		let max = min + height as usize;
		let cell_width = self.side_cell_width();
		let mut res = Vec::new();

		for (display_index, line) in display_lines
			.iter()
			.enumerate()
			.skip(min)
			.take(max.saturating_sub(min) + 1)
		{
			let selected = self.focused()
				&& self.selection.contains(display_index);
			let selected_hunk = self.focused()
				&& line.hunk_index().is_some_and(|hunk| {
					Some(hunk) == self.selected_hunk
				});

			res.push(match line {
				SideBySideDisplayLine::Header { line, .. } => self
					.get_side_by_side_header(
						width,
						line,
						selected,
						selected_hunk,
					),
				SideBySideDisplayLine::Row(row) => {
					let end_of_hunk = display_lines
						.get(display_index.saturating_add(1))
						.is_none_or(|next| {
							next.hunk_index() != line.hunk_index()
						});
					res.extend(self.get_side_by_side_row(
						row,
						cell_width,
						selected,
						selected_hunk,
						end_of_hunk,
						highlighted,
					));
					continue;
				}
			});
		}

		res
	}

	fn get_side_by_side_header<'a>(
		&self,
		width: u16,
		line: &'a DiffLine,
		selected: bool,
		selected_hunk: bool,
	) -> Line<'a> {
		let content =
			tabs_to_spaces(line.content.as_ref().to_string());
		let content =
			trim_offset(&content, self.horizontal_scroll.get_right());
		let inner_width = usize::from(width.saturating_sub(3));
		let filled = if selected {
			format!("{content:inner_width$}\n")
		} else {
			format!("{content}\n")
		};

		Line::from(vec![
			Span::styled(
				Cow::from(symbols::line::TOP_LEFT),
				self.theme.diff_hunk_marker(selected_hunk),
			),
			Span::styled(
				Cow::from(filled),
				self.theme.diff_line(DiffLineType::Header, selected),
			),
		])
	}

	fn get_side_by_side_row<'a>(
		&self,
		row: &SideBySideLine<'a>,
		cell_width: u16,
		selected: bool,
		selected_hunk: bool,
		end_of_hunk: bool,
		highlighted: Option<&'a HighlightedDiff>,
	) -> Vec<Line<'a>> {
		let (left_lines, left_line_type) = self.get_side_cell_lines(
			row.left,
			cell_width,
			selected,
			true,
			highlighted,
		);
		let (right_lines, right_line_type) = self
			.get_side_cell_lines(
				row.right,
				cell_width,
				selected,
				false,
				highlighted,
			);
		let width = usize::from(cell_width);
		let row_count =
			cmp::max(left_lines.len(), right_lines.len()).max(1);
		let mut lines = Vec::with_capacity(row_count);

		for visual_index in 0..row_count {
			let marker =
				if end_of_hunk && visual_index == row_count - 1 {
					symbols::line::BOTTOM_LEFT
				} else {
					symbols::line::VERTICAL
				};
			let mut spans = vec![Span::styled(
				Cow::from(marker),
				self.theme.diff_hunk_marker(selected_hunk),
			)];
			spans.extend(Self::side_cell_spans_at(
				&left_lines,
				visual_index,
				width,
				left_line_type,
				selected,
				&self.theme,
			));
			spans.push(Span::styled(
				Cow::from(symbols::line::VERTICAL),
				self.theme.diff_hunk_marker(selected),
			));
			spans.extend(Self::side_cell_spans_at(
				&right_lines,
				visual_index,
				width,
				right_line_type,
				selected,
				&self.theme,
			));
			spans.push(Span::styled(
				Cow::from("\n"),
				self.theme.diff_line(right_line_type, selected),
			));
			lines.push(Line::from(spans));
		}

		lines
	}

	fn get_side_cell_lines<'a>(
		&self,
		cell: Option<SideCell<'a>>,
		width: u16,
		selected: bool,
		use_old_side: bool,
		highlighted: Option<&'a HighlightedDiff>,
	) -> (Vec<Vec<Span<'a>>>, DiffLineType) {
		let line_type = cell
			.map_or(DiffLineType::None, |cell| cell.line.line_type);

		if let (Some(cell), Some(highlighted)) = (cell, highlighted) {
			if let Some(line) = highlighted_spans_for_side_cell(
				cell.line,
				use_old_side,
				highlighted,
			) {
				return (
					self.highlighted_side_cell_lines(
						line,
						cell.line.line_type,
						selected,
						self.horizontal_scroll.get_right(),
						usize::from(width),
					),
					line_type,
				);
			}
		}

		let fragments = cell.map_or_else(Vec::new, |cell| {
			let content =
				if !matches!(cell.line.line_type, DiffLineType::None)
					&& cell.line.content.as_ref().is_empty()
				{
					self.theme.line_break()
				} else {
					tabs_to_spaces(
						cell.line.content.as_ref().to_string(),
					)
				};
			let content = trim_offset(
				&content,
				self.horizontal_scroll.get_right(),
			)
			.to_string();
			vec![(content, self.theme.diff_line(line_type, selected))]
		});

		(
			Self::wrap_side_cell_fragments(
				fragments,
				usize::from(width),
			),
			line_type,
		)
	}

	fn highlighted_side_cell_lines<'a>(
		&self,
		line: &HighlightedLine,
		line_type: DiffLineType,
		selected: bool,
		scrolled_right: usize,
		width: usize,
	) -> Vec<Vec<Span<'a>>> {
		let spans =
			trim_highlighted_spans(&line.spans, scrolled_right);
		let fragments = if spans.is_empty()
			&& !matches!(line_type, DiffLineType::None)
		{
			vec![(
				self.theme.line_break(),
				self.theme.diff_line(line_type, selected),
			)]
		} else {
			spans
				.into_iter()
				.map(|span| {
					(
						span.content,
						merge_syntax_and_diff_style(
							span.style,
							line_type,
							selected,
							&self.theme,
						),
					)
				})
				.collect()
		};

		Self::wrap_side_cell_fragments(fragments, width)
	}

	fn wrap_side_cell_fragments<'a>(
		fragments: Vec<(String, Style)>,
		width: usize,
	) -> Vec<Vec<Span<'a>>> {
		if width == 0 {
			return vec![Vec::new()];
		}

		let mut lines = Vec::new();
		let mut current = Vec::new();
		let mut current_width = 0_usize;
		let mut buffer = String::new();
		let mut buffer_style = None;

		for (content, style) in fragments {
			Self::push_buffered_side_span(
				&mut current,
				&mut buffer,
				buffer_style.take(),
			);
			buffer_style = Some(style);

			for grapheme in
				UnicodeSegmentation::graphemes(content.as_str(), true)
			{
				let grapheme_width = grapheme.width();
				if current_width > 0
					&& current_width + grapheme_width > width
				{
					Self::push_buffered_side_span(
						&mut current,
						&mut buffer,
						buffer_style.take(),
					);
					lines.push(current);
					current = Vec::new();
					current_width = 0;
					buffer_style = Some(style);
				}

				buffer.push_str(grapheme);
				current_width += grapheme_width;
			}
		}

		Self::push_buffered_side_span(
			&mut current,
			&mut buffer,
			buffer_style,
		);

		if !current.is_empty() {
			lines.push(current);
		}

		if lines.is_empty() {
			lines.push(Vec::new());
		}

		lines
	}

	fn push_buffered_side_span<'a>(
		line: &mut Vec<Span<'a>>,
		buffer: &mut String,
		style: Option<Style>,
	) {
		if !buffer.is_empty() {
			if let Some(style) = style {
				line.push(Span::styled(
					Cow::from(std::mem::take(buffer)),
					style,
				));
			}
		}
	}

	fn side_cell_spans_at<'a>(
		lines: &[Vec<Span<'a>>],
		index: usize,
		width: usize,
		line_type: DiffLineType,
		selected: bool,
		theme: &SharedTheme,
	) -> Vec<Span<'a>> {
		let mut spans = lines.get(index).cloned().unwrap_or_default();
		Self::pad_side_cell_spans(
			&mut spans, width, line_type, selected, theme,
		);
		spans
	}

	fn pad_side_cell_spans<'a>(
		spans: &mut Vec<Span<'a>>,
		width: usize,
		line_type: DiffLineType,
		selected: bool,
		theme: &SharedTheme,
	) {
		let visible_width = spans
			.iter()
			.map(|span| span.content.as_ref().width())
			.sum::<usize>();
		let padding = width.saturating_sub(visible_width);

		if padding > 0 {
			spans.push(Span::styled(
				Cow::from(" ".repeat(padding)),
				theme.diff_line(line_type, selected),
			));
		}
	}

	fn get_text_binary(&self, diff: &FileDiff) -> Vec<Line<'_>> {
		let is_positive = diff.size_delta >= 0;
		let delta_byte_size =
			ByteSize::b(diff.size_delta.unsigned_abs());
		let sign = if is_positive { "+" } else { "-" };
		vec![Line::from(vec![
			Span::raw(Cow::from("size: ")),
			Span::styled(
				Cow::from(format!("{}", ByteSize::b(diff.sizes.0))),
				self.theme.text(false, false),
			),
			Span::raw(Cow::from(" -> ")),
			Span::styled(
				Cow::from(format!("{}", ByteSize::b(diff.sizes.1))),
				self.theme.text(false, false),
			),
			Span::raw(Cow::from(" (")),
			Span::styled(
				Cow::from(format!("{sign}{delta_byte_size:}")),
				self.theme.diff_line(
					if is_positive {
						DiffLineType::Add
					} else {
						DiffLineType::Delete
					},
					false,
				),
			),
			Span::raw(Cow::from(")")),
		])]
	}

	fn get_line_to_add<'a>(
		width: u16,
		line: &'a DiffLine,
		selected: bool,
		selected_hunk: bool,
		end_of_hunk: bool,
		theme: &SharedTheme,
		scrolled_right: usize,
		highlighted: Option<&'a HighlightedDiff>,
	) -> Line<'a> {
		let style = theme.diff_hunk_marker(selected_hunk);

		let is_content_line =
			matches!(line.line_type, DiffLineType::None);

		let left_side_of_line = if end_of_hunk {
			Span::styled(Cow::from(symbols::line::BOTTOM_LEFT), style)
		} else {
			match line.line_type {
				DiffLineType::Header => Span::styled(
					Cow::from(symbols::line::TOP_LEFT),
					style,
				),
				_ => Span::styled(
					Cow::from(symbols::line::VERTICAL),
					style,
				),
			}
		};

		if let Some(highlighted) = highlighted {
			if let Some(highlighted_line) =
				highlighted_spans_for_unified_line(line, highlighted)
			{
				let mut spans = vec![left_side_of_line];
				spans.extend(highlighted_line_to_spans(
					highlighted_line,
					line.line_type,
					selected,
					theme,
					scrolled_right,
					width as usize,
					selected,
					true,
				));
				return Line::from(spans);
			}
		}

		let content =
			if !is_content_line && line.content.as_ref().is_empty() {
				theme.line_break()
			} else {
				tabs_to_spaces(line.content.as_ref().to_string())
			};
		let content = trim_offset(&content, scrolled_right);

		let filled = if selected {
			// selected line
			format!("{content:w$}\n", w = width as usize)
		} else {
			// weird eof missing eol line
			format!("{content}\n")
		};

		Line::from(vec![
			left_side_of_line,
			Span::styled(
				Cow::from(filled),
				theme.diff_line(line.line_type, selected),
			),
		])
	}

	const fn hunk_visible(
		hunk_min: usize,
		hunk_max: usize,
		min: usize,
		max: usize,
	) -> bool {
		// full overlap
		if hunk_min <= min && hunk_max >= max {
			return true;
		}

		// partly overlap
		if (hunk_min >= min && hunk_min <= max)
			|| (hunk_max >= min && hunk_max <= max)
		{
			return true;
		}

		false
	}

	fn unstage_hunk(&self) -> Result<()> {
		if let Some(diff) = &self.diff {
			if let Some(hunk) = self.selected_hunk {
				let hash = diff.hunks[hunk].header_hash;
				sync::unstage_hunk(
					&self.repo.borrow(),
					&self.current.path,
					hash,
					Some(self.options.borrow().diff_options()),
				)?;
				self.queue_update();
			}
		}

		Ok(())
	}

	fn stage_hunk(&self) -> Result<()> {
		if let Some(diff) = &self.diff {
			if let Some(hunk) = self.selected_hunk {
				if diff.untracked {
					sync::stage_add_file(
						&self.repo.borrow(),
						Path::new(&self.current.path),
					)?;
				} else {
					let hash = diff.hunks[hunk].header_hash;
					sync::stage_hunk(
						&self.repo.borrow(),
						&self.current.path,
						hash,
						Some(self.options.borrow().diff_options()),
					)?;
				}

				self.queue_update();
			}
		}

		Ok(())
	}

	fn queue_update(&self) {
		self.queue.push(InternalEvent::Update(NeedsUpdate::ALL));
	}

	fn reset_hunk(&self) {
		if let Some(diff) = &self.diff {
			if let Some(hunk) = self.selected_hunk {
				let hash = diff.hunks[hunk].header_hash;

				self.queue.push(InternalEvent::ConfirmAction(
					Action::ResetHunk(
						self.current.path.clone(),
						hash,
					),
				));
			}
		}
	}

	fn reset_lines(&self) {
		self.queue.push(InternalEvent::ConfirmAction(
			Action::ResetLines(
				self.current.path.clone(),
				self.selected_lines(),
			),
		));
	}

	fn stage_lines(&self) {
		if let Some(diff) = &self.diff {
			//TODO: support untracked files as well
			if !diff.untracked {
				let selected_lines = self.selected_lines();

				try_or_popup!(
					self,
					"(un)stage lines:",
					sync::stage_lines(
						&self.repo.borrow(),
						&self.current.path,
						self.is_stage(),
						&selected_lines,
					)
				);

				self.queue_update();
			}
		}
	}

	fn selected_lines(&self) -> Vec<DiffLinePosition> {
		self.diff.as_ref().map_or_else(Vec::new, |diff| {
			if self.side_by_side_model_active() {
				return side_by_side_selected_positions(diff, |i| {
					self.selection.contains(i)
				});
			}

			diff.hunks
				.iter()
				.flat_map(|hunk| hunk.lines.iter())
				.enumerate()
				.filter_map(|(i, line)| {
					let is_add_or_delete = line.line_type
						== DiffLineType::Add
						|| line.line_type == DiffLineType::Delete;
					if self.selection.contains(i) && is_add_or_delete
					{
						Some(line.position)
					} else {
						None
					}
				})
				.collect()
		})
	}

	fn selected_editor_line(&self) -> Option<u32> {
		let diff = self.diff.as_ref()?;
		let raw_lines = Self::raw_diff_lines(diff);
		let selected = self.selection.get_end();

		if self.side_by_side_model_active() {
			let display_lines = build_side_by_side_lines(diff);
			let display_line = display_lines.get(selected)?;
			let raw_indices = display_line.raw_diff_indices();

			return Self::editor_line_for_raw_indices(
				&raw_lines,
				&raw_indices,
			)
			.or_else(|| {
				raw_indices.first().and_then(|raw_index| {
					Self::editor_line_near_raw_index(
						&raw_lines, *raw_index,
					)
				})
			});
		}

		Self::editor_line_near_raw_index(&raw_lines, selected)
	}

	fn raw_diff_lines(diff: &FileDiff) -> Vec<&DiffLine> {
		diff.hunks
			.iter()
			.flat_map(|hunk| hunk.lines.iter())
			.collect()
	}

	fn editor_line_for_raw_indices(
		raw_lines: &[&DiffLine],
		raw_indices: &[usize],
	) -> Option<u32> {
		let mut old_lineno = None;

		for raw_index in raw_indices {
			let position = raw_lines.get(*raw_index)?.position;
			if let Some(new_lineno) = position.new_lineno {
				return Some(new_lineno);
			}
			old_lineno = old_lineno.or(position.old_lineno);
		}

		old_lineno
	}

	fn editor_line_near_raw_index(
		raw_lines: &[&DiffLine],
		raw_index: usize,
	) -> Option<u32> {
		Self::editor_line_for_raw_indices(raw_lines, &[raw_index])
			.or_else(|| {
				raw_lines
					.iter()
					.skip(raw_index.saturating_add(1))
					.find_map(|line| Self::editor_line_for_line(line))
			})
			.or_else(|| {
				raw_lines
					.iter()
					.take(raw_index)
					.rev()
					.find_map(|line| Self::editor_line_for_line(line))
			})
	}

	fn editor_line_for_line(line: &DiffLine) -> Option<u32> {
		line.position.new_lineno.or(line.position.old_lineno)
	}

	fn reset_untracked(&self) {
		self.queue.push(InternalEvent::ConfirmAction(Action::Reset(
			ResetItem {
				path: self.current.path.clone(),
			},
		)));
	}

	fn stage_unstage_hunk(&self) -> Result<()> {
		if self.current.is_stage {
			self.unstage_hunk()?;
		} else {
			self.stage_hunk()?;
		}

		Ok(())
	}

	fn calc_hunk_move_target(
		&self,
		direction: isize,
	) -> Option<usize> {
		let diff = self.diff.as_ref()?;
		if diff.hunks.is_empty() {
			return None;
		}
		let max = diff.hunks.len() - 1;
		let target_index = self.selected_hunk.map_or(0, |i| {
			let target = if direction >= 0 {
				i.saturating_add(direction.unsigned_abs())
			} else {
				i.saturating_sub(direction.unsigned_abs())
			};
			std::cmp::min(max, target)
		});
		Some(target_index)
	}

	fn diff_hunk_move_up_down(&mut self, direction: isize) {
		let Some(diff) = &self.diff else { return };
		let hunk_index = self.calc_hunk_move_target(direction);
		// return if selected_hunk not change
		if self.selected_hunk == hunk_index {
			return;
		}
		if let Some(hunk_index) = hunk_index {
			let Some((line_index, hunk_end)) =
				self.hunk_range_for_mode(diff, hunk_index)
			else {
				return;
			};
			self.selection = Selection::Single(line_index);
			self.selected_hunk = Some(hunk_index);
			self.vertical_scroll.move_area_to_visible(
				self.current_size.get().1 as usize,
				line_index,
				hunk_end,
			);
		}
	}

	fn hunk_range_for_mode(
		&self,
		diff: &FileDiff,
		hunk_index: usize,
	) -> Option<(usize, usize)> {
		if self.side_by_side_model_active() {
			side_by_side_hunk_range(diff, hunk_index)
		} else {
			let start = diff
				.hunks
				.iter()
				.take(hunk_index)
				.fold(0, |sum, hunk| sum + hunk.lines.len());
			diff.hunks.get(hunk_index).map(|hunk| {
				(start, start.saturating_add(hunk.lines.len()))
			})
		}
	}

	const fn is_stage(&self) -> bool {
		self.current.is_stage
	}

	fn block<'a>(&self, title: &'a str) -> Block<'a> {
		let mut block = Block::default()
			.title(Span::styled(
				title,
				self.theme.title(self.focused()),
			))
			.borders(Borders::ALL)
			.border_style(self.theme.block(self.focused()));

		if !self.pending {
			if let Some(line_stats) = self.line_stats() {
				block =
					block.title(self.line_stats_title(line_stats));
			}
		}

		block
	}

	fn line_stats(&self) -> Option<LineStats> {
		let diff = self.diff.as_ref()?;
		if diff.hunks.is_empty() {
			return None;
		}

		let mut line_stats = LineStats::default();
		for line in
			diff.hunks.iter().flat_map(|hunk| hunk.lines.iter())
		{
			match line.line_type {
				DiffLineType::Add => line_stats.additions += 1,
				DiffLineType::Delete => line_stats.deletions += 1,
				_ => {}
			}
		}

		Some(line_stats)
	}

	fn line_stats_title(
		&self,
		line_stats: LineStats,
	) -> Line<'static> {
		Line::from(vec![
			Span::styled(
				format!("+{}", line_stats.additions),
				self.theme.line_stats_addition(),
			),
			Span::raw(" "),
			Span::styled(
				format!("-{}", line_stats.deletions),
				self.theme.line_stats_deletion(),
			),
		])
		.right_aligned()
	}
}

impl DrawableComponent for DiffComponent {
	fn draw(&self, f: &mut Frame, r: Rect) -> Result<()> {
		self.current_size.set((
			r.width.saturating_sub(2),
			r.height.saturating_sub(2),
		));

		let current_width = self.current_size.get().0;
		let current_height = self.current_size.get().1;

		self.vertical_scroll.update(
			self.selection.get_end(),
			self.lines_count(),
			usize::from(current_height),
		);

		self.horizontal_scroll.update_no_selection(
			self.longest_line,
			if self.side_by_side_render_active() {
				self.side_cell_width().into()
			} else {
				current_width.into()
			},
		);

		let title = format!(
			"{}{}{}{}",
			strings::title_diff(&self.key_config),
			if self.effective_view_mode() == DiffViewMode::SideBySide
			{
				"[side-by-side] "
			} else {
				""
			},
			self.current.path,
			self.syntax_title_suffix()
		);

		let txt = if self.pending {
			vec![Line::from(vec![Span::styled(
				Cow::from(strings::loading_text(&self.key_config)),
				self.theme.text(false, false),
			)])]
		} else {
			self.get_text(r.width, current_height)
		};

		f.render_widget(
			Paragraph::new(txt).block(self.block(title.as_str())),
			r,
		);

		if self.focused() {
			self.vertical_scroll.draw(f, r, &self.theme);

			if self.max_scroll_right() > 0 {
				self.horizontal_scroll.draw(f, r, &self.theme);
			}
		}

		Ok(())
	}
}

impl DiffComponent {
	pub fn draw_unified(&self, f: &mut Frame, r: Rect) -> Result<()> {
		self.render_view_mode_override
			.set(Some(DiffViewMode::Unified));
		let result = self.draw(f, r);
		self.render_view_mode_override.set(None);
		result
	}

	fn syntax_title_suffix(&self) -> String {
		if let Some(progress) = self.syntax_progress {
			return format!(" [syntax: {}%]", progress.progress);
		}

		match self.syntax_cache.as_ref().map(|cache| &cache.status) {
			Some(HighlightStatus::Skipped(reason)) => {
				format!(" [syntax skipped: {reason:?}]")
			}
			Some(HighlightStatus::Failed(_)) => {
				" [syntax failed]".to_string()
			}
			_ => String::new(),
		}
	}
}

impl Component for DiffComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		_force_all: bool,
	) -> CommandBlocking {
		out.push(CommandInfo::new(
			strings::commands::scroll(&self.key_config),
			self.can_scroll(),
			self.focused(),
		));
		out.push(CommandInfo::new(
			strings::commands::diff_hunk_next(&self.key_config),
			self.calc_hunk_move_target(1) != self.selected_hunk,
			self.focused(),
		));
		out.push(CommandInfo::new(
			strings::commands::diff_hunk_prev(&self.key_config),
			self.calc_hunk_move_target(-1) != self.selected_hunk,
			self.focused(),
		));
		out.push(CommandInfo::new(
			strings::commands::diff_toggle_view_mode(
				&self.key_config,
			),
			self.diff
				.as_ref()
				.is_some_and(|diff| !diff.hunks.is_empty()),
			self.focused(),
		));
		out.push(CommandInfo::new(
			strings::commands::diff_context_increase(
				&self.key_config,
			),
			self.diff
				.as_ref()
				.is_some_and(|diff| !diff.hunks.is_empty()),
			self.focused(),
		));
		out.push(CommandInfo::new(
			strings::commands::diff_context_decrease(
				&self.key_config,
			),
			self.diff
				.as_ref()
				.is_some_and(|diff| !diff.hunks.is_empty())
				&& self.options.borrow().diff_options().context > 0,
			self.focused(),
		));
		out.push(
			CommandInfo::new(
				strings::commands::diff_home_end(&self.key_config),
				self.can_scroll(),
				self.focused(),
			)
			.hidden(),
		);

		out.push(CommandInfo::new(
			strings::commands::edit_item(&self.key_config),
			self.can_edit_file(),
			self.focused() && self.can_edit_file(),
		));

		if !self.is_immutable {
			out.push(CommandInfo::new(
				strings::commands::diff_hunk_remove(&self.key_config),
				self.selected_hunk.is_some(),
				self.focused() && self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_hunk_add(&self.key_config),
				self.selected_hunk.is_some(),
				self.focused() && !self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_hunk_revert(&self.key_config),
				self.selected_hunk.is_some(),
				self.focused() && !self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_lines_revert(
					&self.key_config,
				),
				//TODO: only if any modifications are selected
				true,
				self.focused() && !self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_lines_stage(&self.key_config),
				//TODO: only if any modifications are selected
				true,
				self.focused() && !self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_lines_unstage(
					&self.key_config,
				),
				//TODO: only if any modifications are selected
				true,
				self.focused() && self.is_stage(),
			));
		}

		out.push(CommandInfo::new(
			strings::commands::copy(&self.key_config),
			true,
			self.focused(),
		));

		CommandBlocking::PassingOn
	}

	#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.focused() {
			if let Event::Key(e) = ev {
				return if key_match(e, self.key_config.keys.move_down)
				{
					self.move_selection(ScrollType::Down);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.shift_down,
				) {
					self.modify_selection(Direction::Down);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.shift_up)
				{
					self.modify_selection(Direction::Up);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.end) {
					self.move_selection(ScrollType::End);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.home) {
					self.move_selection(ScrollType::Home);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.move_up) {
					self.move_selection(ScrollType::Up);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.page_up) {
					self.move_selection(ScrollType::PageUp);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.page_down)
				{
					self.move_selection(ScrollType::PageDown);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.move_right,
				) {
					self.horizontal_scroll
						.move_right(HorizontalScrollType::Right);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.move_left)
				{
					self.horizontal_scroll
						.move_right(HorizontalScrollType::Left);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_hunk_next,
				) {
					self.diff_hunk_move_up_down(1);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_hunk_prev,
				) {
					self.diff_hunk_move_up_down(-1);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_toggle_view_mode,
				) {
					self.toggle_view_mode();
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_context_increase,
				) {
					self.change_context_lines(true);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_context_decrease,
				) {
					self.change_context_lines(false);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.edit_file)
					&& self.can_edit_file()
				{
					self.queue.push(
						InternalEvent::OpenExternalEditor(Some(
							ExternalEditorOpen::new(
								self.current.path.clone(),
							)
							.with_line(self.selected_editor_line()),
						)),
					);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.stage_unstage_item,
				) && !self.is_immutable
				{
					try_or_popup!(
						self,
						"hunk error:",
						self.stage_unstage_hunk()
					);

					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.status_reset_item,
				) && !self.is_immutable
					&& !self.is_stage()
				{
					if let Some(diff) = &self.diff {
						if diff.untracked {
							self.reset_untracked();
						} else {
							self.reset_hunk();
						}
					}
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_stage_lines,
				) && !self.is_immutable
				{
					self.stage_lines();
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_reset_lines,
				) && !self.is_immutable
					&& !self.is_stage()
				{
					if let Some(diff) = &self.diff {
						//TODO: reset untracked lines
						if !diff.untracked {
							self.reset_lines();
						}
					}
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.copy) {
					self.copy_selection();
					Ok(EventState::Consumed)
				} else {
					Ok(EventState::NotConsumed)
				};
			}
		}

		Ok(EventState::NotConsumed)
	}

	fn focused(&self) -> bool {
		self.focused
	}
	fn focus(&mut self, focus: bool) {
		self.focused = focus;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		app::Environment,
		queue::{Action, InternalEvent},
		ui::style::Theme,
	};
	use asyncgit::sync::diff::Hunk;
	use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
	use std::io::Write;
	use std::rc::Rc;
	use tempfile::NamedTempFile;

	#[test]
	fn test_line_break() {
		let diff_line = DiffLine {
			content: "".into(),
			line_type: DiffLineType::Add,
			position: Default::default(),
		};

		{
			let default_theme = Rc::new(Theme::default());

			assert_eq!(
				DiffComponent::get_line_to_add(
					4,
					&diff_line,
					false,
					false,
					false,
					&default_theme,
					0,
					None
				)
				.spans
				.last()
				.unwrap(),
				&Span::styled(
					Cow::from("¶\n"),
					default_theme
						.diff_line(diff_line.line_type, false)
				)
			);
		}

		{
			let mut file = NamedTempFile::new().unwrap();

			writeln!(
				file,
				r#"
(
	line_break: Some("+")
)
"#
			)
			.unwrap();

			let theme =
				Rc::new(Theme::init(&file.path().to_path_buf()));

			assert_eq!(
				DiffComponent::get_line_to_add(
					4, &diff_line, false, false, false, &theme, 0,
					None,
				)
				.spans
				.last()
				.unwrap(),
				&Span::styled(
					Cow::from("+\n"),
					theme.diff_line(diff_line.line_type, false)
				)
			);
		}
	}

	#[test]
	fn diff_component_opens_editor_for_current_file() {
		let env = Environment::test_env();
		let mut diff = DiffComponent::new(&env, false);

		diff.focus(true);
		diff.current.path = String::from("src/main.rs");

		let event = Event::Key(KeyEvent::new(
			KeyCode::Char('e'),
			KeyModifiers::empty(),
		));

		assert!(matches!(
			diff.event(&event).unwrap(),
			EventState::Consumed
		));

		let event = env.queue.pop();
		assert!(matches!(
			event,
			Some(InternalEvent::OpenExternalEditor(Some(path)))
				if path.path == "src/main.rs" && path.line.is_none()
		));
	}

	#[test]
	fn diff_component_opens_editor_at_selected_new_line() {
		let env = Environment::test_env();
		let mut diff = DiffComponent::new(&env, false);

		diff.focus(true);
		diff.current.path = String::from("src/main.rs");
		diff.diff = Some(test_file_diff(vec![
			test_diff_line(
				"@@ -10,2 +10,2 @@",
				DiffLineType::Header,
				None,
				None,
			),
			test_diff_line(
				"old",
				DiffLineType::Delete,
				Some(10),
				None,
			),
			test_diff_line("new", DiffLineType::Add, None, Some(10)),
		]));
		diff.selection = Selection::Single(2);

		let event = Event::Key(KeyEvent::new(
			KeyCode::Char('e'),
			KeyModifiers::empty(),
		));

		assert!(matches!(
			diff.event(&event).unwrap(),
			EventState::Consumed
		));

		let event = env.queue.pop();
		assert!(matches!(
			event,
			Some(InternalEvent::OpenExternalEditor(Some(path)))
				if path.path == "src/main.rs" && path.line == Some(10)
		));
	}

	#[test]
	fn diff_component_opens_editor_at_nearest_line_from_header() {
		let env = Environment::test_env();
		let mut diff = DiffComponent::new(&env, false);

		diff.focus(true);
		diff.current.path = String::from("src/main.rs");
		diff.diff = Some(test_file_diff(vec![
			test_diff_line(
				"@@ -20,2 +20,2 @@",
				DiffLineType::Header,
				None,
				None,
			),
			test_diff_line(
				"context",
				DiffLineType::None,
				Some(20),
				Some(20),
			),
		]));
		diff.selection = Selection::Single(0);

		let event = Event::Key(KeyEvent::new(
			KeyCode::Char('e'),
			KeyModifiers::empty(),
		));

		assert!(matches!(
			diff.event(&event).unwrap(),
			EventState::Consumed
		));

		let event = env.queue.pop();
		assert!(matches!(
			event,
			Some(InternalEvent::OpenExternalEditor(Some(path)))
				if path.path == "src/main.rs" && path.line == Some(20)
		));
	}

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

	fn test_file_diff_hunks(hunks: Vec<Vec<DiffLine>>) -> FileDiff {
		let lines = hunks.iter().map(Vec::len).sum();
		FileDiff {
			lines,
			hunks: hunks
				.into_iter()
				.enumerate()
				.map(|(i, lines)| Hunk {
					header_hash: i as u64,
					lines,
				})
				.collect(),
			..Default::default()
		}
	}

	fn line_content(line: &Line<'_>) -> String {
		line.spans
			.iter()
			.map(|span| span.content.as_ref())
			.collect()
	}

	fn strip_side_by_side_hunk_marker(line: &str) -> &str {
		line.strip_prefix(symbols::line::VERTICAL)
			.or_else(|| line.strip_prefix(symbols::line::BOTTOM_LEFT))
			.or_else(|| line.strip_prefix(symbols::line::TOP_LEFT))
			.unwrap_or(line)
	}

	#[test]
	fn diff_component_draws_line_stats_in_title() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.current.path = String::from("src/main.rs");
		component.diff = Some(test_file_diff(vec![
			test_diff_line(
				"@@ -1,2 +1,3 @@",
				DiffLineType::Header,
				None,
				None,
			),
			test_diff_line(
				"old",
				DiffLineType::Delete,
				Some(1),
				None,
			),
			test_diff_line("new", DiffLineType::Add, None, Some(1)),
			test_diff_line(
				"another",
				DiffLineType::Add,
				None,
				Some(2),
			),
			test_diff_line(
				"context",
				DiffLineType::None,
				Some(2),
				Some(3),
			),
		]));

		let mut terminal = ratatui::Terminal::new(
			ratatui::backend::TestBackend::new(48, 5),
		)
		.expect("Unable to set up terminal");

		terminal
			.draw(|frame| {
				component
					.draw(frame, Rect::new(0, 0, 48, 5))
					.expect("Draw failed");
			})
			.expect("Draw failed");

		let rendered = terminal.backend().to_string();
		let title_line = rendered
			.lines()
			.find(|line| line.contains("Diff:"))
			.unwrap_or_default();
		assert!(
			title_line.contains("+2 -1"),
			"title line: {title_line:?}"
		);
	}

	#[test]
	fn unified_text_renders_headers_changes_empty_lines_and_context()
	{
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.diff = Some(test_file_diff(vec![
			test_diff_line(
				"@@ -1,3 +1,3 @@",
				DiffLineType::Header,
				None,
				None,
			),
			test_diff_line(
				"old",
				DiffLineType::Delete,
				Some(1),
				None,
			),
			test_diff_line("", DiffLineType::Add, None, Some(1)),
			test_diff_line(
				"context",
				DiffLineType::None,
				Some(2),
				Some(2),
			),
		]));

		let lines = component
			.get_text(40, 10)
			.iter()
			.map(line_content)
			.collect::<Vec<_>>();

		assert_eq!(lines.len(), 4);
		assert!(lines[0].contains("@@ -1,3 +1,3 @@"));
		assert!(lines[1].contains("old"));
		assert!(lines[2].contains('¶'));
		assert!(lines[3].contains("context"));
	}

	#[test]
	fn side_by_side_text_renders_full_width_header_separator_and_cells(
	) {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.current_size.set((MIN_SIDE_BY_SIDE_WIDTH, 20));
		component.view_mode = DiffViewMode::SideBySide;
		component.diff = Some(test_file_diff(vec![
			test_diff_line(
				"@@ -1 +1 @@",
				DiffLineType::Header,
				None,
				None,
			),
			test_diff_line(
				"old",
				DiffLineType::Delete,
				Some(1),
				None,
			),
			test_diff_line("new", DiffLineType::Add, None, Some(1)),
		]));

		let lines = component
			.get_text(MIN_SIDE_BY_SIDE_WIDTH, 10)
			.iter()
			.map(line_content)
			.collect::<Vec<_>>();

		assert_eq!(lines.len(), 2);
		assert!(lines[0].contains("@@ -1 +1 @@"));
		assert!(lines[1].contains("old"));
		assert!(lines[1].contains(symbols::line::VERTICAL));
		assert!(lines[1].contains("new"));
	}

	#[test]
	fn side_by_side_text_renders_selected_hunk_marker_column() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.focus(true);
		component.current_size.set((MIN_SIDE_BY_SIDE_WIDTH, 20));
		component.view_mode = DiffViewMode::SideBySide;
		component.selected_hunk = Some(0);
		component.diff = Some(test_file_diff(vec![
			test_diff_line(
				"@@ -1,2 +1,2 @@",
				DiffLineType::Header,
				None,
				None,
			),
			test_diff_line(
				"context",
				DiffLineType::None,
				Some(1),
				Some(1),
			),
			test_diff_line(
				"old",
				DiffLineType::Delete,
				Some(2),
				None,
			),
			test_diff_line("new", DiffLineType::Add, None, Some(2)),
		]));

		let lines = component.get_text(MIN_SIDE_BY_SIDE_WIDTH, 10);

		assert_eq!(
			lines[0].spans[0].content.as_ref(),
			symbols::line::TOP_LEFT
		);
		assert_eq!(
			lines[0].spans[0].style,
			component.theme.diff_hunk_marker(true)
		);
		assert_eq!(
			lines[1].spans[0].content.as_ref(),
			symbols::line::VERTICAL
		);
		assert_eq!(
			lines[1].spans[0].style,
			component.theme.diff_hunk_marker(true)
		);
	}

	#[test]
	fn unified_render_override_ignores_side_by_side_view_mode() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.current_size.set((MIN_SIDE_BY_SIDE_WIDTH, 20));
		component.view_mode = DiffViewMode::SideBySide;
		component
			.render_view_mode_override
			.set(Some(DiffViewMode::Unified));
		component.diff = Some(test_file_diff(vec![
			test_diff_line(
				"old",
				DiffLineType::Delete,
				Some(1),
				None,
			),
			test_diff_line("new", DiffLineType::Add, None, Some(1)),
		]));

		let lines = component
			.get_text(MIN_SIDE_BY_SIDE_WIDTH, 10)
			.iter()
			.map(line_content)
			.collect::<Vec<_>>();

		assert_eq!(lines.len(), 2);
		assert!(lines[0].contains("old"));
		assert!(lines[1].contains("new"));
		assert!(!lines[0].contains("new"));
		assert!(!lines[1].contains("old"));
	}

	#[test]
	fn side_by_side_wraps_long_left_cell_with_fixed_separator() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.current_size.set((MIN_SIDE_BY_SIDE_WIDTH, 20));
		component.view_mode = DiffViewMode::SideBySide;
		let cell_width = usize::from(component.side_cell_width());
		let left = "x".repeat(cell_width + 8);
		component.diff = Some(test_file_diff(vec![
			test_diff_line(
				&left,
				DiffLineType::Delete,
				Some(1),
				None,
			),
			test_diff_line("new", DiffLineType::Add, None, Some(1)),
		]));

		let lines = component
			.get_text(MIN_SIDE_BY_SIDE_WIDTH, 10)
			.iter()
			.map(line_content)
			.collect::<Vec<_>>();

		assert_eq!(lines.len(), 2);
		for line in &lines {
			let line = strip_side_by_side_hunk_marker(line);
			assert_eq!(
				line.find(symbols::line::VERTICAL),
				Some(cell_width)
			);
		}
		assert!(strip_side_by_side_hunk_marker(&lines[0])
			.starts_with(&"x".repeat(cell_width)));
		assert!(lines[0].contains("new"));
		assert!(strip_side_by_side_hunk_marker(&lines[1])
			.starts_with(&"x".repeat(8)));
	}

	#[test]
	fn side_by_side_wraps_left_cell_by_display_width() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.current_size.set((MIN_SIDE_BY_SIDE_WIDTH, 20));
		component.view_mode = DiffViewMode::SideBySide;
		let cell_width = usize::from(component.side_cell_width());
		let left = "あ".repeat(cell_width);
		component.diff = Some(test_file_diff(vec![
			test_diff_line(
				&left,
				DiffLineType::Delete,
				Some(1),
				None,
			),
			test_diff_line("new", DiffLineType::Add, None, Some(1)),
		]));

		let lines = component
			.get_text(MIN_SIDE_BY_SIDE_WIDTH, 10)
			.iter()
			.map(line_content)
			.collect::<Vec<_>>();

		assert!(lines.len() > 1);
		for line in &lines {
			let line = strip_side_by_side_hunk_marker(line);
			let (left, _) =
				line.split_once(symbols::line::VERTICAL).unwrap();
			assert_eq!(left.width(), cell_width);
		}
	}

	#[test]
	fn side_by_side_continuation_keeps_short_side_line_style() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.current_size.set((MIN_SIDE_BY_SIDE_WIDTH, 20));
		component.view_mode = DiffViewMode::SideBySide;
		let cell_width = usize::from(component.side_cell_width());
		let right = "y".repeat(cell_width + 8);
		component.diff = Some(test_file_diff(vec![
			test_diff_line(
				"old",
				DiffLineType::Delete,
				Some(1),
				None,
			),
			test_diff_line(&right, DiffLineType::Add, None, Some(1)),
		]));

		let lines = component.get_text(MIN_SIDE_BY_SIDE_WIDTH, 10);

		assert_eq!(lines.len(), 2);
		assert_eq!(
			lines[1].spans[1].content.as_ref(),
			" ".repeat(cell_width)
		);
		assert_eq!(
			lines[1].spans[1].style,
			component.theme.diff_line(DiffLineType::Delete, false)
		);
	}

	#[test]
	fn side_by_side_narrow_fallback_renders_unified_lines_but_uses_side_selection_model(
	) {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.current_size.set((MIN_SIDE_BY_SIDE_WIDTH - 1, 20));
		component.view_mode = DiffViewMode::SideBySide;
		component.diff = Some(test_file_diff(vec![
			test_diff_line(
				"old",
				DiffLineType::Delete,
				Some(3),
				None,
			),
			test_diff_line("new", DiffLineType::Add, None, Some(4)),
		]));
		component.selection = Selection::Single(0);

		let lines = component
			.get_text(MIN_SIDE_BY_SIDE_WIDTH - 1, 10)
			.iter()
			.map(line_content)
			.collect::<Vec<_>>();

		assert_eq!(lines.len(), 2);
		assert!(lines[0].contains("old"));
		assert!(lines[1].contains("new"));
		assert_eq!(
			component.selected_lines(),
			vec![
				DiffLinePosition {
					old_lineno: Some(3),
					new_lineno: None,
				},
				DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(4),
				},
			]
		);
	}

	#[test]
	fn side_by_side_selection_skips_context_lines_for_line_actions() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.current.path = "src/lib.rs".to_string();
		component.current_size.set((MIN_SIDE_BY_SIDE_WIDTH, 20));
		component.view_mode = DiffViewMode::SideBySide;
		component.diff = Some(test_file_diff(vec![
			test_diff_line(
				"context",
				DiffLineType::None,
				Some(2),
				Some(2),
			),
			test_diff_line(
				"old",
				DiffLineType::Delete,
				Some(3),
				None,
			),
			test_diff_line("new", DiffLineType::Add, None, Some(3)),
		]));
		component.selection = Selection::Multiple(0, 1);

		assert_eq!(
			component.selected_lines(),
			vec![
				DiffLinePosition {
					old_lineno: Some(3),
					new_lineno: None,
				},
				DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(3),
				},
			]
		);

		component.reset_lines();

		assert!(matches!(
			env.queue.pop(),
			Some(InternalEvent::ConfirmAction(Action::ResetLines(
				path,
				lines
			))) if path == "src/lib.rs" && lines == component.selected_lines()
		));
	}

	#[test]
	fn toggling_view_mode_preserves_selected_hunk() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.current_size.set((MIN_SIDE_BY_SIDE_WIDTH, 20));
		component.diff = Some(test_file_diff_hunks(vec![
			vec![
				test_diff_line(
					"@@ -1 +1 @@",
					DiffLineType::Header,
					None,
					None,
				),
				test_diff_line(
					"old",
					DiffLineType::Delete,
					Some(1),
					None,
				),
				test_diff_line(
					"new",
					DiffLineType::Add,
					None,
					Some(1),
				),
			],
			vec![
				test_diff_line(
					"@@ -5 +5 @@",
					DiffLineType::Header,
					None,
					None,
				),
				test_diff_line(
					"next",
					DiffLineType::Add,
					None,
					Some(5),
				),
			],
		]));
		component.selection = Selection::Single(3);
		component.selected_hunk = Some(1);

		component.toggle_view_mode();

		assert!(matches!(
			component.view_mode,
			DiffViewMode::SideBySide
		));
		assert_eq!(component.selected_hunk, Some(1));
		assert!(matches!(component.selection, Selection::Single(2)));

		component.toggle_view_mode();

		assert!(matches!(component.view_mode, DiffViewMode::Unified));
		assert_eq!(component.selected_hunk, Some(1));
		assert!(matches!(component.selection, Selection::Single(3)));
	}

	#[test]
	fn side_by_side_selected_display_line_maps_to_raw_positions() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.current_size.set((MIN_SIDE_BY_SIDE_WIDTH, 20));
		component.view_mode = DiffViewMode::SideBySide;
		component.diff = Some(test_file_diff(vec![
			test_diff_line(
				"foo",
				DiffLineType::Delete,
				Some(3),
				None,
			),
			test_diff_line("bar", DiffLineType::Add, None, Some(4)),
		]));
		component.selection = Selection::Single(0);

		assert_eq!(
			component.selected_lines(),
			vec![
				DiffLinePosition {
					old_lineno: Some(3),
					new_lineno: None,
				},
				DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(4),
				},
			]
		);
	}

	#[test]
	fn side_by_side_fallback_width_keeps_selected_line_mapping() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.current_size.set((MIN_SIDE_BY_SIDE_WIDTH - 1, 20));
		component.view_mode = DiffViewMode::SideBySide;
		component.diff = Some(test_file_diff(vec![
			test_diff_line(
				"foo",
				DiffLineType::Delete,
				Some(3),
				None,
			),
			test_diff_line("bar", DiffLineType::Add, None, Some(4)),
		]));
		component.selection = Selection::Single(0);

		assert_eq!(
			component.selected_lines(),
			vec![
				DiffLinePosition {
					old_lineno: Some(3),
					new_lineno: None,
				},
				DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(4),
				},
			]
		);
	}

	#[test]
	fn diff_context_keys_update_options_and_queue_diff_refresh() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.focus(true);
		component.diff = Some(test_file_diff(vec![test_diff_line(
			"@@ -1 +1 @@",
			DiffLineType::Header,
			None,
			None,
		)]));

		let increase = Event::Key(KeyEvent::new(
			KeyCode::Char('+'),
			KeyModifiers::empty(),
		));
		assert!(matches!(
			component.event(&increase).unwrap(),
			EventState::Consumed
		));
		assert_eq!(env.options.borrow().diff_options().context, 2);
		assert!(matches!(
			env.queue.pop(),
			Some(InternalEvent::Update(update))
				if update.contains(NeedsUpdate::DIFF)
					&& update.contains(NeedsUpdate::COMMANDS)
		));

		let decrease = Event::Key(KeyEvent::new(
			KeyCode::Char('-'),
			KeyModifiers::empty(),
		));
		assert!(matches!(
			component.event(&decrease).unwrap(),
			EventState::Consumed
		));
		assert_eq!(env.options.borrow().diff_options().context, 1);
		assert!(matches!(
			env.queue.pop(),
			Some(InternalEvent::Update(update))
				if update.contains(NeedsUpdate::DIFF)
					&& update.contains(NeedsUpdate::COMMANDS)
		));
	}
}
