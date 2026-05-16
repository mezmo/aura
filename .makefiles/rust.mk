fmt-check::fmt-rust
lint:: lint-rust fmt-rust
clean:: clean-rust clean-node clean-report
build:: build-rust
build-release:: AURA_RELEASE=1
build-release:: build-rust
test:: test-rust

CARGO_BIN_DIR ?= .bin
NEXTEST_BIN = $(CARGO_BIN_DIR)/cargo-nextest
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

.PHONY:test-rust
test-rust: $(DOCKER_ENV) ## Test all rust targets
	$(RUN) cargo test --workspace

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
		|| touch $(TARGET_DIR)/.coverage-failed

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

.PHONY:clean-rust
clean-rust: ## Clean up rust build artifacts
	$(RUN_NO_ENV) cargo clean

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
	curl -LsSf "https://get.nexte.st/latest/$$plat" | tar zxf - -C $(CARGO_BIN_DIR); \
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

