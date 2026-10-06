CARGO ?= cargo

.PHONY: all build lint format test check clean

all: build

build:
	$(CARGO) build --release --locked --bins

lint:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --all-targets --all-features --locked -- -D warnings
	bash -n tools/setup-zram.sh
	shellcheck tools/setup-zram.sh
	git diff --check

format:
	$(CARGO) fmt --all

test:
	$(CARGO) test --all-targets --all-features --locked

check: lint test build

clean:
	rm -rf target
