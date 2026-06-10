# vc-frame (Zellij fork) — Canonical Build System
# 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI
#
# Usage:
#   make              — full build (plugins + binary)
#   make install      — build + install to ~/.cargo/bin + ~/.local/bin alias
#   make test         — run all tests
#   make precheck     — fmt + clippy + typecheck  (gate before push)
#   make help         — show all targets
#
# Requirements:
#   - rustup-managed toolchain (recommended over distro/homebrew)
#   - wasm32-wasip1 target installed
#   - protobuf compiler (protoc)

.PHONY: all build plugins binary install run test test-server test-utils \
        test-client test-no-web check clippy precheck fmt clean doctor \
        doctor-quiet doctor-install-quiet help

# ──────────────────────────────────────────────────────────
# Toolchain resolution.
#
# Priority order:
#   1. ~/.cargo/bin/{cargo,rustc}  (rustup proxy — reads rust-toolchain.toml)
#   2. Homebrew rustup keg proxies (when rustup is installed keg-only)
#   3. Whatever is on PATH         (distro/homebrew/nix/CI)
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
HOMEBREW_RUSTUP_BIN_DIR := /opt/homebrew/opt/rustup/bin

# Prepend rustup bin dir to PATH if cargo there is actually executable.
# We use `test -x` instead of Make's `wildcard` because wildcard
# sees broken symlinks (cargo -> rustup when rustup is uninstalled)
# as existing files.
RUSTUP_CARGO_OK := $(shell test -x $(CARGO_BIN_DIR)/cargo && echo yes)
HOMEBREW_RUSTUP_CARGO_OK := $(shell test -x $(HOMEBREW_RUSTUP_BIN_DIR)/cargo && echo yes)
ifeq ($(RUSTUP_CARGO_OK),yes)
  export PATH := $(CARGO_BIN_DIR):$(PATH)
  CARGO := $(CARGO_BIN_DIR)/cargo
else ifeq ($(HOMEBREW_RUSTUP_CARGO_OK),yes)
  export PATH := $(HOMEBREW_RUSTUP_BIN_DIR):$(CARGO_BIN_DIR):$(PATH)
  CARGO := $(HOMEBREW_RUSTUP_BIN_DIR)/cargo
else
  CARGO := $(shell command -v cargo 2>/dev/null || echo cargo)
endif

# Stack size for tests that build deep plugin trees
export RUST_MIN_STACK := 8388608
export CARGO_TERM_COLOR := always

# Help colors
C_CYAN   := \033[36m
C_GREEN  := \033[32m
C_YELLOW := \033[33m
C_RESET  := \033[0m

# ──────────────────────────────────────────────────────────
# Top-level targets
# ──────────────────────────────────────────────────────────

## Build everything: WASM plugins first, then the vc-frame binary
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

## Build + install the vc-frame binary (with bundled plugins), then expose aliases on ~/.local/bin
## Usage: make install  OR  make install DEST=/usr/local/bin/vc-frame
DEST ?= $(CARGO_BIN_DIR)/vc-frame
LOCAL_BIN_DIR ?= $(HOME)/.local/bin
LOCAL_VC_FRAME_ALIAS ?= $(LOCAL_BIN_DIR)/vc-frame
LOCAL_ZELLIJ_ALIAS ?= $(LOCAL_BIN_DIR)/zellij
install: doctor-quiet doctor-install-quiet
	$(CARGO) xtask install $(DEST)
	@mkdir -p "$(LOCAL_BIN_DIR)"
	@installed="$(DEST)"; \
	if [ -d "$$installed" ]; then installed="$$installed/vc-frame"; fi; \
	ln -sfn "$$installed" "$(LOCAL_VC_FRAME_ALIAS)"; \
	ln -sfn "$$installed" "$(LOCAL_ZELLIJ_ALIAS)"; \
	echo "✓ Installed vc-frame: $$installed"; \
	echo "✓ Linked $(LOCAL_VC_FRAME_ALIAS) -> $$installed"; \
	echo "✓ Linked $(LOCAL_ZELLIJ_ALIAS) -> $$installed"

## Run the locally built vc-frame
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
	@command -v mandown >/dev/null 2>&1 \
		&& echo "mandown:  $$(command -v mandown)" \
		|| echo "mandown:  ✗ NOT FOUND (required for install; run: cargo install mandown)"
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

doctor-install-quiet:
	@command -v mandown >/dev/null 2>&1 \
		|| { echo "ERROR: mandown missing. Run: cargo install mandown"; exit 1; }

# ──────────────────────────────────────────────────────────
# Help
# ──────────────────────────────────────────────────────────

help:
	@printf "\n$(C_CYAN)vc-frame$(C_RESET) — Zellij fork canonical build system\n"
	@printf "$(C_CYAN)────────────────────────────────────────────────────────────────────────$(C_RESET)\n\n"
	@printf "  $(C_YELLOW)BUILD$(C_RESET)\n"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "build" "Build plugins + binary (default)"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "plugins" "Build only WASM plugins"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "binary" "Build only host binary (plugins must exist)"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "release" "Build everything in release mode"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "install" "Release build + install to ~/.cargo/bin + link vc-frame and zellij aliases"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "run" "Run the locally built vc-frame"
	@printf "\n  $(C_YELLOW)QUALITY GATES$(C_RESET)\n"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "precheck" "Format check + clippy -D warnings + workspace typecheck"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "ci" "Precheck + full test suite"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "clippy" "Clippy only"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "fmt" "Format code"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "fmt-check" "Check formatting without modifying files"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "check" "Quick workspace typecheck"
	@printf "\n  $(C_YELLOW)TEST$(C_RESET)\n"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "test" "Full test suite"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "test-server" "Test zellij-server only"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "test-utils" "Test zellij-utils only"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "test-client" "Test zellij-client only"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "test-no-web" "Test without web support"
	@printf "\n  $(C_YELLOW)INSPECTION / HOUSEKEEPING$(C_RESET)\n"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "doctor" "Show environment info"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "clean" "Clean build artifacts"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "help" "Show this help"
	@printf "\n  $(C_CYAN)Quick start:$(C_RESET)\n"
	@printf "    make precheck       # format + clippy + typecheck\n"
	@printf "    make plugins        # refresh WASM plugin artifacts for local gates\n"
	@printf "    make install        # canonical release install + ~/.local/bin alias\n"
	@printf "    make run            # run local debug vc-frame\n\n"
