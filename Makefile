# Makefile for aura project

-include .config.mk
-include .local.mk
slugify = $(shell echo '$(1)' | tr '[:upper:]' '[:lower:]' | sed -E 's/[^a-z0-9]+/-/g' | sed 's/^-//;s/-$$//')

# Provide standard defaults - set overrides in .config.mk
SHELL=/bin/bash -o pipefail
TARGET_DIR = target
COVERAGE_DIR := report
REPORT_DIR = $(COVERAGE_DIR)/ci
AURA_AUTO_DOWNLOAD ?= true

CI ?=
IS_CI := $(if $(filter true, $(CI)), true,)
ALWAYS_TIMESTAMP_VERSION ?= false
APP_NAME ?= $(shell git remote -v | awk '/origin/ && /fetch/ { sub(/\.git/, ""); n=split($$2, origin, "/"); print origin[n]}')
## Define sources for rendering and templating
GIT_SHA1 ?= $(shell git log --pretty=format:'%h' -n 1)
GIT_BRANCH ?= $(shell git branch --show-current)
GIT_URL ?= $(shell git remote get-url origin)
GIT_INFO ?= $(TMP_DIR)/.git-info.$(GIT_SHA1)
BUILD_URL ?= localbuild://${USER}@$(shell uname -n | sed "s/'//g")
BUILD_DATESTAMP ?= $(shell date -u '+%Y%m%dT%H%M%SZ')
ENABLE_DOCKER ?=
TMP_DIR ?= tmp
BUILD_ENV ?= $(TMP_DIR)/build-env
VERSION_INFO ?= $(TMP_DIR)/version-info
PROJECT_ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))

# Define commands via docker
DOCKER ?= docker
DOCKER_FILE := $(PROJECT_ROOT)/Dockerfile
DOCKER_RUN := $(DOCKER) run --rm -i
DOCKER_ENV = $(TARGET_DIR)/aura-env
RUNNER_CMD = $(DOCKER_RUN) --env-file=$(DOCKER_ENV) $(if $(filter true, $(IS_CI)), ,-t) -v $(PWD):/home/aura $(AURA_RUNNER_IMAGE)
RUNNER_NO_ENV_CMD = $(DOCKER_RUN) $(if $(filter true, $(IS_CI)),,-t) -v $(PWD):/home/aura $(AURA_RUNNER_IMAGE)
DOCKER_RUN_BUILD_ENV := $(RUNNER_CMD)
BUILD_TAG ?= 1 # this is set by Jenkins and is unique per build
AURA_RUNNER_IMAGE := local/aura-runner:$(call slugify, $(BUILD_TAG))

RUN := $(if $(filter true, $(ENABLE_DOCKER)), $(RUNNER_CMD),)
RUN_NO_ENV := $(if $(filter true, $(ENABLE_DOCKER)), $(RUNNER_NO_ENV_CMD),)

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
MAKEFILE_LIB ?= .makefiles
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

$(TARGET_DIR):
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

.PHONY:test
test::                ## Run all tests targets

.PHONY:setup
setup::              ## Setup local depencies for development

.PHONY:lint
lint::                ## Apply all lint targets

.PHONY:ci
ci:: fmt-check test lint  ## Run CI checks locally (fmt + test + lint)
	@echo "✅ All CI checks passed!"

.PHONY:clean
clean::               ## Cleanup the local checkout

.PHONY:docker-build
docker-build::        ## Build Docker image (full release)
	$(DOCKER) build -t $(APP_NAME):latest .

.PHONY:docker-test
docker-test::         ## Run Docker build with test stage (base)
	$(DOCKER) build --target release-lint-test -t $(APP_NAME):test .

.PHONY:docker-build-release
docker-build-release:: ## Build Docker release stage only
	$(DOCKER) build --target release -t $(APP_NAME):$(RELEASE_VERSION) .

.PHONY:publish
publish::             ## Placeholder for publishing artifacts

$(DOCKER_ENV): $(TARGET_DIR) $(REPORT_DIR) ## Set up docker info
	@env | awk '!/TOKEN|KEY/ && /^(AURA_|CI|LLVM|RUST|GRCOV|CARGO|NEXTEST)/ { print }' | sort > $(@)


.PHONY:version
version::             ## Show version information
	@MAKEFLAGS+=--no-print-directory $(MAKE) debug-RELEASE_VERSION debug-BUILD_VERSION debug-GIT_SHA1 debug-GIT_BRANCH
