use crate::{
	string_utils::{tabs_to_spaces, trim_offset},
	ui::{style::SharedTheme, SyntaxText},
	AsyncAppNotification, DiffSyntaxHighlightProgress,
};
use anyhow::Result;
use asyncgit::{
	asyncjob::{AsyncJob, RunParams},
	hash,
	sync::{
		commit_files::OldNew,
		diff::{
			diff_commit_file_content, diff_commit_parent,
			diff_head_file_content, diff_index_file_content,
			diff_worktree_file_content, FileContent,
		},
		utils::repo_dir,
		RepoPath,
	},
	DiffLine, DiffLineType, DiffParams, DiffType, Error,
	ProgressPercent,
};
use once_cell::sync::Lazy;
use ratatui::{
	style::{Color, Style},
	text::Span,
};
use ron::{
	de::from_bytes,
	ser::{to_string_pretty, PrettyConfig},
};
use serde::{Deserialize, Serialize};
use std::{
	borrow::Cow,
	collections::VecDeque,
	fs::File,
	io::{Read, Write},
	path::Path,
	path::PathBuf,
	sync::{Arc, Mutex},
};
use unicode_width::UnicodeWidthStr;

#[derive(
	Clone, Hash, PartialEq, Eq, Debug, Serialize, Deserialize,
)]
pub struct HighlightedDiffKey {
	pub path: String,
	pub diff_hash: u64,
	pub diff_params_hash: u64,
	pub syntax_theme: String,
	pub tab_width: usize,
}

impl HighlightedDiffKey {
	pub fn new(
		path: String,
		diff_hash: u64,
		params: &DiffParams,
		syntax_theme: String,
		tab_width: usize,
	) -> Self {
		Self {
			path,
			diff_hash,
			diff_params_hash: hash(params),
			syntax_theme,
			tab_width,
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HighlightSkipReason {
	Binary,
	TooLarge { bytes: u64, max_bytes: u64 },
	TooManyLines { lines: usize, max_lines: usize },
	MissingContent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HighlightStatus {
	Loading,
	Ready,
	Failed(String),
	Skipped(HighlightSkipReason),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightedSpan {
	pub content: String,
	pub style: Style,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightedLine {
	pub spans: Vec<HighlightedSpan>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightedFile {
	pub path: String,
	pub line_count: usize,
	pub lines: Vec<HighlightedLine>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightedDiff {
	pub key: HighlightedDiffKey,
	pub old: Option<HighlightedFile>,
	pub new: Option<HighlightedFile>,
	pub status: HighlightStatus,
}

const HIGHLIGHTED_DIFF_CACHE_CAPACITY: usize = 16;
const HIGHLIGHTED_DIFF_CACHE_VERSION: u32 = 1;
const HIGHLIGHTED_DIFF_CACHE_FILENAME: &str =
	"gitui_diff_syntax_cache.ron";
static HIGHLIGHTED_DIFF_CACHE: Lazy<
	Mutex<VecDeque<HighlightedDiff>>,
> = Lazy::new(|| Mutex::new(VecDeque::new()));
static HIGHLIGHTED_DIFF_CACHE_PATH: Lazy<Mutex<Option<PathBuf>>> =
	Lazy::new(|| Mutex::new(None));

#[derive(Serialize, Deserialize)]
struct HighlightedDiffCacheFile {
	version: u32,
	entries: Vec<HighlightedDiff>,
}

impl HighlightedDiff {
	pub const fn is_ready(&self) -> bool {
		matches!(self.status, HighlightStatus::Ready)
	}
}

pub fn cached_highlighted_diff(
	key: &HighlightedDiffKey,
) -> Option<HighlightedDiff> {
	let mut cache = HIGHLIGHTED_DIFF_CACHE.lock().ok()?;
	let index = cache.iter().position(|diff| diff.key == *key)?;
	let diff = cache.remove(index)?;
	cache.push_front(diff.clone());
	Some(diff)
}

pub fn init_highlighted_diff_cache(repo: &RepoPath) -> Result<()> {
	let cache_path =
		repo_dir(repo)?.join(HIGHLIGHTED_DIFF_CACHE_FILENAME);

	if let Ok(mut path) = HIGHLIGHTED_DIFF_CACHE_PATH.lock() {
		*path = Some(cache_path.clone());
	}
	if let Ok(mut cache) = HIGHLIGHTED_DIFF_CACHE.lock() {
		cache.clear();
	}

	let mut file = match File::open(cache_path) {
		Ok(file) => file,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			return Ok(());
		}
		Err(e) => return Err(e.into()),
	};
	let mut buffer = Vec::new();
	file.read_to_end(&mut buffer)?;
	let cache_file: HighlightedDiffCacheFile = from_bytes(&buffer)?;
	if cache_file.version != HIGHLIGHTED_DIFF_CACHE_VERSION {
		return Ok(());
	}

	if let Ok(mut cache) = HIGHLIGHTED_DIFF_CACHE.lock() {
		*cache = cache_file
			.entries
			.into_iter()
			.filter(|diff| {
				!matches!(diff.status, HighlightStatus::Loading)
			})
			.take(HIGHLIGHTED_DIFF_CACHE_CAPACITY)
			.collect();
	}

	Ok(())
}

pub fn cache_highlighted_diff(diff: HighlightedDiff) {
	if matches!(diff.status, HighlightStatus::Loading) {
		return;
	}

	let mut snapshot = None;
	if let Ok(mut cache) = HIGHLIGHTED_DIFF_CACHE.lock() {
		if let Some(index) =
			cache.iter().position(|cached| cached.key == diff.key)
		{
			cache.remove(index);
		}

		cache.push_front(diff);
		cache.truncate(HIGHLIGHTED_DIFF_CACHE_CAPACITY);
		snapshot = Some(cache.iter().cloned().collect::<Vec<_>>());
	}

	let path = HIGHLIGHTED_DIFF_CACHE_PATH
		.lock()
		.ok()
		.and_then(|path| path.clone());
	if let (Some(path), Some(entries)) = (path, snapshot) {
		rayon_core::spawn(move || {
			if let Err(e) =
				persist_highlighted_diff_cache(path, entries)
			{
				log::error!("diff syntax cache save error: {e}");
			}
		});
	}
}

pub fn flush_highlighted_diff_cache() -> Result<()> {
	let path = HIGHLIGHTED_DIFF_CACHE_PATH
		.lock()
		.ok()
		.and_then(|path| path.clone());
	let entries = HIGHLIGHTED_DIFF_CACHE
		.lock()
		.ok()
		.map(|cache| cache.iter().cloned().collect::<Vec<_>>());

	if let (Some(path), Some(entries)) = (path, entries) {
		persist_highlighted_diff_cache(path, entries)?;
	}

	Ok(())
}

fn persist_highlighted_diff_cache(
	path: PathBuf,
	entries: Vec<HighlightedDiff>,
) -> Result<()> {
	let data = HighlightedDiffCacheFile {
		version: HIGHLIGHTED_DIFF_CACHE_VERSION,
		entries,
	};
	let data = to_string_pretty(&data, PrettyConfig::default())?;
	let mut file = File::create(path)?;
	file.write_all(data.as_bytes())?;
	Ok(())
}

#[derive(Clone)]
pub struct DiffSyntaxRequest {
	key: HighlightedDiffKey,
	repo: RepoPath,
	diff_type: DiffType,
	max_file_bytes: u64,
	max_file_lines: usize,
}

enum DiffSyntaxJobState {
	Request(DiffSyntaxRequest),
	Response(HighlightedDiff),
}

#[derive(Clone, Default)]
pub struct AsyncDiffSyntaxJob {
	state: Arc<Mutex<Option<DiffSyntaxJobState>>>,
}

impl AsyncDiffSyntaxJob {
	pub fn new(
		key: HighlightedDiffKey,
		repo: RepoPath,
		diff_type: DiffType,
		max_file_bytes: u64,
		max_file_lines: usize,
	) -> Self {
		Self {
			state: Arc::new(Mutex::new(Some(
				DiffSyntaxJobState::Request(DiffSyntaxRequest {
					key,
					repo,
					diff_type,
					max_file_bytes,
					max_file_lines,
				}),
			))),
		}
	}

	pub fn result(&self) -> Option<HighlightedDiff> {
		if let Ok(mut state) = self.state.lock() {
			if let Some(state) = state.take() {
				return match state {
					DiffSyntaxJobState::Request(_) => None,
					DiffSyntaxJobState::Response(diff) => Some(diff),
				};
			}
		}

		None
	}
}

impl AsyncJob for AsyncDiffSyntaxJob {
	type Notification = AsyncAppNotification;
	type Progress = ProgressPercent;

	fn run(
		&mut self,
		params: RunParams<Self::Notification, Self::Progress>,
	) -> asyncgit::Result<Self::Notification> {
		let mut state = self.state.lock()?;

		if let Some(DiffSyntaxJobState::Request(request)) =
			state.take()
		{
			let key = request.key.clone();
			let highlighted =
				match build_highlighted_diff(request, &params) {
					Ok(diff) => diff,
					Err(e) => HighlightedDiff {
						key,
						old: None,
						new: None,
						status: HighlightStatus::Failed(
							e.to_string(),
						),
					},
				};

			*state = Some(DiffSyntaxJobState::Response(highlighted));
		}

		Ok(AsyncAppNotification::DiffSyntaxHighlighting(
			DiffSyntaxHighlightProgress::Done,
		))
	}
}

fn build_highlighted_diff(
	request: DiffSyntaxRequest,
	params: &RunParams<AsyncAppNotification, ProgressPercent>,
) -> asyncgit::Result<HighlightedDiff> {
	let (old, new) = match load_diff_content(
		&request.repo,
		&request.key.path,
		&request.diff_type,
	) {
		Ok(content) => content,
		Err(Error::BinaryFile) => {
			return Ok(HighlightedDiff {
				key: request.key,
				old: None,
				new: None,
				status: HighlightStatus::Skipped(
					HighlightSkipReason::Binary,
				),
			});
		}
		Err(e) => return Err(e),
	};

	let skip = old
		.as_ref()
		.and_then(|content| {
			should_skip(
				content,
				request.max_file_bytes,
				request.max_file_lines,
			)
		})
		.or_else(|| {
			new.as_ref().and_then(|content| {
				should_skip(
					content,
					request.max_file_bytes,
					request.max_file_lines,
				)
			})
		});
	if let Some(reason) = skip {
		return Ok(HighlightedDiff {
			key: request.key,
			old: None,
			new: None,
			status: HighlightStatus::Skipped(reason),
		});
	}

	if old.is_none() && new.is_none() {
		return Ok(HighlightedDiff {
			key: request.key,
			old: None,
			new: None,
			status: HighlightStatus::Skipped(
				HighlightSkipReason::MissingContent,
			),
		});
	}

	let syntax_theme = request.key.syntax_theme.clone();
	let old = match old {
		Some(content) => {
			Some(highlight_file(content, &syntax_theme, params)?)
		}
		None => None,
	};
	let new = match new {
		Some(content) => {
			Some(highlight_file(content, &syntax_theme, params)?)
		}
		None => None,
	};

	Ok(HighlightedDiff {
		key: request.key,
		old,
		new,
		status: HighlightStatus::Ready,
	})
}

fn should_skip(
	content: &FileContent,
	max_file_bytes: u64,
	max_file_lines: usize,
) -> Option<HighlightSkipReason> {
	if content.bytes > max_file_bytes {
		return Some(HighlightSkipReason::TooLarge {
			bytes: content.bytes,
			max_bytes: max_file_bytes,
		});
	}

	let lines = content.content.lines().count();
	if lines > max_file_lines {
		return Some(HighlightSkipReason::TooManyLines {
			lines,
			max_lines: max_file_lines,
		});
	}

	None
}

fn highlight_file(
	content: FileContent,
	syntax_theme: &str,
	params: &RunParams<AsyncAppNotification, ProgressPercent>,
) -> asyncgit::Result<HighlightedFile> {
	let content = FileContent {
		content: tabs_to_spaces(content.content),
		..content
	};
	let syntax = SyntaxText::new_with_progress_notification(
		content.content,
		Path::new(&content.path),
		params,
		syntax_theme,
		AsyncAppNotification::DiffSyntaxHighlighting(
			DiffSyntaxHighlightProgress::Progress,
		),
	)?;

	let line_count = syntax.line_count();
	let lines = (0..line_count)
		.filter_map(|line| {
			syntax
				.line_spans_owned(line)
				.map(|spans| HighlightedLine { spans })
		})
		.collect();

	Ok(HighlightedFile {
		path: content.path,
		line_count,
		lines,
	})
}

fn load_diff_content(
	repo: &RepoPath,
	path: &str,
	diff_type: &DiffType,
) -> asyncgit::Result<(Option<FileContent>, Option<FileContent>)> {
	match diff_type {
		DiffType::WorkDir => Ok((
			diff_index_file_content(repo, path)?,
			diff_worktree_file_content(repo, path)?,
		)),
		DiffType::Stage => Ok((
			diff_head_file_content(repo, path)?,
			diff_index_file_content(repo, path)?,
		)),
		DiffType::Commit(commit) => {
			let old = diff_commit_parent(repo, *commit)?
				.map(|parent| {
					diff_commit_file_content(repo, parent, path)
				})
				.transpose()?
				.flatten();
			let new = diff_commit_file_content(repo, *commit, path)?;
			Ok((old, new))
		}
		DiffType::Commits(OldNew { old, new }) => Ok((
			diff_commit_file_content(repo, *old, path)?,
			diff_commit_file_content(repo, *new, path)?,
		)),
	}
}

fn line_no_to_index(line_no: Option<u32>) -> Option<usize> {
	line_no?.checked_sub(1).map(|line| line as usize)
}

pub fn highlighted_spans_for_unified_line<'a>(
	line: &DiffLine,
	highlighted: &'a HighlightedDiff,
) -> Option<&'a HighlightedLine> {
	match line.line_type {
		DiffLineType::Delete => line_no_to_index(
			line.position.old_lineno,
		)
		.and_then(|idx| highlighted.old.as_ref()?.lines.get(idx)),
		DiffLineType::Add => line_no_to_index(
			line.position.new_lineno,
		)
		.and_then(|idx| highlighted.new.as_ref()?.lines.get(idx)),
		DiffLineType::None => line_no_to_index(
			line.position.new_lineno.or(line.position.old_lineno),
		)
		.and_then(|idx| {
			highlighted
				.new
				.as_ref()
				.or(highlighted.old.as_ref())?
				.lines
				.get(idx)
		}),
		DiffLineType::Header => None,
	}
}

pub fn highlighted_spans_for_side_cell<'a>(
	line: &DiffLine,
	use_old_side: bool,
	highlighted: &'a HighlightedDiff,
) -> Option<&'a HighlightedLine> {
	if use_old_side {
		line_no_to_index(line.position.old_lineno)
			.and_then(|idx| highlighted.old.as_ref()?.lines.get(idx))
	} else {
		line_no_to_index(line.position.new_lineno)
			.and_then(|idx| highlighted.new.as_ref()?.lines.get(idx))
	}
}

pub fn merge_syntax_and_diff_style(
	mut syntax: Style,
	line_type: DiffLineType,
	selected: bool,
	theme: &SharedTheme,
) -> Style {
	if let Some(bg) = theme.diff_line_background(line_type) {
		syntax = syntax.bg(bg);
	}

	theme.apply_selection(syntax, selected)
}

pub fn spans_to_line_content(spans: &[HighlightedSpan]) -> String {
	spans.iter().map(|span| span.content.as_str()).collect()
}

pub fn trim_highlighted_spans(
	spans: &[HighlightedSpan],
	scrolled_right: usize,
) -> Vec<HighlightedSpan> {
	let mut remaining = scrolled_right;
	let mut out = Vec::new();

	for span in spans {
		let trimmed = trim_offset(&span.content, remaining);
		if trimmed.is_empty() {
			remaining =
				remaining.saturating_sub(span.content.width());
			continue;
		}

		remaining = 0;
		out.push(HighlightedSpan {
			content: trimmed.to_string(),
			style: span.style,
		});
	}

	out
}

pub fn highlighted_line_to_spans<'a>(
	line: &HighlightedLine,
	line_type: DiffLineType,
	selected: bool,
	theme: &SharedTheme,
	scrolled_right: usize,
	width: usize,
	fill_width: bool,
	trailing_newline: bool,
) -> Vec<Span<'a>> {
	let mut out = Vec::new();
	let spans = trim_highlighted_spans(&line.spans, scrolled_right);

	if spans.is_empty() && !matches!(line_type, DiffLineType::None) {
		out.push(Span::styled(
			Cow::from(theme.line_break()),
			theme.diff_line(line_type, selected),
		));
	} else {
		for span in spans {
			out.push(Span::styled(
				Cow::from(span.content),
				merge_syntax_and_diff_style(
					span.style, line_type, selected, theme,
				),
			));
		}
	}

	if fill_width || trailing_newline {
		let content = spans_to_line_content(&line.spans);
		let content_width = if line.spans.is_empty()
			&& !matches!(line_type, DiffLineType::None)
		{
			theme.line_break().width()
		} else {
			content.width()
		};
		let visible_width =
			content_width.saturating_sub(scrolled_right);
		let padding = if fill_width {
			width.saturating_sub(visible_width)
		} else {
			0
		};
		let mut suffix = " ".repeat(padding);
		if trailing_newline {
			suffix.push('\n');
		}
		out.push(Span::styled(
			Cow::from(suffix),
			theme
				.diff_line_background(line_type)
				.map_or_else(Style::default, |bg: Color| {
					Style::default().bg(bg)
				})
				.patch(theme.diff_line(DiffLineType::None, selected)),
		));
	}

	out
}
