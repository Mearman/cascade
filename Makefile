.PHONY: start build dev release

BINARY := ./target/release/cascade

$(BINARY):
	cargo build --release

release: $(BINARY)

start: $(BINARY)
	$(BINARY) start

build:
	cargo build

dev:
	cargo watch -x "run -- start"
