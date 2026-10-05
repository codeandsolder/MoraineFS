CC ?= gcc
UV ?= uv
PKG_CONFIG ?= pkg-config

CPPFLAGS += -Isrc
CFLAGS += -std=gnu11 -O2 -g3 \
	-Wall -Wextra -Wshadow -Wformat=2 -Wundef -Wcast-qual -Wwrite-strings \
	-Wstrict-prototypes -Wmissing-prototypes -Wconversion -Wsign-conversion \
	-Wduplicated-cond -Wduplicated-branches -Wlogical-op -Wnull-dereference -Werror
FUSE_CFLAGS := $(shell $(PKG_CONFIG) fuse3 --cflags)
FUSE_LIBS := $(shell $(PKG_CONFIG) fuse3 --libs)

.PHONY: all build lint format test check clean

all: build

build: build/morainefs

build/morainefs: src/morainefs.c src/passthrough_helpers.h
	@mkdir -p build
	$(CC) $(CPPFLAGS) $(CFLAGS) $(FUSE_CFLAGS) src/morainefs.c $(FUSE_LIBS) -lpthread -o $@

lint:
	$(UV) run ruff check tools tests
	$(UV) run ruff format --check tools tests
	bash -n tools/setup-zram.sh
	git diff --check

format:
	$(UV) run ruff check --fix tools tests
	$(UV) run ruff format tools tests

test:
	$(UV) run python tests/test_checkpoint_recovery.py
	$(UV) run python tests/test_settle_scheduler.py

check: lint test build

clean:
	rm -rf build
