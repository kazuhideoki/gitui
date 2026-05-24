//! sync git api for fetching a diff

use super::{
	commit_files::{
		get_commit_diff, get_compare_commits_diff, OldNew,
	},
	utils::{get_head_repo, work_dir},
	CommitId, RepoPath, ShowUntrackedFilesConfig,
};
use crate::{
	error::Error,
	error::Result,
	hash,
	sync::{get_stashes, repository::repo},
};
use easy_cast::Conv;
use git2::{
	Blob, Delta, Diff, DiffDelta, DiffFormat, DiffHunk, ObjectType,
	Patch, Repository,
};
use scopetime::scope_time;
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, fs, path::Path, rc::Rc};

/// type of diff of a single line
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash, Debug)]
pub enum DiffLineType {
	/// just surrounding line, no change
	#[default]
	None,
	/// header of the hunk
	Header,
	/// line added
	Add,
	/// line deleted
	Delete,
}

impl From<git2::DiffLineType> for DiffLineType {
	fn from(line_type: git2::DiffLineType) -> Self {
		match line_type {
			git2::DiffLineType::HunkHeader => Self::Header,
			git2::DiffLineType::DeleteEOFNL
			| git2::DiffLineType::Deletion => Self::Delete,
			git2::DiffLineType::AddEOFNL
			| git2::DiffLineType::Addition => Self::Add,
			_ => Self::None,
		}
	}
}

///
#[derive(Default, Clone, Hash, Debug)]
pub struct DiffLine {
	///
	pub content: Box<str>,
	///
	pub line_type: DiffLineType,
	///
	pub position: DiffLinePosition,
}

///
#[derive(Clone, Copy, Default, Hash, Debug, PartialEq, Eq)]
pub struct DiffLinePosition {
	///
	pub old_lineno: Option<u32>,
	///
	pub new_lineno: Option<u32>,
}

impl PartialEq<&git2::DiffLine<'_>> for DiffLinePosition {
	fn eq(&self, other: &&git2::DiffLine) -> bool {
		other.new_lineno() == self.new_lineno
			&& other.old_lineno() == self.old_lineno
	}
}

impl From<&git2::DiffLine<'_>> for DiffLinePosition {
	fn from(line: &git2::DiffLine<'_>) -> Self {
		Self {
			old_lineno: line.old_lineno(),
			new_lineno: line.new_lineno(),
		}
	}
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Hash)]
pub(crate) struct HunkHeader {
	pub old_start: u32,
	pub old_lines: u32,
	pub new_start: u32,
	pub new_lines: u32,
}

impl From<DiffHunk<'_>> for HunkHeader {
	fn from(h: DiffHunk) -> Self {
		Self {
			old_start: h.old_start(),
			old_lines: h.old_lines(),
			new_start: h.new_start(),
			new_lines: h.new_lines(),
		}
	}
}

/// single diff hunk
#[derive(Default, Clone, Hash, Debug)]
pub struct Hunk {
	/// hash of the hunk header
	pub header_hash: u64,
	/// list of `DiffLine`s
	pub lines: Vec<DiffLine>,
}

/// collection of hunks, sum of all diff lines
#[derive(Default, Clone, Hash, Debug)]
pub struct FileDiff {
	/// list of hunks
	pub hunks: Vec<Hunk>,
	/// lines total summed up over hunks
	pub lines: usize,
	///
	pub untracked: bool,
	/// old and new file size in bytes
	pub sizes: (u64, u64),
	/// size delta in bytes
	pub size_delta: i64,
}

/// full file content for one side of a diff
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileContent {
	/// repository-relative path
	pub path: String,
	/// UTF-8 file content
	pub content: String,
	/// original byte length
	pub bytes: u64,
}

/// line change statistics for a diff
#[derive(Default, Clone, Copy, Hash, Debug, PartialEq, Eq)]
pub struct LineStats {
	/// added lines
	pub additions: usize,
	/// deleted lines
	pub deletions: usize,
}

/// see <https://libgit2.org/libgit2/#HEAD/type/git_diff_options>
#[derive(
	Debug, Hash, Clone, Copy, PartialEq, Eq, Serialize, Deserialize,
)]
pub struct DiffOptions {
	/// see <https://libgit2.org/libgit2/#HEAD/type/git_diff_options>
	pub ignore_whitespace: bool,
	/// see <https://libgit2.org/libgit2/#HEAD/type/git_diff_options>
	pub context: u32,
	/// see <https://libgit2.org/libgit2/#HEAD/type/git_diff_options>
	pub interhunk_lines: u32,
}

impl Default for DiffOptions {
	fn default() -> Self {
		Self {
			ignore_whitespace: false,
			context: 1,
			interhunk_lines: 0,
		}
	}
}

pub(crate) fn get_diff_raw<'a>(
	repo: &'a Repository,
	p: &str,
	stage: bool,
	reverse: bool,
	options: Option<DiffOptions>,
) -> Result<Diff<'a>> {
	// scope_time!("get_diff_raw");

	let mut opt = git2::DiffOptions::new();
	if let Some(options) = options {
		opt.context_lines(options.context);
		opt.ignore_whitespace(options.ignore_whitespace);
		opt.interhunk_lines(options.interhunk_lines);
	}
	opt.pathspec(p);
	opt.reverse(reverse);

	let diff = if stage {
		// diff against head
		if let Ok(id) = get_head_repo(repo) {
			let parent = repo.find_commit(id.into())?;

			let tree = parent.tree()?;
			repo.diff_tree_to_index(
				Some(&tree),
				Some(&repo.index()?),
				Some(&mut opt),
			)?
		} else {
			repo.diff_tree_to_index(
				None,
				Some(&repo.index()?),
				Some(&mut opt),
			)?
		}
	} else {
		opt.include_untracked(true);
		opt.recurse_untracked_dirs(true);
		repo.diff_index_to_workdir(None, Some(&mut opt))?
	};

	Ok(diff)
}

/// returns diff of a specific file either in `stage` or workdir
pub fn get_diff(
	repo_path: &RepoPath,
	p: &str,
	stage: bool,
	options: Option<DiffOptions>,
) -> Result<FileDiff> {
	scope_time!("get_diff");

	let repo = repo(repo_path)?;
	let work_dir = work_dir(&repo)?;
	let diff = get_diff_raw(&repo, p, stage, false, options)?;

	raw_diff_to_file_diff(&diff, work_dir)
}

/// returns line change statistics either in `stage` or workdir
pub fn get_diff_line_stats(
	repo_path: &RepoPath,
	stage: bool,
	show_untracked: Option<ShowUntrackedFilesConfig>,
) -> Result<LineStats> {
	let repo = repo(repo_path)?;
	let mut opt = git2::DiffOptions::new();

	let diff = if stage {
		if let Ok(id) = get_head_repo(&repo) {
			let parent = repo.find_commit(id.into())?;
			let tree = parent.tree()?;
			repo.diff_tree_to_index(
				Some(&tree),
				Some(&repo.index()?),
				Some(&mut opt),
			)?
		} else {
			repo.diff_tree_to_index(
				None,
				Some(&repo.index()?),
				Some(&mut opt),
			)?
		}
	} else {
		let show_untracked = if let Some(config) = show_untracked {
			config
		} else {
			crate::sync::config::untracked_files_config_repo(&repo)?
		};

		opt.include_untracked(show_untracked.include_untracked());
		opt.recurse_untracked_dirs(
			show_untracked.recurse_untracked_dirs(),
		);
		opt.show_untracked_content(
			show_untracked.include_untracked(),
		);

		repo.diff_index_to_workdir(None, Some(&mut opt))?
	};

	let stats = diff.stats()?;
	Ok(LineStats {
		additions: stats.insertions(),
		deletions: stats.deletions(),
	})
}

/// returns diff of a specific file inside a commit
/// see `get_commit_diff`
pub fn get_diff_commit(
	repo_path: &RepoPath,
	id: CommitId,
	p: String,
	options: Option<DiffOptions>,
) -> Result<FileDiff> {
	scope_time!("get_diff_commit");

	let repo = repo(repo_path)?;
	let work_dir = work_dir(&repo)?;
	let diff = get_commit_diff(
		&repo,
		id,
		Some(p),
		options,
		Some(&get_stashes(repo_path)?.into_iter().collect()),
	)?;

	raw_diff_to_file_diff(&diff, work_dir)
}

/// get file changes of a diff between two commits
pub fn get_diff_commits(
	repo_path: &RepoPath,
	ids: OldNew<CommitId>,
	p: String,
	options: Option<DiffOptions>,
) -> Result<FileDiff> {
	scope_time!("get_diff_commits");

	let repo = repo(repo_path)?;
	let work_dir = work_dir(&repo)?;
	let diff =
		get_compare_commits_diff(&repo, ids, Some(p), options)?;

	raw_diff_to_file_diff(&diff, work_dir)
}

/// returns the UTF-8 worktree content for diff syntax highlighting
pub fn diff_worktree_file_content(
	repo_path: &RepoPath,
	path: &str,
) -> Result<Option<FileContent>> {
	let repo = repo(repo_path)?;
	let full_path = work_dir(&repo)?.join(path);
	worktree_file_content(path, &full_path)
}

/// returns the UTF-8 index content for diff syntax highlighting
pub fn diff_index_file_content(
	repo_path: &RepoPath,
	path: &str,
) -> Result<Option<FileContent>> {
	let repo = repo(repo_path)?;
	let index = repo.index()?;
	let Some(entry) = index.get_path(Path::new(path), 0) else {
		return Ok(None);
	};
	let blob = repo.find_blob(entry.id)?;
	blob_file_content(path, &blob)
}

/// returns the UTF-8 HEAD content for diff syntax highlighting
pub fn diff_head_file_content(
	repo_path: &RepoPath,
	path: &str,
) -> Result<Option<FileContent>> {
	let repo = repo(repo_path)?;
	let Ok(head) = get_head_repo(&repo) else {
		return Ok(None);
	};
	diff_commit_file_content(repo_path, head, path)
}

/// returns the UTF-8 content for `path` from `commit`
pub fn diff_commit_file_content(
	repo_path: &RepoPath,
	commit: CommitId,
	path: &str,
) -> Result<Option<FileContent>> {
	let repo = repo(repo_path)?;
	let commit = repo.find_commit(commit.into())?;
	let tree = commit.tree()?;
	let entry = match tree.get_path(Path::new(path)) {
		Ok(entry) => entry,
		Err(e) if e.code() == git2::ErrorCode::NotFound => {
			return Ok(None);
		}
		Err(e) => return Err(e.into()),
	};

	if entry.kind() != Some(ObjectType::Blob) {
		return Ok(None);
	}

	let blob = repo.find_blob(entry.id())?;
	blob_file_content(path, &blob)
}

/// returns the first parent of `commit`, if any
pub fn diff_commit_parent(
	repo_path: &RepoPath,
	commit: CommitId,
) -> Result<Option<CommitId>> {
	let repo = repo(repo_path)?;
	let commit = repo.find_commit(commit.into())?;
	Ok(commit.parent_id(0).ok().map(CommitId::new))
}

fn blob_file_content(
	path: &str,
	blob: &Blob<'_>,
) -> Result<Option<FileContent>> {
	if blob.is_binary() {
		return Err(Error::BinaryFile);
	}

	let bytes = blob.content();
	let content = std::str::from_utf8(bytes)
		.map_err(|e| Error::Generic(e.to_string()))?
		.to_string();

	Ok(Some(FileContent {
		path: path.to_string(),
		content,
		bytes: bytes.len() as u64,
	}))
}

fn worktree_file_content(
	path: &str,
	full_path: &Path,
) -> Result<Option<FileContent>> {
	let Ok(meta) = fs::symlink_metadata(full_path) else {
		return Ok(None);
	};

	let bytes = if meta.file_type().is_symlink() {
		let path = fs::read_link(full_path)?;
		path.to_str().map_or_else(Vec::new, |p| {
			p.to_string().as_bytes().into()
		})
	} else if meta.file_type().is_dir() {
		return Ok(None);
	} else {
		fs::read(full_path)?
	};

	if bytes.contains(&0) {
		return Err(Error::BinaryFile);
	}

	let content = String::from_utf8(bytes)
		.map_err(|e| Error::Generic(e.to_string()))?;
	let bytes = content.len() as u64;

	Ok(Some(FileContent {
		path: path.to_string(),
		content,
		bytes,
	}))
}

///
//TODO: refactor into helper type with the inline closures as dedicated functions
#[allow(clippy::too_many_lines)]
fn raw_diff_to_file_diff(
	diff: &Diff,
	work_dir: &Path,
) -> Result<FileDiff> {
	let res = Rc::new(RefCell::new(FileDiff::default()));
	{
		let mut current_lines = Vec::new();
		let mut current_hunk: Option<HunkHeader> = None;

		let res_cell = Rc::clone(&res);
		let adder = move |header: &HunkHeader,
		                  lines: &Vec<DiffLine>| {
			let mut res = res_cell.borrow_mut();
			res.hunks.push(Hunk {
				header_hash: hash(header),
				lines: lines.clone(),
			});
			res.lines += lines.len();
		};

		let res_cell = Rc::clone(&res);
		let mut put = |delta: DiffDelta,
		               hunk: Option<DiffHunk>,
		               line: git2::DiffLine| {
			{
				let mut res = res_cell.borrow_mut();
				res.sizes = (
					delta.old_file().size(),
					delta.new_file().size(),
				);
				//TODO: use try_conv
				res.size_delta = (i64::conv(res.sizes.1))
					.saturating_sub(i64::conv(res.sizes.0));
			}
			if let Some(hunk) = hunk {
				let hunk_header = HunkHeader::from(hunk);

				match current_hunk {
					None => current_hunk = Some(hunk_header),
					Some(h) => {
						if h != hunk_header {
							adder(&h, &current_lines);
							current_lines.clear();
							current_hunk = Some(hunk_header);
						}
					}
				}

				let diff_line = DiffLine {
					position: DiffLinePosition::from(&line),
					content: String::from_utf8_lossy(line.content())
						//Note: trim await trailing newline characters
						.trim_matches(is_newline)
						.into(),
					line_type: line.origin_value().into(),
				};

				current_lines.push(diff_line);
			}
		};

		let new_file_diff = if diff.deltas().len() == 1 {
			if let Some(delta) = diff.deltas().next() {
				if delta.status() == Delta::Untracked {
					let relative_path =
						delta.new_file().path().ok_or_else(|| {
							Error::Generic(
								"new file path is unspecified."
									.to_string(),
							)
						})?;

					let newfile_path = work_dir.join(relative_path);

					if let Some(newfile_content) =
						new_file_content(&newfile_path)
					{
						let mut patch = Patch::from_buffers(
							&[],
							None,
							newfile_content.as_slice(),
							Some(&newfile_path),
							None,
						)?;

						patch.print(
							&mut |delta,
							      hunk: Option<DiffHunk>,
							      line: git2::DiffLine| {
								put(delta, hunk, line);
								true
							},
						)?;

						true
					} else {
						false
					}
				} else {
					false
				}
			} else {
				false
			}
		} else {
			false
		};

		if !new_file_diff {
			diff.print(
				DiffFormat::Patch,
				move |delta, hunk, line: git2::DiffLine| {
					put(delta, hunk, line);
					true
				},
			)?;
		}

		if !current_lines.is_empty() {
			adder(
				&current_hunk.map_or_else(
					|| Err(Error::Generic("invalid hunk".to_owned())),
					Ok,
				)?,
				&current_lines,
			);
		}

		if new_file_diff {
			res.borrow_mut().untracked = true;
		}
	}
	let res = Rc::try_unwrap(res)
		.map_err(|_| Error::Generic("rc unwrap error".to_owned()))?;
	Ok(res.into_inner())
}

const fn is_newline(c: char) -> bool {
	c == '\n' || c == '\r'
}

fn new_file_content(path: &Path) -> Option<Vec<u8>> {
	if let Ok(meta) = fs::symlink_metadata(path) {
		if meta.file_type().is_symlink() {
			if let Ok(path) = fs::read_link(path) {
				return Some(
					path.to_str()?.to_string().as_bytes().into(),
				);
			}
		} else if !meta.file_type().is_dir() {
			if let Ok(content) = fs::read(path) {
				return Some(content);
			}
		}
	}

	None
}

#[cfg(test)]
mod tests {
	use super::{get_diff, get_diff_commit, get_diff_line_stats};
	use crate::{
		error::Result,
		sync::{
			commit, stage_add_file,
			status::{get_status, StatusType},
			tests::{get_statuses, repo_init, repo_init_empty},
			RepoPath,
		},
	};
	use std::{
		fs::{self, File},
		io::Write,
		path::Path,
	};

	#[test]
	fn test_untracked_subfolder() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		assert_eq!(get_statuses(repo_path), (0, 0));

		fs::create_dir(root.join("foo")).unwrap();
		File::create(root.join("foo/bar.txt"))
			.unwrap()
			.write_all(b"test\nfoo")
			.unwrap();

		assert_eq!(get_statuses(repo_path), (1, 0));

		let diff =
			get_diff(repo_path, "foo/bar.txt", false, None).unwrap();

		assert_eq!(diff.hunks.len(), 1);
		assert_eq!(&*diff.hunks[0].lines[1].content, "test");
	}

	#[test]
	fn test_diff_line_stats_for_workdir_and_stage() {
		let file_path = Path::new("foo.txt");
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))
			.unwrap()
			.write_all(b"a\nb\nc\n")
			.unwrap();
		stage_add_file(repo_path, file_path).unwrap();
		commit(repo_path, "add file").unwrap();

		File::create(root.join(file_path))
			.unwrap()
			.write_all(b"a\nb2\nc\nd\n")
			.unwrap();

		let workdir_stats =
			get_diff_line_stats(repo_path, false, None).unwrap();
		assert_eq!(workdir_stats.additions, 2);
		assert_eq!(workdir_stats.deletions, 1);

		stage_add_file(repo_path, file_path).unwrap();

		let stage_stats =
			get_diff_line_stats(repo_path, true, None).unwrap();
		assert_eq!(stage_stats.additions, 2);
		assert_eq!(stage_stats.deletions, 1);

		let workdir_stats =
			get_diff_line_stats(repo_path, false, None).unwrap();
		assert_eq!(workdir_stats.additions, 0);
		assert_eq!(workdir_stats.deletions, 0);
	}

	#[test]
	fn test_empty_repo() {
		let file_path = Path::new("foo.txt");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		assert_eq!(get_statuses(repo_path), (0, 0));

		File::create(root.join(file_path))
			.unwrap()
			.write_all(b"test\nfoo")
			.unwrap();

		assert_eq!(get_statuses(repo_path), (1, 0));

		stage_add_file(repo_path, file_path).unwrap();

		assert_eq!(get_statuses(repo_path), (0, 1));

		let diff = get_diff(
			repo_path,
			file_path.to_str().unwrap(),
			true,
			None,
		)
		.unwrap();

		assert_eq!(diff.hunks.len(), 1);
	}

	static HUNK_A: &str = r"
1   start
2
3
4
5
6   middle
7
8
9
0
1   end";

	static HUNK_B: &str = r"
1   start
2   newa
3
4
5
6   middle
7
8
9
0   newb
1   end";

	#[test]
	fn test_hunks() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		assert_eq!(get_statuses(repo_path), (0, 0));

		let file_path = root.join("bar.txt");

		{
			File::create(&file_path)
				.unwrap()
				.write_all(HUNK_A.as_bytes())
				.unwrap();
		}

		let res = get_status(repo_path, StatusType::WorkingDir, None)
			.unwrap();
		assert_eq!(res.len(), 1);
		assert_eq!(res[0].path, "bar.txt");

		stage_add_file(repo_path, Path::new("bar.txt")).unwrap();
		assert_eq!(get_statuses(repo_path), (0, 1));

		// overwrite with next content
		{
			File::create(&file_path)
				.unwrap()
				.write_all(HUNK_B.as_bytes())
				.unwrap();
		}

		assert_eq!(get_statuses(repo_path), (1, 1));

		let res =
			get_diff(repo_path, "bar.txt", false, None).unwrap();

		assert_eq!(res.hunks.len(), 2);
	}

	#[test]
	fn test_diff_newfile_in_sub_dir_current_dir() {
		let file_path = Path::new("foo/foo.txt");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();

		let sub_path = root.join("foo/");

		fs::create_dir_all(&sub_path).unwrap();
		File::create(root.join(file_path))
			.unwrap()
			.write_all(b"test")
			.unwrap();

		let diff = get_diff(
			&sub_path.to_str().unwrap().into(),
			file_path.to_str().unwrap(),
			false,
			None,
		)
		.unwrap();

		assert_eq!(&*diff.hunks[0].lines[1].content, "test");
	}

	#[test]
	fn test_diff_delta_size() -> Result<()> {
		let file_path = Path::new("bar");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"\x00")?;

		stage_add_file(repo_path, file_path).unwrap();

		commit(repo_path, "commit").unwrap();

		File::create(root.join(file_path))?.write_all(b"\x00\x02")?;

		let diff = get_diff(
			repo_path,
			file_path.to_str().unwrap(),
			false,
			None,
		)
		.unwrap();

		dbg!(&diff);
		assert_eq!(diff.sizes, (1, 2));
		assert_eq!(diff.size_delta, 1);

		Ok(())
	}

	#[test]
	fn test_binary_diff_delta_size_untracked() -> Result<()> {
		let file_path = Path::new("bar");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"\x00\xc7")?;

		let diff = get_diff(
			repo_path,
			file_path.to_str().unwrap(),
			false,
			None,
		)
		.unwrap();

		dbg!(&diff);
		assert_eq!(diff.sizes, (0, 2));
		assert_eq!(diff.size_delta, 2);

		Ok(())
	}

	#[test]
	fn test_diff_delta_size_commit() -> Result<()> {
		let file_path = Path::new("bar");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"\x00")?;

		stage_add_file(repo_path, file_path).unwrap();

		commit(repo_path, "").unwrap();

		File::create(root.join(file_path))?.write_all(b"\x00\x02")?;

		stage_add_file(repo_path, file_path).unwrap();

		let id = commit(repo_path, "").unwrap();

		let diff =
			get_diff_commit(repo_path, id, String::new(), None)
				.unwrap();

		dbg!(&diff);
		assert_eq!(diff.sizes, (1, 2));
		assert_eq!(diff.size_delta, 1);

		Ok(())
	}
}
