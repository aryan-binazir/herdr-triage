# Agent instructions

## Releases

`main` is the release line, and every PR ships a version bump.

- Every PR runs `make release-bump` before it is opened. It bumps the patch version from the last tag. Use `make release-bump BUMP=minor` or `BUMP=major` only when Ar asks for it. Commit the changed `Cargo.toml`, `herdr-plugin.toml` and `Cargo.lock` with the PR.
- After a PR merges to `main`, run `make release`. It tags `origin/main` with the version from `Cargo.toml` and publishes a GitHub release marked Latest, with notes listing the PRs since the last tag. It does nothing when `main`'s head is already tagged, and refuses when the version isn't above the last tag.
