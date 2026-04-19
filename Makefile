# auditui Makefile — default target is release build.
# Why: debug builds have no test value here (see git history for discussion).

CARGO ?= cargo
BIN := target/release/auditui

.DEFAULT_GOAL := release

.PHONY: release run dry-run bench group-dump memory-dump md-dump clean deploy-xserver

release:
	$(CARGO) build --release

run: release
	./$(BIN)

dry-run: release
	./$(BIN) --dry-run

bench: release
	./$(BIN) --bench

group-dump: release
	./$(BIN) --group-dump

memory-dump: release
	./$(BIN) --memory-dump

md-dump: release
	@[ -n "$(FILE)" ] || { echo "usage: make md-dump FILE=<path>"; exit 1; }
	./$(BIN) --md-dump "$(FILE)"

deploy-xserver: release
	./deploy-xserver.sh

clean:
	$(CARGO) clean
