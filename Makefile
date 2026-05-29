.PHONY: start build

BINARY := ./target/release/cascade

$(BINARY):
	cargo build --release

start: $(BINARY)
	$(BINARY) start

build:
	cargo build
