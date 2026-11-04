CARGO ?= cargo
.PHONY: check fmt-check lint test
check: fmt-check lint test
fmt-check:
	$(CARGO) fmt --all --check
lint:
	$(CARGO) clippy --workspace --all-targets --all-features -- -Dwarnings
# Tests never see the developer's display; .cargo/config.toml points both D-Bus addresses
# at a socket that does not exist.
test:
	env -u WAYLAND_DISPLAY -u DISPLAY $(CARGO) test --workspace
