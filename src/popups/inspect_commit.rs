use crate::components::{
	command_pump, event_pump, visibility_blocking, CommandBlocking,
	CommandInfo, CommitDetailsComponent, Component, DiffComponent,
	DrawableComponent, EventState,
};
use crate::{
	accessors,
	app::Environment,
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	queue::{InternalEvent, Queue, StackablePopupOpen},
	strings, AsyncNotification,
};
use anyhow::Result;
use asyncgit::{
	sync::{CommitId, CommitTags},
	AsyncDiff, AsyncGitNotification, DiffParams, DiffType,
};
use crossterm::event::Event;
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	widgets::Clear,
	Frame,
};

use super::FileTreeOpen;

#[derive(Clone, Debug)]
pub struct InspectCommitOpen {
	pub commit_id: CommitId,
	/// in case we wanna compare
	pub compare_id: Option<CommitId>,
	pub tags: Option<CommitTags>,
}

impl InspectCommitOpen {
	pub const fn new(commit_id: CommitId) -> Self {
		Self {
			commit_id,
			compare_id: None,
			tags: None,
		}
	}

	pub const fn new_with_tags(
		commit_id: CommitId,
		tags: Option<CommitTags>,
	) -> Self {
		Self {
			commit_id,
			compare_id: None,
			tags,
		}
	}
}

pub struct InspectCommitPopup {
	queue: Queue,
	open_request: Option<InspectCommitOpen>,
	diff: DiffComponent,
	details: CommitDetailsComponent,
	git_diff: AsyncDiff,
	visible: bool,
	key_config: SharedKeyConfig,
	options: SharedOptions,
}

impl DrawableComponent for InspectCommitPopup {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if self.is_visible() {
			let percentages = if self.diff.focused() {
				(0, 100)
			} else {
				(50, 50)
			};

			let chunks = Layout::default()
				.direction(Direction::Horizontal)
				.constraints(
					[
						Constraint::Percentage(percentages.0),
						Constraint::Percentage(percentages.1),
					]
					.as_ref(),
				)
				.split(rect);

			f.render_widget(Clear, rect);

			self.details.draw(f, chunks[0])?;
			if self.diff.focused() {
				self.diff.draw(f, chunks[1])?;
			} else {
				self.diff.draw_unified(f, chunks[1])?;
			}
		}

		Ok(())
	}
}

impl Component for InspectCommitPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.is_visible() || force_all {
			command_pump(
				out,
				force_all,
				self.components().as_slice(),
			);

			out.push(
				CommandInfo::new(
					strings::commands::close_popup(&self.key_config),
					true,
					true,
				)
				.order(1),
			);

			out.push(CommandInfo::new(
				if self.details.files().selection_file().is_none() {
					strings::commands::directory_diff_focus(
						&self.key_config,
					)
				} else {
					strings::commands::diff_focus_right(
						&self.key_config,
					)
				},
				self.can_focus_diff(),
				!self.diff.focused() || force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::close_popup(&self.key_config),
				true,
				self.diff.focused() || force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::inspect_file_tree(
					&self.key_config,
				),
				true,
				true,
			));
		}

		visibility_blocking(self)
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.is_visible() {
			if event_pump(ev, self.components_mut().as_mut_slice())?
				.is_consumed()
			{
				if !self.details.is_visible() {
					self.hide_stacked(true);
				}

				return Ok(EventState::Consumed);
			}

			if let Event::Key(e) = ev {
				if key_match(e, self.key_config.keys.exit_popup) {
					if self.diff.focused() {
						self.details.focus(true);
						self.diff.focus(false);
					} else {
						self.hide_stacked(false);
					}
				} else if (key_match(
					e,
					self.key_config.keys.move_right,
				) || (key_match(
					e,
					self.key_config.keys.enter,
				) && self
					.details
					.files()
					.selection_file()
					.is_none())) && self.can_focus_diff()
				{
					self.details.focus(false);
					self.diff.focus(true);
				} else if key_match(e, self.key_config.keys.move_left)
				{
					self.hide_stacked(false);
				} else if key_match(
					e,
					self.key_config.keys.open_file_tree,
				) {
					if let Some(commit_id) = self
						.open_request
						.as_ref()
						.map(|open_commit| open_commit.commit_id)
					{
						self.hide_stacked(true);
						self.queue.push(InternalEvent::OpenPopup(
							StackablePopupOpen::FileTree(
								FileTreeOpen::new(commit_id),
							),
						));
						return Ok(EventState::Consumed);
					}
					return Ok(EventState::NotConsumed);
				}

				return Ok(EventState::Consumed);
			}
		}

		Ok(EventState::NotConsumed)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}
	fn hide(&mut self) {
		self.visible = false;
	}
	fn show(&mut self) -> Result<()> {
		self.visible = true;
		self.details.show()?;
		self.details.focus(true);
		self.diff.focus(false);
		self.update()?;
		Ok(())
	}
}

impl InspectCommitPopup {
	accessors!(self, [diff, details]);

	///
	pub fn new(env: &Environment) -> Self {
		Self {
			queue: env.queue.clone(),
			details: CommitDetailsComponent::new(env),
			diff: DiffComponent::new(env, true),
			open_request: None,
			git_diff: AsyncDiff::new(
				env.repo.borrow().clone(),
				&env.sender_git,
			),
			visible: false,
			key_config: env.key_config.clone(),
			options: env.options.clone(),
		}
	}

	///
	pub fn open(&mut self, open: InspectCommitOpen) -> Result<()> {
		self.open_request = Some(open);
		self.show()?;

		Ok(())
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.git_diff.is_pending()
			|| self.details.any_work_pending()
			|| self.diff.any_work_pending()
	}

	pub fn update_async(&mut self, ev: AsyncNotification) {
		self.diff.update_async(ev);
	}

	///
	pub fn update_git(
		&mut self,
		ev: AsyncGitNotification,
	) -> Result<()> {
		if self.is_visible() {
			if ev == AsyncGitNotification::CommitFiles {
				self.update()?;
			} else if ev == AsyncGitNotification::Diff {
				self.update_diff()?;
			}
		}

		Ok(())
	}

	/// called when any tree component changed selection
	pub fn update_diff(&mut self) -> Result<()> {
		if self.is_visible() {
			if let Some(request) = &self.open_request {
				if let Some(path) =
					self.details.files().selection_diff_path()
				{
					let diff_params = DiffParams {
						path: path.clone(),
						diff_type: DiffType::Commit(
							request.commit_id,
						),
						options: self.options.borrow().diff_options(),
					};

					if let Some((params, last)) =
						self.git_diff.last()?
					{
						if params == diff_params {
							self.diff.update(
								path,
								false,
								last,
								diff_params,
							);
							return Ok(());
						}
					}

					if let Some(diff) =
						self.git_diff.request(diff_params.clone())?
					{
						self.diff.update(
							path,
							false,
							diff,
							diff_params,
						);
					} else {
						self.diff.clear(true);
					}
					return Ok(());
				}
			}

			self.diff.clear(false);
		}

		Ok(())
	}

	fn update(&mut self) -> Result<()> {
		if let Some(request) = &self.open_request {
			self.details.set_commits(
				Some(request.commit_id.into()),
				request.tags.as_ref(),
			)?;
			self.update_diff()?;
		}

		Ok(())
	}

	fn can_focus_diff(&self) -> bool {
		self.details.files().selection_diff_path().is_some()
	}

	fn hide_stacked(&mut self, stack: bool) {
		self.hide();

		if stack {
			if let Some(open_request) = self.open_request.take() {
				self.queue.push(InternalEvent::PopupStackPush(
					StackablePopupOpen::InspectCommit(open_request),
				));
			}
		} else {
			self.queue.push(InternalEvent::PopupStackPop);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::popups::CompareCommitsPopup;
	use asyncgit::sync::{commit, stage_add_file, RepoPath};
	use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
	use std::{
		fs,
		path::Path,
		time::{Duration, Instant},
	};

	#[test]
	fn directory_selection_renders_commit_and_comparison_diffs() {
		let (temp, repo) = git2_testing::repo_init();
		let repo_path: RepoPath =
			temp.path().to_str().unwrap().into();
		let old =
			CommitId::new(repo.head().unwrap().target().unwrap());
		fs::create_dir_all(temp.path().join("dir/nested")).unwrap();
		for (path, content) in [
			("dir/first.txt", "first change\n"),
			("dir/nested/second.txt", "second change\n"),
		] {
			fs::write(temp.path().join(path), content).unwrap();
			stage_add_file(&repo_path, Path::new(path)).unwrap();
		}
		let new = commit(&repo_path, "changes").unwrap();
		let mut env = Environment::test_env();
		*env.repo.borrow_mut() = repo_path;
		let (sender, receiver) = crossbeam_channel::unbounded();
		env.sender_git = sender;
		let (sender_app, _receiver_app) =
			crossbeam_channel::unbounded();
		env.sender_app = sender_app;
		let mut popup = InspectCommitPopup::new(&env);
		popup.open(InspectCommitOpen::new(new)).unwrap();
		wait_inspect(&mut popup, &receiver);
		assert_eq!(popup.diff.current().0, "dir/");
		assert!(popup.can_focus_diff());
		assert_directory_render(&popup);

		// Move from the directory to a file and back using real key events.
		popup.event(&key(KeyCode::Down)).unwrap();
		popup.update_diff().unwrap();
		wait_inspect(&mut popup, &receiver);
		assert_eq!(popup.diff.current().0, "dir/first.txt");
		popup.event(&key(KeyCode::Up)).unwrap();
		popup.update_diff().unwrap();
		wait_inspect(&mut popup, &receiver);
		assert_directory_render(&popup);
		popup.event(&key(KeyCode::Enter)).unwrap();
		assert!(popup.diff.focused());
		assert_directory_render(&popup);
		popup.event(&key(KeyCode::Esc)).unwrap();
		assert!(!popup.diff.focused());
		assert_directory_render(&popup);

		let mut comparison = CompareCommitsPopup::new(&env);
		comparison
			.open(InspectCommitOpen {
				commit_id: new,
				compare_id: Some(old),
				tags: None,
			})
			.unwrap();
		let deadline = Instant::now() + Duration::from_secs(5);
		comparison
			.update_git(AsyncGitNotification::CommitFiles)
			.unwrap();
		while comparison.any_work_pending() {
			assert!(Instant::now() < deadline);
			if let Ok(ev) =
				receiver.recv_timeout(Duration::from_millis(10))
			{
				comparison.update_git(ev).unwrap();
			}
		}
		comparison
			.update_git(AsyncGitNotification::CommitFiles)
			.unwrap();
		comparison.update_diff().unwrap();
		assert_directory_render(&comparison);
		comparison.event(&key(KeyCode::Enter)).unwrap();
		assert_directory_render(&comparison);
	}

	fn wait_inspect(
		popup: &mut InspectCommitPopup,
		receiver: &crossbeam_channel::Receiver<AsyncGitNotification>,
	) {
		let deadline = Instant::now() + Duration::from_secs(5);
		popup.update_git(AsyncGitNotification::CommitFiles).unwrap();
		while popup.any_work_pending() {
			assert!(Instant::now() < deadline);
			if let Ok(ev) =
				receiver.recv_timeout(Duration::from_millis(10))
			{
				popup.update_git(ev).unwrap();
			}
		}
		popup.update_git(AsyncGitNotification::CommitFiles).unwrap();
		popup.update_diff().unwrap();
	}

	fn key(code: KeyCode) -> Event {
		Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
	}

	fn assert_directory_render(popup: &impl DrawableComponent) {
		let mut terminal = ratatui::Terminal::new(
			ratatui::backend::TestBackend::new(160, 40),
		)
		.unwrap();
		terminal
			.draw(|frame| popup.draw(frame, frame.area()).unwrap())
			.unwrap();
		let rendered = terminal.backend().to_string();
		assert!(rendered.contains("Diff: dir/"), "{rendered}");
		assert!(rendered.contains("+2 -0"), "{rendered}");
		for expected in [
			"dir/first.txt",
			"dir/nested/second.txt",
			"first change",
			"second change",
		] {
			assert!(
				rendered.contains(expected),
				"missing {expected}: {rendered}"
			);
		}
	}
}
