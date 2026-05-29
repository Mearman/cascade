.PHONY: start stop build dev release

BINARY := ./target/release/cascade

$(BINARY):
	cargo build --release

release: $(BINARY)

start: $(BINARY)
	$(BINARY) start

stop: $(BINARY)
	$(BINARY) stop

build:
	cargo build

dev:
	RUST_LOG="$${RUST_LOG:-debug}" cargo watch -x "run -- start"
