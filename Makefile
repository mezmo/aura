# Makefile for aura project
# Adapted from pipeline-assignment-loop

-include .config.mk

# Provide standard defaults - set overrides in .config.mk
SHELL=/bin/bash -o pipefail
ALWAYS_TIMESTAMP_VERSION ?= false
APP_NAME ?= aura-api
PROJECT_NAME ?= aura
DEFAULT_BRANCH ?= main
PUBLISH_LATEST ?= false

## Define sources for rendering and templating
GIT_SHA1 ?= $(shell git log --pretty=format:'%h' -n 1)
GIT_BRANCH ?= $(shell git branch --show-current)
GIT_URL ?= $(shell git remote get-url origin)
GIT_INFO ?= $(TMP_DIR)/.git-info.$(GIT_SHA1)
BUILD_URL ?= localbuild://${USER}@$(shell uname -n | sed "s/'//g")
BUILD_DATESTAMP ?= $(shell date -u '+%Y%m%dT%H%M%SZ')

TMP_DIR ?= tmp
BUILD_ENV ?= $(TMP_DIR)/build-env
VERSION_INFO ?= $(TMP_DIR)/version-info

# Define commands via docker
DOCKER ?= docker
DOCKER_RUN := $(DOCKER) run --rm -i
DOCKER_RUN_BUILD_ENV := $(DOCKER_RUN) --env-file=$(BUILD_ENV)

# Handle versioning
ifeq ("$(VERSION_INFO)", "$(wildcard $(VERSION_INFO))")
  # if tmp/version-info exists on disk, use it
  include $(VERSION_INFO)
else
  # Extract version from Cargo.toml
  CARGO_VERSION := $(shell grep -m1 '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
  BUILD_VERSION := $(CARGO_VERSION)-$(BUILD_DATESTAMP)
  ifneq ("$(GIT_BRANCH)", $(filter "$(GIT_BRANCH)", "master" "main"))
    # Feature branch - use timestamped version
    RELEASE_VERSION := $(BUILD_VERSION)
  else ifeq ("$(ALWAYS_TIMESTAMP_VERSION)", "true")
    # Always use timestamp
    RELEASE_VERSION := $(BUILD_VERSION)
  else
    # Release branch - use semantic version from Cargo.toml
    RELEASE_VERSION := $(CARGO_VERSION)
  endif
endif

# Exports the variables for shell use
export

# Source in repository specific environment variables
MAKEFILE_LIB=.makefiles
MAKEFILE_INCLUDES=$(wildcard $(MAKEFILE_LIB)/*.mk)
include $(MAKEFILE_INCLUDES)

$(BUILD_ENV):: $(GIT_INFO) $(VERSION_INFO)
	@cat $(VERSION_INFO) $(GIT_INFO) | sort > $(@)

$(VERSION_INFO):: $(GIT_INFO)
	@env | awk '!/TOKEN/ && /^(BUILD|CARGO_VERSION|RELEASE_VERSION)/ { print }' | sort > $(@)

$(GIT_INFO):: $(TMP_DIR)
	@env | awk '!/TOKEN/ && /^(GIT)/ { print }' | sort > $(@)

$(TMP_DIR)::
	@mkdir -p $(@)

# This helper function makes debugging much easier.
.PHONY:debug-%
debug-%:              ## Debug a variable by calling `make debug-VARIABLE`
	@echo $(*) = $($(*))

.PHONY:help
.SILENT:help
help:                 ## Show this help, includes list of all actions.
	@awk 'BEGIN {FS = ":.*?## "}; /^.+: .*?## / && !/awk/ {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}' ${MAKEFILE_LIST}

.PHONY:build
build:: $(BUILD_ENV)  ## Build all workspace binaries
	cargo build --workspace

.PHONY:build-release
build-release::       ## Build release binaries
	cargo build --release --workspace

.PHONY:test
test::                ## Run all tests (cargo + integration)
	cargo test --workspace
	@$(MAKE) test-integration

.PHONY:test-integration
test-integration::    ## Run integration tests via Docker Compose
	@echo "Starting integration test environment..."
	docker compose -p aura-test-$(GIT_SHA1) \
		-f compose/base.yml \
		-f compose/test.yml \
		up --build --force-recreate --exit-code-from aura-integration-test
	@$(MAKE) test-integration-down

.PHONY:test-integration-down
test-integration-down:  ## Cleanup integration test containers
	-docker compose -p aura-test-$(GIT_SHA1) \
		-f compose/base.yml \
		-f compose/test.yml \
		down --remove-orphans --volumes --rmi=local 2>/dev/null || true

# Mock MCP source directory (override for worktrees: MOCK_MCP_SRC_DIR=../aura-mock-mcp/main)
MOCK_MCP_SRC_DIR ?= ../aura-mock-mcp

.PHONY:mock-mcp-build
mock-mcp-build::      ## Build local mock-mcp image (set MOCK_MCP_SRC_DIR for custom path)
	@echo "Building aura-mock-mcp:local from $(MOCK_MCP_SRC_DIR)..."
	@if [ ! -d "$(MOCK_MCP_SRC_DIR)" ]; then \
		echo "Error: $(MOCK_MCP_SRC_DIR) not found."; \
		echo "  Clone it: git clone git@github.com:answerbook/aura-mock-mcp.git $(MOCK_MCP_SRC_DIR)"; \
		echo "  Or set MOCK_MCP_SRC_DIR to your git branch/worktree path"; \
		exit 1; \
	fi
	docker build -t aura-mock-mcp:local $(MOCK_MCP_SRC_DIR)

.PHONY:test-integration-local-up
test-integration-local-up::  ## Start local aura infra for testing
	@echo "Building fresh mock-mcp image from $(MOCK_MCP_SRC_DIR)..."
	@$(MAKE) mock-mcp-build
	@echo "Starting local aura infra for testing..."
	docker compose -f compose/base.yml -f compose/dev.yml up -d --build --force-recreate
	@echo "Waiting for services to be healthy..."
	@timeout=90; while [ $$timeout -gt 0 ]; do \
		mcp_status=$$(docker compose -f compose/base.yml -f compose/dev.yml ps mock-mcp --format '{{.Health}}' 2>/dev/null); \
		aura_status=$$(docker compose -f compose/base.yml -f compose/dev.yml ps aura-web-server --format '{{.Health}}' 2>/dev/null); \
		if [ "$$mcp_status" = "healthy" ] && [ "$$aura_status" = "healthy" ]; then \
			echo "✅ mock-mcp is healthy"; \
			echo "✅ aura-web-server is healthy"; \
			exit 0; \
		fi; \
		echo "Waiting... mock-mcp: $$mcp_status, aura-web-server: $$aura_status ($$timeout s remaining)"; \
		sleep 2; \
		timeout=$$((timeout - 2)); \
	done; \
	echo "❌ Timeout waiting for services to become healthy"; \
	exit 1

.PHONY:test-integration-local-down
test-integration-local-down::  ## Stop local aura infra
	@echo "Stopping local aura infra..."
	docker compose -f compose/base.yml -f compose/dev.yml down

.PHONY:test-integration-local
test-integration-local::  ## Start local aura infra, run integration tests, then cleanup
	@$(MAKE) test-integration-local-up
	@echo "Running integration tests..."
	@trap '$(MAKE) test-integration-local-down; exit 130' INT TERM; \
	cargo test --package aura-web-server --features integration --no-fail-fast -- --test-threads=1; \
	test_exit=$$?; \
	$(MAKE) test-integration-local-down; \
	exit $$test_exit

# --- Orchestration integration tests ---
# Orchestration tests require a different server config (orchestration.enabled = true).
# These are NOT included in the parent `integration` feature flag.

.PHONY:test-integration-orchestration
test-integration-orchestration::  ## Run orchestration integration tests via Docker Compose
	@echo "Starting orchestration integration test environment..."
	docker compose -p aura-test-orchestration-$(GIT_SHA1) \
		-f compose/base.yml \
		-f compose/orchestration.yml \
		-f compose/orchestration-test.yml \
		up --build --force-recreate --exit-code-from aura-orchestration-test
	@$(MAKE) test-integration-orchestration-down

.PHONY:test-integration-orchestration-down
test-integration-orchestration-down:  ## Cleanup orchestration integration test containers
	-docker compose -p aura-test-orchestration-$(GIT_SHA1) \
		-f compose/base.yml \
		-f compose/orchestration.yml \
		-f compose/orchestration-test.yml \
		down --remove-orphans --volumes --rmi=local 2>/dev/null || true

.PHONY:test-integration-orchestration-local-up
test-integration-orchestration-local-up::  ## Start local aura infra for orchestration testing
	@echo "Starting local aura infra for orchestration testing (math-mcp built via compose)..."
	docker compose -f compose/base.yml -f compose/orchestration.yml -f compose/dev.yml up -d --build --force-recreate
	@echo "Waiting for services to be healthy..."
	@timeout=120; while [ $$timeout -gt 0 ]; do \
		math_status=$$(docker compose -f compose/base.yml -f compose/orchestration.yml -f compose/dev.yml ps math-mcp --format '{{.Health}}' 2>/dev/null); \
		aura_status=$$(docker compose -f compose/base.yml -f compose/orchestration.yml -f compose/dev.yml ps aura-web-server --format '{{.Health}}' 2>/dev/null); \
		if [ "$$math_status" = "healthy" ] && [ "$$aura_status" = "healthy" ]; then \
			echo "✅ math-mcp is healthy"; \
			echo "✅ aura-web-server is healthy"; \
			exit 0; \
		fi; \
		echo "Waiting... math-mcp: $$math_status, aura-web-server: $$aura_status ($$timeout s remaining)"; \
		sleep 2; \
		timeout=$$((timeout - 2)); \
	done; \
	echo "❌ Timeout waiting for services to become healthy"; \
	exit 1

.PHONY:test-integration-orchestration-local-down
test-integration-orchestration-local-down::  ## Stop local aura infra for orchestration
	@echo "Stopping local aura infra for orchestration..."
	docker compose -f compose/base.yml -f compose/orchestration.yml -f compose/dev.yml down

.PHONY:test-integration-orchestration-local
test-integration-orchestration-local::  ## Start local orchestration infra, run tests, then cleanup
	@$(MAKE) test-integration-orchestration-local-up
	@echo "Running orchestration integration tests..."
	@trap '$(MAKE) test-integration-orchestration-local-down; exit 130' INT TERM; \
	cargo test --package aura-web-server --features integration-orchestration --no-fail-fast -- --test-threads=1; \
	test_exit=$$?; \
	$(MAKE) test-integration-orchestration-local-down; \
	exit $$test_exit

.PHONY:fmt
fmt::                 ## Format code with rustfmt
	cargo fmt --all

.PHONY:fmt-check
fmt-check::           ## Check code formatting (CI)
	cargo fmt --all -- --check

.PHONY:lint
lint::                ## Run clippy linter
	cargo clippy --all-targets --all-features -- -D warnings

.PHONY:ci
ci:: fmt-check test lint  ## Run CI checks locally (fmt + test + lint)
	@echo "✅ All CI checks passed!"

.PHONY:clean
clean::               ## Cleanup the local checkout
	-rm -rf *.backup tmp/ output/
	cargo clean

.PHONY:clean-all
clean-all:: clean     ## Full cleanup of all artifacts
	-git clean -Xdf

.PHONY:docker-build
docker-build::        ## Build Docker image (full release)
	$(DOCKER) build -t $(PROJECT_NAME):latest .

.PHONY:docker-test
docker-test::         ## Run Docker build with test stage (base)
	$(DOCKER) build --target release-build -t $(PROJECT_NAME):test .

.PHONY:docker-build-release
docker-build-release:: ## Build Docker release stage only
	$(DOCKER) build --target release -t $(PROJECT_NAME):$(RELEASE_VERSION) .

.PHONY:publish
publish::             ## Placeholder for publishing artifacts

.PHONY:version
version::             ## Show version information
	@echo "RELEASE_VERSION: $(RELEASE_VERSION)"
	@echo "BUILD_VERSION: $(BUILD_VERSION)"
	@echo "GIT_SHA1: $(GIT_SHA1)"
	@echo "GIT_BRANCH: $(GIT_BRANCH)"
