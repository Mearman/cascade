.PHONY: start stop build dev debug release

BINARY := ./target/release/cascade

$(BINARY):
	cargo build --release

release: $(BINARY)

start: $(BINARY)
	exec $(BINARY) start

stop: $(BINARY)
	$(BINARY) stop

build:
	cargo build

dev:
	exec env RUST_LOG="$${RUST_LOG:-debug}" cargo watch -x "run -- start"

debug:
	exec env RUST_LOG="$${RUST_LOG:-debug}" $(BINARY) start
