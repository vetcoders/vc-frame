# vc-frame (Zellij fork) — Canonical Build System
# 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI
#
# Usage:
#   make              — full build (plugins + binary)
#   make install      — build + install to ~/.cargo/bin
#   make test         — run all tests
#   make precheck     — fmt + clippy + typecheck  (gate before push)
#   make help         — show all targets
#
# Requirements:
#   - rustup-managed toolchain (recommended over distro/homebrew)
#   - wasm32-wasip1 target installed
#   - protobuf compiler (protoc)

.PHONY: all build plugins binary install run test test-server test-utils \
        test-client test-no-web check clippy precheck fmt clean doctor help

# ──────────────────────────────────────────────────────────
# Toolchain resolution.
#
# Priority order:
#   1. ~/.cargo/bin/{cargo,rustc}  (rustup proxy — reads rust-toolchain.toml)
#   2. Whatever is on PATH         (distro/homebrew/nix/CI)
#
# On macOS with Homebrew Rust installed alongside rustup,
# /opt/homebrew/bin/{cargo,rustc} ignores rust-toolchain.toml
# and lacks the wasm32-wasip1 sysroot. Prepending ~/.cargo/bin
# to PATH fixes both cargo AND the rustc it spawns internally.
#
# On Linux without homebrew, ~/.cargo/bin is typically already
# first in PATH (rustup installer sets it up). If rustup is
# not installed at all, we fall back to whatever cargo is on PATH.
#
# On Windows: use `cargo xtask` directly, not make.
# ──────────────────────────────────────────────────────────
CARGO_BIN_DIR := $(HOME)/.cargo/bin

# Prepend rustup bin dir to PATH if cargo there is actually executable.
# We use `test -x` instead of Make's `wildcard` because wildcard
# sees broken symlinks (cargo -> rustup when rustup is uninstalled)
# as existing files.
RUSTUP_CARGO_OK := $(shell test -x $(CARGO_BIN_DIR)/cargo && echo yes)
ifeq ($(RUSTUP_CARGO_OK),yes)
  export PATH := $(CARGO_BIN_DIR):$(PATH)
  CARGO := $(CARGO_BIN_DIR)/cargo
else
  CARGO := $(shell command -v cargo 2>/dev/null || echo cargo)
endif

# Stack size for tests that build deep plugin trees
export RUST_MIN_STACK := 8388608
export CARGO_TERM_COLOR := always

# ──────────────────────────────────────────────────────────
# Top-level targets
# ──────────────────────────────────────────────────────────

## Build everything: WASM plugins first, then the zellij binary
all: build

build: doctor-quiet
	$(CARGO) xtask build

## Build only WASM plugins (no host binary)
plugins: doctor-quiet
	$(CARGO) xtask build --plugins-only

## Build only the host binary (assumes plugins are already built)
binary: doctor-quiet
	$(CARGO) xtask build --no-plugins

## Build in release mode
release: doctor-quiet
	$(CARGO) xtask build --release

## Build + install the zellij binary (with bundled plugins) to $DEST
## Usage: make install  OR  make install DEST=/usr/local/bin
DEST ?=
install: doctor-quiet
	$(CARGO) xtask install $(DEST)

## Run the locally built zellij
run: doctor-quiet
	$(CARGO) xtask run

# ──────────────────────────────────────────────────────────
# Test targets
# ──────────────────────────────────────────────────────────

## Full test suite (all workspace crates)
test: doctor-quiet
	$(CARGO) xtask test

## Test only zellij-server
test-server:
	$(CARGO) test -p zellij-server

## Test only zellij-utils
test-utils:
	$(CARGO) test -p zellij-utils

## Test only zellij-client
test-client:
	$(CARGO) test -p zellij-client

## Test without web support
test-no-web:
	$(CARGO) xtask test --no-web

# ──────────────────────────────────────────────────────────
# Quality gates
# ──────────────────────────────────────────────────────────

## Quick typecheck (no output binary)
check:
	$(CARGO) check --workspace

## Clippy: zero warnings, with project-agreed allowances
clippy:
	$(CARGO) clippy --workspace --all-targets -- \
		-D warnings \
		-A clippy::too_many_arguments \
		-A clippy::type_complexity \
		-A clippy::borrowed_box \
		-A clippy::ptr_arg

## Format all code
fmt:
	$(CARGO) xtask format

## Format check (dry-run)
fmt-check:
	$(CARGO) xtask format --check

## Full pre-push gate: format → clippy → typecheck
## This is what CI runs. If this passes locally, CI will pass.
precheck:
	@echo "╔══════════════════════════════════════╗"
	@echo "║  vc-frame precheck                   ║"
	@echo "╚══════════════════════════════════════╝"
	@echo ""
	@echo "→ [1/3] Formatting..."
	@$(CARGO) xtask format --check || { echo "✗ Run 'make fmt' to fix"; exit 1; }
	@echo "✓ Format OK"
	@echo ""
	@echo "→ [2/3] Clippy..."
	@$(CARGO) clippy --workspace --all-targets -- \
		-D warnings \
		-A clippy::too_many_arguments \
		-A clippy::type_complexity \
		-A clippy::borrowed_box \
		-A clippy::ptr_arg
	@echo "✓ Clippy OK"
	@echo ""
	@echo "→ [3/3] Typecheck..."
	@$(CARGO) check --workspace
	@echo "✓ Check OK"
	@echo ""
	@echo "══════════════════════════════════════"
	@echo "  ✓ All precheck gates passed"
	@echo "══════════════════════════════════════"

## Full validation: precheck + test suite
ci: precheck test
	@echo ""
	@echo "══════════════════════════════════════"
	@echo "  ✓ CI-equivalent gates passed"
	@echo "══════════════════════════════════════"

# ──────────────────────────────────────────────────────────
# Housekeeping
# ──────────────────────────────────────────────────────────

## Clean all build artifacts
clean:
	$(CARGO) clean

## Environment doctor — loud version, shows what's configured
doctor:
	@echo "── vc-frame doctor ──"
	@echo "cargo:    $$($(CARGO) --version) ($$(command -v $(CARGO)))"
	@echo "rustc:    $$(rustc --version) ($$(command -v rustc))"
	@echo "toolchain: $$(rustup show active-toolchain 2>/dev/null || echo 'rustup not available')"
	@echo ""
	@echo "WASM target:"
	@if command -v rustup >/dev/null 2>&1; then \
		rustup target list --installed 2>/dev/null | grep -q wasm32-wasip1 \
			&& echo "  ✓ wasm32-wasip1 installed" \
			|| echo "  ✗ wasm32-wasip1 MISSING — run: rustup target add wasm32-wasip1"; \
	else \
		echo "  ? rustup not found — cannot verify wasm target"; \
	fi
	@echo ""
	@command -v protoc >/dev/null 2>&1 \
		&& echo "protoc:   $$(protoc --version)" \
		|| echo "protoc:   ✗ NOT FOUND (required for build)"
	@echo ""
	@echo "── OK ──"

## Silent doctor — prerequisite for build targets, fails fast on missing deps
doctor-quiet:
	@command -v $(CARGO) >/dev/null 2>&1 \
		|| { echo "ERROR: cargo not found at '$(CARGO)'. Install rustup: https://rustup.rs"; exit 1; }
	@if command -v rustup >/dev/null 2>&1; then \
		rustup target list --installed 2>/dev/null | grep -q wasm32-wasip1 \
			|| { echo "ERROR: wasm32-wasip1 target missing. Run: rustup target add wasm32-wasip1"; exit 1; }; \
	fi

# ──────────────────────────────────────────────────────────
# Help
# ──────────────────────────────────────────────────────────

help:
	@echo "vc-frame Build System"
	@echo ""
	@echo "Build:"
	@echo "  make              Build plugins + binary (default)"
	@echo "  make plugins      Build only WASM plugins"
	@echo "  make binary       Build only host binary (plugins must exist)"
	@echo "  make release      Build everything in release mode"
	@echo "  make install      Build + install to ~/.cargo/bin (or DEST=path)"
	@echo "  make run          Run the locally built binary"
	@echo ""
	@echo "Test:"
	@echo "  make test         Full test suite"
	@echo "  make test-server  Test zellij-server only"
	@echo "  make test-utils   Test zellij-utils only"
	@echo "  make test-client  Test zellij-client only"
	@echo "  make test-no-web  Test without web support"
	@echo ""
	@echo "Quality:"
	@echo "  make precheck     fmt + clippy + typecheck (pre-push gate)"
	@echo "  make ci           precheck + full test suite"
	@echo "  make clippy       Clippy only"
	@echo "  make fmt          Format code"
	@echo "  make fmt-check    Check formatting (dry-run)"
	@echo "  make check        Quick typecheck"
	@echo ""
	@echo "Other:"
	@echo "  make doctor       Show environment info"
	@echo "  make clean        Clean build artifacts"
	@echo "  make help         This message"
