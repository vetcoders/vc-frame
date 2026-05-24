# Zellij (vc-frame) Build System
# Includes comprehensive cargo xtask wrapping and VetCoders flow
#
# 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI

.PHONY: all build install run clean test check clippy precheck fmt help

# Default target
all: build

# Build all workspace members (including WASM plugins and the zellij binary)
build:
	cargo xtask build

# Install the zellij binary locally
install:
	cargo xtask install

# Run the locally built zellij binary
run:
	cargo xtask run

# Run tests
test:
	RUST_MIN_STACK=8388608 cargo test -p zellij-server

# Quick check (compilation only)
check:
	cargo check --workspace

# Run clippy checks
clippy:
	cargo clippy --workspace --all-targets -- -D warnings -A clippy::too_many_arguments -A clippy::type_complexity -A clippy::borrowed_box -A clippy::ptr_arg

# Full pre-push/pre-build validation (fmt + clippy + check)
precheck:
	@echo "=== Pre-push Check ==="
	@echo "[1/3] Checking formatting..."
	@cargo xtask format --check || (echo "Run 'make fmt' to fix" && exit 1)
	@echo "[2/3] Running clippy..."
	@cargo clippy --workspace --all-targets -- -D warnings -A clippy::too_many_arguments -A clippy::type_complexity -A clippy::borrowed_box -A clippy::ptr_arg
	@echo "[3/3] Type checking..."
	@cargo check --workspace
	@echo "=== All checks passed ==="

# Format code
fmt:
	cargo xtask format

# Clean build artifacts
clean:
	cargo clean

# Help description
help:
	@echo "Zellij Build System"
	@echo ""
	@echo "Commands:"
	@echo "  make precheck  - Run fmt, clippy, and type checks"
	@echo "  make build     - Build plugins and zellij binary"
	@echo "  make install   - Install zellij locally"
	@echo "  make run       - Run local zellij"
	@echo "  make test      - Run zellij-server tests"
	@echo "  make check     - Quick compilation typecheck"
	@echo "  make clippy    - Run clippy linting checks"
	@echo "  make fmt       - Format all code"
	@echo "  make clean     - Clean cargo build files"
