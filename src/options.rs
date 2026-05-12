use anyhow::Result;
use asyncgit::sync::{
	diff::DiffOptions, repo_dir, RepoPathRef,
	ShowUntrackedFilesConfig,
};
use ron::{
	de::from_bytes,
	ser::{to_string_pretty, PrettyConfig},
};
use serde::{Deserialize, Serialize};
use std::{
	cell::RefCell,
	fs::File,
	io::{Read, Write},
	path::PathBuf,
	rc::Rc,
};

#[derive(Clone, Serialize, Deserialize)]
struct OptionsData {
	pub tab: usize,
	pub diff: DiffOptions,
	pub status_show_untracked: Option<ShowUntrackedFilesConfig>,
	pub commit_msgs: Vec<String>,
	#[serde(default = "default_diff_syntax_highlight")]
	pub diff_syntax_highlight: bool,
	#[serde(default = "default_diff_syntax_max_file_bytes")]
	pub diff_syntax_max_file_bytes: u64,
	#[serde(default = "default_diff_syntax_max_file_lines")]
	pub diff_syntax_max_file_lines: usize,
}

impl Default for OptionsData {
	fn default() -> Self {
		Self {
			tab: 0,
			diff: DiffOptions::default(),
			status_show_untracked: None,
			commit_msgs: Vec::new(),
			diff_syntax_highlight: default_diff_syntax_highlight(),
			diff_syntax_max_file_bytes:
				default_diff_syntax_max_file_bytes(),
			diff_syntax_max_file_lines:
				default_diff_syntax_max_file_lines(),
		}
	}
}

const COMMIT_MSG_HISTORY_LENGTH: usize = 20;

const fn default_diff_syntax_highlight() -> bool {
	true
}

const fn default_diff_syntax_max_file_bytes() -> u64 {
	524_288
}

const fn default_diff_syntax_max_file_lines() -> usize {
	10_000
}

#[derive(Clone)]
pub struct Options {
	repo: RepoPathRef,
	data: OptionsData,
}

#[cfg(test)]
impl Options {
	pub fn test_env() -> Self {
		use asyncgit::sync::RepoPath;
		Self {
			repo: RefCell::new(RepoPath::Path(Default::default())),
			data: Default::default(),
		}
	}
}

pub type SharedOptions = Rc<RefCell<Options>>;

impl Options {
	pub fn new(repo: RepoPathRef) -> SharedOptions {
		Rc::new(RefCell::new(Self {
			data: Self::read(&repo).unwrap_or_default(),
			repo,
		}))
	}

	pub fn set_current_tab(&mut self, tab: usize) {
		self.data.tab = tab;
		self.save();
	}

	pub const fn current_tab(&self) -> usize {
		self.data.tab
	}

	pub const fn diff_options(&self) -> DiffOptions {
		self.data.diff
	}

	pub const fn diff_syntax_highlight(&self) -> bool {
		self.data.diff_syntax_highlight
	}

	pub const fn diff_syntax_max_file_bytes(&self) -> u64 {
		self.data.diff_syntax_max_file_bytes
	}

	pub const fn diff_syntax_max_file_lines(&self) -> usize {
		self.data.diff_syntax_max_file_lines
	}

	pub const fn status_show_untracked(
		&self,
	) -> Option<ShowUntrackedFilesConfig> {
		self.data.status_show_untracked
	}

	pub fn set_status_show_untracked(
		&mut self,
		value: Option<ShowUntrackedFilesConfig>,
	) {
		self.data.status_show_untracked = value;
		self.save();
	}

	pub fn diff_context_change(&mut self, increase: bool) {
		self.data.diff.context = if increase {
			self.data.diff.context.saturating_add(1)
		} else {
			self.data.diff.context.saturating_sub(1)
		};

		self.save();
	}

	pub fn diff_hunk_lines_change(&mut self, increase: bool) {
		self.data.diff.interhunk_lines = if increase {
			self.data.diff.interhunk_lines.saturating_add(1)
		} else {
			self.data.diff.interhunk_lines.saturating_sub(1)
		};

		self.save();
	}

	pub fn diff_toggle_whitespace(&mut self) {
		self.data.diff.ignore_whitespace =
			!self.data.diff.ignore_whitespace;

		self.save();
	}

	pub fn add_commit_msg(&mut self, msg: &str) {
		self.data.commit_msgs.push(msg.to_owned());
		while self.data.commit_msgs.len() > COMMIT_MSG_HISTORY_LENGTH
		{
			self.data.commit_msgs.remove(0);
		}
		self.save();
	}

	pub const fn has_commit_msg_history(&self) -> bool {
		!self.data.commit_msgs.is_empty()
	}

	pub fn commit_msg(&self, idx: usize) -> Option<String> {
		if self.data.commit_msgs.is_empty() {
			None
		} else {
			let entries = self.data.commit_msgs.len();
			let mut index = idx;

			while index >= entries {
				index -= entries;
			}

			index = entries.saturating_sub(1) - index;

			Some(self.data.commit_msgs[index].clone())
		}
	}

	fn save(&self) {
		if let Err(e) = self.save_failable() {
			log::error!("options save error: {e}");
		}
	}

	fn read(repo: &RepoPathRef) -> Result<OptionsData> {
		let dir = Self::options_file(repo)?;

		let mut f = File::open(dir)?;
		let mut buffer = Vec::new();
		f.read_to_end(&mut buffer)?;
		Ok(from_bytes(&buffer)?)
	}

	fn save_failable(&self) -> Result<()> {
		let dir = Self::options_file(&self.repo)?;

		let mut file = File::create(dir)?;
		let data =
			to_string_pretty(&self.data, PrettyConfig::default())?;
		file.write_all(data.as_bytes())?;

		Ok(())
	}

	fn options_file(repo: &RepoPathRef) -> Result<PathBuf> {
		let dir = repo_dir(&repo.borrow())?;
		let dir = dir.join("gitui");
		Ok(dir)
	}
}
