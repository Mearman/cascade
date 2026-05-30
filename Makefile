.PHONY: start stop build dev debug release

BINARY := ./target/release/cascade

# Always defer to cargo for build decisions — cargo's incremental
# compilation makes a no-op build cheap, and a file-existence check
# would silently run a stale binary after source edits.
release:
	cargo build --release

start: release
	exec $(BINARY) start

stop:
	$(BINARY) stop

build:
	cargo build

dev:
	exec env RUST_LOG="$${RUST_LOG:-debug}" cargo watch -x "run -- start"

debug: release
	exec env RUST_LOG="$${RUST_LOG:-debug}" $(BINARY) start
