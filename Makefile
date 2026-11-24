.PHONY: check fmt lint test bench release-bump release

check: fmt lint test

fmt:
	cargo fmt --all -- --check

lint:
	cargo clippy --all-targets --locked -- -D warnings

test:
	cargo test --all-targets --locked

bench:
	cargo bench --bench perf --locked

# Every PR bumps the version: `make release-bump` (patch), or BUMP=minor / BUMP=major.
release-bump:
	scripts/release.sh bump $(BUMP)

# After merging to main: tag origin/main's version and publish the GitHub release.
release:
	scripts/release.sh tag
