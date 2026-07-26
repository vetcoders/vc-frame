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

.PHONY: all build plugins plugins-assets plugins-parity plugins-parity-double \
        plugins-parity-self-test binary install run test test-server test-utils \
        test-client test-no-web check clippy precheck semgrep fmt clean doctor \
        doctor-quiet doctor-install-quiet help release-guard \
        release-guard-self-test package install-test release-contract-test \
        triage-runtime-e2e-static triage-runtime-e2e \
        version version-show version-check version-bump version-patch bump-patch \
        changelog-close release-notes release-plan release-prepare release-check \
        release-preflight release-tag release-push

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

# --- Release versioning (aicx-shaped) ----------------------------------------
# Single source of truth: [workspace.package].version in Cargo.toml
# TYPE= is an alias for VERSION= on version / version-bump / release-prepare.
PACKAGE_NAME := vc-frame
WS_VERSION := $(shell python3 -c 'import pathlib,tomllib; d=tomllib.loads(pathlib.Path("Cargo.toml").read_text()); print(d["workspace"]["package"]["version"])' 2>/dev/null)
TAG := v$(WS_VERSION)
PYTHON := $(shell command -v python3.14 2>/dev/null || command -v python3.13 2>/dev/null || command -v python3.12 2>/dev/null || command -v python3.11 2>/dev/null || command -v python3)
TRIAGE_RUNTIME_E2E_BINARY ?= target/debug/vc-frame
TRIAGE_RUNTIME_E2E_PROFILE ?= debug
TRIAGE_RUNTIME_E2E_ARTIFACT_ROOT ?= /tmp/vc-frame-triage-runtime-e2e
PYTHON_CACHE_ROOT ?= $(CURDIR)/target/python-cache
RELEASE_KEYS_DIR ?= $(HOME)/.keys

# ──────────────────────────────────────────────────────────
# Top-level targets
# ──────────────────────────────────────────────────────────

## Build everything: WASM plugins first, then the vc-frame binary
all: build

build: doctor-quiet
	$(CARGO) xtask build

## Build only WASM plugins (no host binary)
## Without --release this is a compile check only — it does NOT refresh
## zellij-utils/assets/plugins/*.wasm. Use plugins-assets for the product surface.
plugins: doctor-quiet
	$(CARGO) xtask build --plugins-only

## Canonical product-surface producer: release-build plugins and copy into assets/
plugins-assets: doctor-quiet
	$(CARGO) xtask build --release --plugins-only
	@./scripts/plugins-parity.zsh write-manifest

## CI-fast parity: on-disk assets must match committed SHA256SUMS
plugins-parity:
	@./scripts/plugins-parity.zsh check

## Two isolated consecutive rebuilds must produce identical hashes (W0-C gate)
plugins-parity-double: doctor-quiet
	@./scripts/plugins-parity.zsh double-rebuild

## Positive + deliberate perturbation negative + restore
plugins-parity-self-test:
	@./scripts/plugins-parity.zsh self-test

## Build only the host binary (assumes plugins are already built)
binary: doctor-quiet
	$(CARGO) xtask build --no-plugins

## Build in release mode
release: doctor-quiet
	$(CARGO) xtask build --release

## Build + install the vc-frame binary (with bundled plugins), then expose the vc-frame alias on ~/.local/bin
## vc-frame replaces zellij 100% in the runtime — no `zellij` alias is created.
## Usage: make install  OR  make install DEST=/usr/local/bin/vc-frame
DEST ?= $(CARGO_BIN_DIR)/vc-frame
LOCAL_BIN_DIR ?= $(HOME)/.local/bin
LOCAL_VC_FRAME_ALIAS ?= $(LOCAL_BIN_DIR)/vc-frame
install: doctor-quiet doctor-install-quiet
	$(CARGO) xtask install $(DEST)
	@mkdir -p "$(LOCAL_BIN_DIR)"
	@installed="$(DEST)"; \
	if [ -d "$$installed" ]; then installed="$$installed/vc-frame"; fi; \
	ln -sfn "$$installed" "$(LOCAL_VC_FRAME_ALIAS)"; \
	echo "✓ Installed vc-frame: $$installed"; \
	echo "✓ Linked $(LOCAL_VC_FRAME_ALIAS) -> $$installed"

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

## Quick typecheck (builds bundled WASM plugins first)
check: plugins
	$(CARGO) check --workspace

## Clippy: zero warnings, with project-agreed allowances; builds bundled WASM plugins first
clippy: plugins
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

## Full pre-push gate: format → plugin build → clippy → typecheck
## This is what CI runs. If this passes locally, CI will pass.
precheck:
	@echo "╔══════════════════════════════════════╗"
	@echo "║  vc-frame precheck                   ║"
	@echo "╚══════════════════════════════════════╝"
	@echo ""
	@echo "→ [1/4] Formatting..."
	@$(CARGO) xtask format --check || { echo "✗ Run 'make fmt' to fix"; exit 1; }
	@echo "✓ Format OK"
	@echo ""
	@echo "→ [2/4] WASM plugins..."
	@$(CARGO) xtask build --plugins-only
	@echo "✓ Plugins OK"
	@echo ""
	@echo "→ [3/4] Clippy..."
	@$(CARGO) clippy --workspace --all-targets -- \
		-D warnings \
		-A clippy::too_many_arguments \
		-A clippy::type_complexity \
		-A clippy::borrowed_box \
		-A clippy::ptr_arg
	@echo "✓ Clippy OK"
	@echo ""
	@echo "→ [4/4] Typecheck..."
	@$(CARGO) check --workspace
	@echo "✓ Check OK"
	@echo ""
	@echo "══════════════════════════════════════"
	@echo "  ✓ All precheck gates passed"
	@echo "══════════════════════════════════════"

## Static/unit gate for the isolated triage process-boundary harness
triage-runtime-e2e-static:
	@PYTHONPYCACHEPREFIX="$(PYTHON_CACHE_ROOT)" $(PYTHON) -m py_compile \
		scripts/triage-runtime-e2e.py tools/triage_runtime_e2e_test.py
	@PYTHONDONTWRITEBYTECODE=1 $(PYTHON) tools/triage_runtime_e2e_test.py

## Real isolated process-boundary regression against the exact clean checkout build
triage-runtime-e2e:
	@test -x "$(TRIAGE_RUNTIME_E2E_BINARY)" \
		|| { echo "ERROR: missing executable $(TRIAGE_RUNTIME_E2E_BINARY); run 'cargo build --bin vc-frame' first"; exit 1; }
	@VC_FRAME_E2E_ARTIFACT_ROOT="$(TRIAGE_RUNTIME_E2E_ARTIFACT_ROOT)" \
		$(PYTHON) scripts/triage-runtime-e2e.py \
		"$(TRIAGE_RUNTIME_E2E_BINARY)" \
		--expect-current-checkout-sha \
		--expected-profile "$(TRIAGE_RUNTIME_E2E_PROFILE)"

## Full validation: precheck + test suite + triage harness contract tests
ci: precheck test triage-runtime-e2e-static
	@echo ""
	@echo "══════════════════════════════════════"
	@echo "  ✓ CI-equivalent gates passed"
	@echo "══════════════════════════════════════"

## Packaging provenance guard — refuse to package a dirty worktree
release-guard:
	@./scripts/release-provenance.zsh guard

## Prove the provenance guard rejects a dirty tree (and restores it)
release-guard-self-test:
	@./scripts/release-provenance.zsh self-test

## Canonical local package: guard → release build → archive → checksum → receipt
package:
	@./scripts/release-provenance.zsh package

## Installer negative matrix — proves tools/install.sh fails closed
install-test:
	@sh tools/install_test.sh

## Static release contract — version/signing/workflow/cold-install invariants
release-contract-test:
	@PYTHONDONTWRITEBYTECODE=1 $(PYTHON) tools/release_contract_test.py

## Release security gate — fail on unexplained Semgrep findings or baseline drift
semgrep:
	@command -v semgrep >/dev/null 2>&1 || { \
		echo "ERROR: semgrep is required for release verification" >&2; \
		exit 1; \
	}
	python3 tools/semgrep_inventory.py scan --config p/rust

# ──────────────────────────────────────────────────────────
# Version + release (aicx-shaped; TYPE= aliases VERSION=)
# ──────────────────────────────────────────────────────────
#
#   make version                              # bare = check (sync surfaces)
#   make version TYPE=patch|minor|major|x.y.z # bump (also VERSION=)
#   make release-prepare TYPE=patch           # bump + changelog + precheck
#   make release-preflight                    # canonical production tag gate
#   make release-check                        # alias of release-preflight
#   make release-plan                         # printed operator flow
#   make release                              # still = cargo release build
#                                             # (see release-build alias)
#

## Bare `make version` = check. With TYPE=/VERSION= = bump.
version:
ifeq ($(origin VERSION),command line)
	@$(MAKE) version-bump VERSION=$(VERSION)
else ifneq ($(TYPE),)
	@$(MAKE) version-bump VERSION=$(TYPE)
else
	@$(MAKE) version-check
endif

version-show:
	@printf "package: %s\n" "$(PACKAGE_NAME)"
	@printf "version: %s\n" "$(WS_VERSION)"
	@printf "tag: %s\n" "$(TAG)"
	@if git rev-parse --verify "refs/tags/$(TAG)" >/dev/null 2>&1; then \
		echo "tag-state: exists"; \
	else \
		echo "tag-state: missing"; \
	fi

version-check:
	@$(PYTHON) tools/release_sync.py check

version-bump:
ifeq ($(origin VERSION),command line)
	@$(PYTHON) tools/release_sync.py bump "$(VERSION)"
	@echo ""
	@echo "Workspace version, path-dep pins, and installer default synced."
	@echo "Cargo.lock is intentionally not touched by version-bump."
	@echo "To sync lockfile offline:  cargo update --workspace --offline"
	@echo "Or rely on 'make release-prepare' to sync it for you."
else ifneq ($(TYPE),)
	@$(MAKE) version-bump VERSION=$(TYPE)
else
	@echo "VERSION (or TYPE) is required. Usage:" >&2
	@echo "  make version TYPE={patch|minor|major|x.y.z}" >&2
	@echo "  make version-bump VERSION={patch|minor|major|x.y.z}" >&2
	@exit 1
endif

version-patch bump-patch:
	@$(MAKE) version-bump VERSION=patch

changelog-close:
	@$(PYTHON) tools/changelog_close.py $(if $(CHANGELOG_GENERATE),--generate-if-empty)

release-notes:
	@$(PYTHON) tools/release_sync.py notes $(if $(origin VERSION),$(VERSION),) $(if $(OUTPUT),--output $(OUTPUT),)

release-plan:
	@echo "vc-frame release flow (aicx-shaped)"
	@echo ""
	@echo "1. Ensure branch is green (make precheck / make ci)."
	@echo "2. Prepare:"
	@echo "     make release-prepare TYPE={patch|minor|major|x.y.z}"
	@echo "   (or VERSION=… — same thing)"
	@echo "   → version-bump + changelog-close + notes preview + precheck"
	@echo "3. Review diff, commit Cargo.toml + Cargo.lock + tools/install.sh + CHANGELOG.md."
	@echo "4. make release-preflight     # canonical fail-closed production gate"
	@echo "   (make release-check is an exact alias)"
	@echo "5. make release-tag           # reruns preflight; requires pinned GPG key"
	@echo "6. make release-push          # re-verifies ref, signature, target, remote"
	@echo "7. Wait for GitHub Actions / draft release from the tag."
	@echo "8. Optional local archive: make package   # needs clean worktree"
	@echo ""
	@echo "Build-only (not a publish): make release   # cargo xtask build --release"
	@echo "Docs: docs/RELEASE.md"

release-prepare:
ifeq ($(origin VERSION),command line)
	@$(MAKE) version-bump VERSION=$(VERSION)
	@$(MAKE) changelog-close CHANGELOG_GENERATE=1
	@cargo update --workspace --offline
	@$(MAKE) version-check
	@mkdir -p target/dist
	@$(PYTHON) tools/release_sync.py notes --output target/dist/release-notes.md
	@$(MAKE) precheck
	@echo ""
	@echo "=== Release prepared ==="
	@echo "Next: review diff, commit, then:"
	@echo "  make release-preflight"
	@echo "  make release-tag"
	@echo "  make release-push"
	@echo "  cat target/dist/release-notes.md"
else ifneq ($(TYPE),)
	@$(MAKE) release-prepare VERSION=$(TYPE)
else
	@echo "VERSION (or TYPE) is required. Usage:" >&2
	@echo "  make release-prepare TYPE={patch|minor|major|x.y.z}" >&2
	@exit 1
endif

# Canonical local production release gate. Keep the ref/provenance checks both
# before and after the quality cone so a gate cannot silently dirty the source
# represented by the tag. The manual candidate workflow deliberately owns its
# own GitHub checkout verification and does not require a local main branch.
release-preflight:
	@./scripts/release-provenance.zsh preflight
	@$(PYTHON) tools/release_sync.py check --require-version-section
	@$(MAKE) release-contract-test
	@$(MAKE) plugins-parity
	@$(MAKE) semgrep
	@$(MAKE) ci
	@$(CARGO) build --bin vc-frame
	@$(MAKE) triage-runtime-e2e
	@$(MAKE) install-test
	@./scripts/release-provenance.zsh preflight
	@echo "Canonical release preflight passed."

release-check: release-preflight

# The prerequisite is intentionally PHONY: tag creation always reruns the full
# gate, even if an operator already invoked release-check in the same checkout.
release-tag: release-preflight
	@RELEASE_KEYS_DIR="$(RELEASE_KEYS_DIR)" \
		./scripts/release-provenance.zsh create-tag "$(TAG)"

release-push:
	@RELEASE_KEYS_DIR="$(RELEASE_KEYS_DIR)" \
		./scripts/release-provenance.zsh push-tag "$(TAG)"

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
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "plugins" "Compile-check WASM plugins (no asset copy)"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "plugins-assets" "Release-build plugins into assets/ + SHA256SUMS"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "plugins-parity" "Verify assets match SHA256SUMS"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "binary" "Build only host binary (plugins must exist)"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "release" "Build everything in release mode (cargo; not publish)"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "install" "Release build + install to ~/.cargo/bin + link the vc-frame alias"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "run" "Run the locally built vc-frame"
	@printf "\n  $(C_YELLOW)QUALITY GATES$(C_RESET)\n"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "precheck" "Format check + clippy -D warnings + workspace typecheck"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "ci" "Precheck + full test suite"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "clippy" "Clippy only"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "fmt" "Format code"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "fmt-check" "Check formatting without modifying files"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "check" "Quick workspace typecheck"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "triage-runtime-e2e-static" "Harness syntax + unit contract tests"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "triage-runtime-e2e" "Isolated triage process-boundary regression"
	@printf "\n  $(C_YELLOW)VERSION / RELEASE$(C_RESET)\n"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "version" "Bare = check; TYPE=|VERSION= patch|minor|major|x.y.z = bump"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "version-show" "Print package version + tag state"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "version-check" "Validate Cargo pins + installer version + CHANGELOG basics"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "version-bump" "Bump VERSION={patch|minor|major|x.y.z} (TYPE= alias ok)"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "release-plan" "Print operator release flow"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "release-prepare" "Bump + changelog-close + notes + precheck (TYPE= required)"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "release-preflight" "Canonical clean-main + full production gate"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "release-check" "Exact alias of release-preflight"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "release-tag" "Rerun preflight; create + verify pinned GPG tag"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "release-push" "Re-verify immutable signed tag, then push exact object"
	@printf "\n  $(C_YELLOW)RELEASE PROVENANCE$(C_RESET)\n"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "release-guard" "Refuse to package a dirty worktree"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "package" "Guard + release build + archive + receipt"
	@printf "    $(C_GREEN)%-16s$(C_RESET) %s\n" "install-test" "Installer fail-closed negative matrix"
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
	@printf "    make plugins-assets # release-build + refresh bundled WASM + SHA256SUMS\n"
	@printf "    make plugins-parity # verify bundled WASM hashes\n"
	@printf "    make install        # canonical release install + ~/.local/bin alias\n"
	@printf "    make run            # run local debug vc-frame\n"
	@printf "    make version        # check version surfaces\n"
	@printf "    make release-plan   # how to cut a release\n\n"
