CARGO ?= cargo

# The release binary ships default features; tests build every feature so the
# optional code stays compiled and covered. See CLAUDE.md for what 'otel' is.
BUILD_FEATURES ?=
TEST_FEATURES ?= --all-features

# Optional formatters. 'make indent' degrades to what is installed rather than
# failing: commentflow reflows comments in Rust and shell sources, shfmt lays
# out the shell scripts themselves.
COMMENTFLOW := $(shell command -v commentflow 2>/dev/null)
SHFMT := $(shell command -v shfmt 2>/dev/null)
LLVM_COV := $(shell command -v cargo-llvm-cov 2>/dev/null)

# Shell scripts are handed to the formatters through find -exec rather than a
# variable: '+' runs nothing when there are no scripts (a bare shfmt would read
# stdin and hang), and paths containing spaces survive.
FIND_SHELL := find . -path ./target -prune -o -path ./.git -prune -o -name '*.sh'

BINDIR ?= $(HOME)/.local/bin
PYTHON ?= python3
RELEASE_BUILD = $(CARGO) build --release $(BUILD_FEATURES)

# Through the environment, not the command line: a path may hold quotes or
# spaces that no recipe quoting survives.
export BINDIR

.PHONY: all clean check coverage indent install register

all:
	$(RELEASE_BUILD)

# Builds, installs into BINDIR, then registers. scripts/install.py holds the
# why of each step.
install:
	$(PYTHON) scripts/install.py install $(RELEASE_BUILD)

register:
	$(PYTHON) scripts/install.py register

clean:
	$(CARGO) clean

check:
	$(CARGO) test --all-targets $(TEST_FEATURES)

# Line coverage for the same suite 'check' runs. Needs cargo-llvm-cov.
coverage:
ifeq ($(LLVM_COV),)
	@echo "cargo-llvm-cov not found; install it with: cargo install cargo-llvm-cov"
else
	$(CARGO) llvm-cov --all-targets $(TEST_FEATURES) --summary-only
endif

# Comments are rewrapped first, then each language's own formatter runs over the
# result so it owns the final layout.
indent:
ifeq ($(COMMENTFLOW),)
	@echo "commentflow not found; comment reflow skipped"
else
	$(COMMENTFLOW) src tests
	$(FIND_SHELL) -exec $(COMMENTFLOW) {} +
endif
	$(CARGO) fmt
ifeq ($(SHFMT),)
	@echo "shfmt not found; shell scripts left unformatted"
else
# No printer flags below: passing even one makes shfmt ignore .editorconfig,
# which is where this project's shell style is defined.
	$(FIND_SHELL) -exec $(SHFMT) --write {} +
endif
