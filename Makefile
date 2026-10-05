CC ?= gcc
CARGO ?= cargo
PKG_CONFIG ?= pkg-config
MIN_LIBFUSE_VERSION ?= 3.17.2

CPPFLAGS += -Isrc
CFLAGS += -std=gnu11 -O2 -g3 \
	-Wall -Wextra -Wshadow -Wformat=2 -Wundef -Wcast-qual -Wwrite-strings \
	-Wstrict-prototypes -Wmissing-prototypes -Wconversion -Wsign-conversion \
	-Wduplicated-cond -Wduplicated-branches -Wlogical-op -Wnull-dereference -Werror
FUSE_CFLAGS := $(shell $(PKG_CONFIG) fuse3 --cflags)
FUSE_LIBS := $(shell $(PKG_CONFIG) fuse3 --libs)

.PHONY: all build rust-build check-libfuse lint format test check clean

all: build

build: check-libfuse build/morainefs rust-build

rust-build:
	$(CARGO) build --release --locked --bins

check-libfuse:
	@$(PKG_CONFIG) --atleast-version=$(MIN_LIBFUSE_VERSION) fuse3 || \
		{ echo "MoraineFS requires libfuse >= $(MIN_LIBFUSE_VERSION)" >&2; exit 1; }

build/morainefs: src/morainefs.c src/passthrough_helpers.h
	@mkdir -p build
	$(CC) $(CPPFLAGS) $(CFLAGS) $(FUSE_CFLAGS) src/morainefs.c $(FUSE_LIBS) -lpthread -o $@

lint:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --all-targets --all-features --locked -- -D warnings
	bash -n tools/setup-zram.sh
	git diff --check

format:
	$(CARGO) fmt --all

test:
	$(CARGO) test --all-targets --all-features --locked

check: lint test build

clean:
	rm -rf build target
