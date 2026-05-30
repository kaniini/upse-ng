# SPDX-License-Identifier: LGPL-2.1-or-later

.PHONY: check fmt clippy test doc

check: fmt clippy test doc

fmt:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

test:
	cargo test --workspace --all-targets --all-features --locked

doc:
	RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked
