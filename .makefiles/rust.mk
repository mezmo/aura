lint:: lint-rust fmt-rust
clean:: clean-rust clean-node clean-report
build:: build-rust

CARGO_BIN_DIR ?= .bin
NEXTEST_BIN = $(CARGO_BIN_DIR)/cargo-nextest
NEXTEST_VERSION ?= 0.9.133
GRCOV_VERSION ?= v0.10.7
GRCOV_BIN = $(CARGO_BIN_DIR)/grcov
PATH := $(CARGO_BIN_DIR):$(PATH)
AURA_RELEASE :=

$(CARGO_BIN_DIR):
	@mkdir -p $(@)

$(REPORT_DIR):
	@mkdir -p $(@)

.PHONY:build-rust
build-rust: $(DOCKER_ENV) ## Build all rust targets
	$(RUN) cargo build --workspace $(if $(AURA_RELEASE),--release,) $(if $(IS_CI),--quiet,)

.PHONY:coverage
coverage: $(DOCKER_ENV) $(REPORT_DIR) $(GRCOV_BIN) ## Run the local test suite with code coverage
	-$(MAKE) debug-PROJECT_ROOT
	-export RUSTFLAGS="--allow=warnings -Cinstrument-coverage"; \
		export LLVM_PROFILE_FILE=$(PROJECT_ROOT)/$(COVERAGE_DIR)/build-%p-%m.profraw; \
		cargo build --all-targets --workspace --frozen; \
		export LLVM_PROFILE_FILE=$(PROJECT_ROOT)/$(COVERAGE_DIR)/profile-%p-%m.profraw; \
		$(MAKE) nextest|| touch $(TARGET_DIR)/.nextest-failed
	$(RUN) grcov $(COVERAGE_DIR) . \
		--binary-path $(TARGET_DIR)/debug \
		--ignore-not-existing \
		--keep-only 'crates/**' \
		--ignore '/*' \
		--ignore '/usr/local/cargo/**' \
		--ignore '*_test.rs' \
		--output-types cobertura,html \
		--output-path $(REPORT_DIR) \
		--llvm \
		--branch \
		--source-dir . \
		|| touch $(TARGET_DIR)/.grcov-failed

	@# Hit/miss/non-cacheable counters land in the lane log when the
	@# coverage compile ran through sccache.
	-@if [ -n "$(RUSTC_WORKSPACE_WRAPPER)" ] || [ -n "$(RUSTC_WRAPPER)" ]; then \
		sccache --show-stats || true; \
	fi
	@if [ -f $(TARGET_DIR)/.nextest-failed ] || [ -f $(TARGET_DIR)/.grcov-failed ]; then \
		rm -f $(TARGET_DIR)/.nextest-failed $(TARGET_DIR)/.grcov-failed; \
		exit 1; \
	fi

.PHONY:nextest
nextest: $(DOCKER_ENV) $(NEXTEST_BIN) $(REPORT_DIR)
	$(RUN) cargo nextest run --workspace --all-targets --features integration $(if $(IS_CI),-P ci,)

.PHONY:lint-rust
lint-rust: | $(DOCKER_ENV) $(REPORT_DIR)  ## lint rust code via clippy
	$(RUN) cargo clippy $(if $(IS_CI),-q,) --all-targets --all-features $(if $(IS_CI),--message-format=json,) -- -D warnings $(if $(IS_CI),> $(REPORT_DIR)/clippy.json,)

.PHONY: check-cli-http-only
check-cli-http-only: $(DOCKER_ENV) ## Verify the HTTP-only (no-default-features) aura-cli still builds
	$(RUN) cargo clippy -p aura-cli --no-default-features --all-targets -- -D warnings
	$(RUN) cargo test -p aura-cli --no-default-features

.PHONY: update-lockfile
# cargo update only re-resolves Cargo.lock and never compiles, so strip any
# sccache wrapper the CI env injects: the S3-backed cache would demand AWS
# creds this step lacks and time out on the IMDS fallback.
update-lockfile: $(DOCKER_ENV) ## Regenerate Cargo.lock after version changes
	$(RUN) env -u RUSTC_WRAPPER -u RUSTC_WORKSPACE_WRAPPER cargo update --quiet --workspace

.PHONY:clean-rust
clean-rust: ## Clean up rust build artifacts
	$(RUN_NO_ENV) cargo clean

clean:: clean-toolchain-cache
.PHONY:clean-toolchain-cache
clean-toolchain-cache: ## Remove the workspace-scoped cargo and rustup homes
	$(RUN_NO_ENV) rm -rf .cargo .rustup

.PHONY:clean-report
clean-report:  ## Clear out the report directory
	$(RUN_NO_ENV) rm  -rf $(COVERAGE_DIR)/*

.PHONY:clean-profile
clean-profile: ## Clean artifacts left over from profiling
	$(RUN_NO_ENV) rm -rf $(COVERAGE_DIR)/*.profraw

.PHONY:clean-bin
clean-bin: $(DOCKER_ENV) ## Cleanup the binaries added by aura
	$(RUN_NO_ENV) rm -f $(NEXTEST_BIN) $(GRCOV_BIN)

.PHONY:fmt-rust
fmt-rust:: $(REPORT_DIR)                 ## Format code with rustfmt
	$(RUN_NO_ENV) cargo +nightly fmt --all $(if $(IS_CI),-- --emit checkstyle > $(REPORT_DIR)/fmt.tmp,)
	@if [ "$(IS_CI)" ]; then \
		REPO_ROOT=$$($(RUN_NO_ENV) pwd); \
		echo '<?xml version="1.0" encoding="utf-8"?><checkstyle version="4.3">' > $(REPORT_DIR)/rustfmt.xml; \
		sed 's/></>\n</g' $(REPORT_DIR)/fmt.tmp | grep -E '^<(file|error|/file)' >> $(REPORT_DIR)/rustfmt.xml || true; \
		echo '</checkstyle>' >> $(REPORT_DIR)/rustfmt.xml; \
		sed -i.bak "s|name=\"$$REPO_ROOT/|name=\"|g" $(REPORT_DIR)/rustfmt.xml; \
		rm -f $(REPORT_DIR)/rustfmt.xml.bak; \
	fi

$(DIST_DIR):
	@mkdir -p $(@)

# Cargo build profile for the packaged binaries: "release" or "debug". Selects
# both the cargo flag and the target/ output subdirectory cargo writes to.
PROFILE ?= release
ifeq ($(filter $(PROFILE),release debug),)
$(error PROFILE must be 'release' or 'debug' (got '$(PROFILE)'))
endif
CARGO_PROFILE_FLAG := $(if $(filter release,$(PROFILE)),--release,)

# Shell snippet that aborts unless the build host matches $(1)=uname -s and
# $(2)=uname -m ($(2) empty = any arch). Inlined into a build's bash -c so the
# check runs against the same context that compiles.
require_host = os=\$$(uname -s); arch=\$$(uname -m); if [ \$$os != $(1) ]$(if $(2), || [ \$$arch != $(2) ],); then echo error: $@ must be built on $(1)$(if $(2), $(2),), build host is \$$os \$$arch >&2; exit 1; fi;

.PHONY: build-binary-linux-amd64
build-binary-linux-amd64: $(DIST_DIR) $(DOCKER_ENV) ## Build binaries for linux/amd64 (PROFILE=release|debug)
	$(RUN) bash -c "\
		$(call require_host,Linux,x86_64) \
		cargo build $(CARGO_PROFILE_FLAG) --bin aura-web-server && \
		cargo build $(CARGO_PROFILE_FLAG) -p aura-cli --bin aura; \
		rc=\$$?; \
		if [ -n \"\$$RUSTC_WRAPPER\" ]; then sccache --show-stats || true; fi; \
		exit \$$rc"
	cp target/$(PROFILE)/aura-web-server $(DIST_DIR)/aura-web-server-linux-amd64
	cp target/$(PROFILE)/aura $(DIST_DIR)/aura-linux-amd64

.PHONY: build-binary-linux-arm64
build-binary-linux-arm64: $(DIST_DIR) $(DOCKER_ENV) ## Build binaries for linux/arm64 (PROFILE=release|debug)
	$(RUN) bash -c "\
		$(call require_host,Linux,x86_64) \
		rustup target add aarch64-unknown-linux-gnu 2>/dev/null; \
		export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc; \
		export CC_aarch64_unknown_linux_gnu=/usr/bin/aarch64-linux-gnu-gcc; \
		export CXX_aarch64_unknown_linux_gnu=/usr/bin/aarch64-linux-gnu-g++; \
		cargo build $(CARGO_PROFILE_FLAG) --target aarch64-unknown-linux-gnu --bin aura-web-server && \
		cargo build $(CARGO_PROFILE_FLAG) --target aarch64-unknown-linux-gnu -p aura-cli --bin aura; \
		rc=\$$?; \
		if [ -n \"\$$RUSTC_WRAPPER\" ]; then sccache --show-stats || true; fi; \
		exit \$$rc"
	cp target/aarch64-unknown-linux-gnu/$(PROFILE)/aura-web-server $(DIST_DIR)/aura-web-server-linux-arm64
	cp target/aarch64-unknown-linux-gnu/$(PROFILE)/aura $(DIST_DIR)/aura-linux-arm64

.PHONY: build-binary-darwin-amd64
build-binary-darwin-amd64: $(DIST_DIR) $(DOCKER_ENV) ## Build binaries for darwin/amd64 (PROFILE=release|debug)
	$(RUN) bash -c "\
		$(call require_host,Darwin,) \
		rustup target add x86_64-apple-darwin 2>/dev/null; \
		cargo build $(CARGO_PROFILE_FLAG) --target x86_64-apple-darwin --bin aura-web-server && \
		cargo build $(CARGO_PROFILE_FLAG) --target x86_64-apple-darwin -p aura-cli --bin aura"
	cp target/x86_64-apple-darwin/$(PROFILE)/aura-web-server $(DIST_DIR)/aura-web-server-darwin-amd64
	cp target/x86_64-apple-darwin/$(PROFILE)/aura $(DIST_DIR)/aura-darwin-amd64

.PHONY: build-binary-darwin-arm64
build-binary-darwin-arm64: $(DIST_DIR) $(DOCKER_ENV) ## Build binaries for darwin/arm64 (PROFILE=release|debug)
	$(RUN) bash -c "\
		$(call require_host,Darwin,) \
		rustup target add aarch64-apple-darwin 2>/dev/null; \
		cargo build $(CARGO_PROFILE_FLAG) --target aarch64-apple-darwin --bin aura-web-server && \
		cargo build $(CARGO_PROFILE_FLAG) --target aarch64-apple-darwin -p aura-cli --bin aura"
	cp target/aarch64-apple-darwin/$(PROFILE)/aura-web-server $(DIST_DIR)/aura-web-server-darwin-arm64
	cp target/aarch64-apple-darwin/$(PROFILE)/aura $(DIST_DIR)/aura-darwin-arm64

.PHONY: build-binaries-linux
build-binaries-linux: ## Build binaries for linux (amd64 + arm64, PROFILE=release|debug)
	$(MAKE) build-binary-linux-amd64 build-binary-linux-arm64

.PHONY: build-binaries-darwin
build-binaries-darwin: ## Build binaries for darwin (amd64 + arm64, PROFILE=release|debug)
	$(MAKE) build-binary-darwin-amd64 build-binary-darwin-arm64

.PHONY: build-checksums
build-checksums: ## Write sha256 checksums for the binaries in dist
	cd $(DIST_DIR) && sha256sum aura-* > checksums.txt

# Every binary a complete release must contain, across all platforms.
EXPECTED_BINARIES := \
	aura-linux-amd64 aura-web-server-linux-amd64 \
	aura-linux-arm64 aura-web-server-linux-arm64 \
	aura-darwin-amd64 aura-web-server-darwin-amd64 \
	aura-darwin-arm64 aura-web-server-darwin-arm64

.PHONY: verify-binaries
verify-binaries: build-checksums $(DOCKER_ENV) ## Verify every expected binary is present and checksummed correctly
	@cd $(DIST_DIR) && \
	for f in $(EXPECTED_BINARIES); do \
		[ -f "$$f" ] || { echo "error: missing binary: $$f" >&2; exit 1; }; \
		grep -q " $$f\$$" checksums.txt || { echo "error: $$f absent from checksums.txt" >&2; exit 1; }; \
	done
	cd $(DIST_DIR) && sha256sum -c checksums.txt
	@# Execution smoke for the pair the runner container can actually run;
	@# cross-arch artifacts are covered by presence + checksum only. The
	@# chmod restores the executable bit, which stash/unstash can drop.
	@if [ "$(ENABLE_DOCKER)" = "true" ]; then \
		chmod +x $(DIST_DIR)/aura-linux-amd64 $(DIST_DIR)/aura-web-server-linux-amd64 && \
		$(RUN) ./dist/aura-linux-amd64 --version && \
		$(RUN) ./dist/aura-web-server-linux-amd64 --version; \
	else \
		echo "skipping execution smoke: runner container disabled"; \
	fi

clean:: clean-dist

.PHONY: clean-dist
clean-dist:
	rm -rf $(DIST_DIR)

$(NEXTEST_BIN): $(CARGO_BIN_DIR)
	@if [ "$(AURA_AUTO_DOWNLOAD)" != "true" ]; then \
		exit 0; \
	fi; \
	echo "Setting up cargo-nextest in $(CARGO_BIN_DIR)"; \
	case "$$(uname -s)" in \
		Darwin) plat="mac";; \
		Linux) \
			case "$$(uname -m)" in \
				aarch64|arm64) plat="linux-arm";; \
				*) plat="linux";; \
			esac;; \
		*) echo "Unsupported platform"; exit 1;; \
	esac; \
	echo "Downloading for $$plat"; \
	curl -LsSf "https://get.nexte.st/$(NEXTEST_VERSION)/$$plat" | tar zxf - -C $(CARGO_BIN_DIR); \
	chmod +x $@; \
	touch $@

$(GRCOV_BIN): | $(CARGO_BIN_DIR)
	@if [ "$(AURA_AUTO_DOWNLOAD)" != "true" ]; then \
		exit 0; \
	fi; \
	echo "Setting up grcov"; \
	case "$$(uname -s)" in \
		Darwin) \
			case "$$(uname -m)" in \
				aarch64|arm64) target="aarch64-apple-darwin";; \
				x86_64) target="x86_64-apple-darwin";; \
				*) target="x86_64-apple-darwin";; \
			esac;; \
		Linux) \
			case "$$(uname -m)" in \
				aarch64|arm64) target="aarch64-unknown-linux-gnu";; \
				x86_64) \
					if ldd $$(which ls) | grep -q musl; then \
						target="x86_64-unknown-linux-musl"; \
					else \
						target="x86_64-unknown-linux-gnu"; \
					fi;; \
				*) target="x86_64-unknown-linux-gnu";; \
			esac;; \
		*) echo "Unsupported platform"; exit 1;; \
	esac; \
	echo "Downloading for $$target"; \
	curl -LsSf "https://github.com/mozilla/grcov/releases/download/$(GRCOV_VERSION)/grcov-$$target.tar.bz2" | tar xjf - -C $(CARGO_BIN_DIR); \
	chmod +x "$@"; \
	touch "$@"
