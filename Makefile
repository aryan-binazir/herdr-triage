.PHONY: check fmt lint test

check: fmt lint test

fmt:
	cargo fmt --all -- --check

lint:
	cargo clippy --all-targets --locked -- -D warnings

test:
	cargo test --all-targets --locked
