## Branch Workflow

- Treat `master` as the original upstream branch.
- Treat `main` as the fork branch that carries local feature work, especially diff-related UI changes.
- Prefer keeping `main` as a linear stack of local commits on top of `upstream/master`.
- When updating `main` from upstream, prefer:

```sh
git fetch upstream
git switch main
git rebase upstream/master
cargo test
```

- Use merge instead of rebase only when preserving already-shared branch history is more important than linear history.
- If `main` has been rebased and must be pushed to a shared remote, use `git push --force-with-lease`, not plain force push.
- Keep feature/topic branches such as side-by-side diff, syntax highlighting, and diff cache changes available when practical; they make conflict resolution and regression isolation easier.
