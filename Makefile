build:
	cargo build --release

run: build
	./target/release/cwo

install: build
	cp target/release/cwo ~/.local/bin/cwo
