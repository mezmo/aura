lint:: lint-rust fmt-rust
clean:: clean-rust clean-node
build:: build-rust

COVERAGE_DIR ?= $(TARGET_DIR)/ci
REPORT_DIR = $(COVERAGE_DIR)/reports
CARGO_BIN_DIR ?= .bin
NEXTEST_BIN = $(CARGO_BIN_DIR)/cargo-nextest
LLVM_PROFILE_FILE ?= $(COVERAGE_DIR)/profile-%p-%m.profraw
GRCOV_VERSION ?= v0.10.7
GRCOV_BIN = $(CARGO_BIN_DIR)/grcov
PATH := $(CARGO_BIN_DIR):$(PATH)
RUSTFLAGS=--allow=warnings -Cinstrument-coverage
AURA_RELEASE :=

$(CARGO_BIN_DIR):
	@mkdir -p $(@)

$(REPORT_DIR):
	@mkdir -p $(@)

.PHONY:build-rust
build-rust: $(DOCKER_ENV) ## Build all rust targets
	$(RUN) cargo build --workspace $(if $(AURA_RELEASE),--release,)

.PHONY:coverage
coverage: $(DOCKER_ENV) $(REPORT_DIR) $(GRCOV_BIN) ## Run the local test suite with code coverage
	-$(MAKE) nextest || touch $(TARGET_DIR)/.nextest-failed
	
	$(RUN) grcov $(COVERAGE_DIR) . --binary-path $(TARGET_DIR)/debug --output-types cobertura,html --output-path $(REPORT_DIR) --llvm --branch --source-dir . || touch $(TARGET_DIR)/.coverage-failed

	@if [ -f $(TARGET_DIR)/.nextest-failed ] || [ -f $(TARGET_DIR)/.grcov-failed ]; then \
		rm -f $(TARGET_DIR)/.nextest-failed $(TARGET_DIR)/.grcov-failed; \
		exit 1; \
	fi

.PHONY:nextest
nextest: $(DOCKER_ENV) $(NEXTEST_BIN) $(REPORT_DIR)
	$(RUN) cargo nextest run --workspace --all-targets $(if $(IS_CI),-P ci,)

.PHONY:lint-rust
lint-rust: | $(DOCKER_ENV) $(REPORT_DIR) .check-env-render  ## lint rust code via clippy
	$(RUN) cargo clippy $(if $(IS_CI),-q,) --all-targets --all-features $(if $(IS_CI),--message-format=json,) -- -D warnings $(if $(IS_CI),> $(REPORT_DIR)/clippy.json,)

.PHONY:clean-rust
clean-rust: ## Clean up rust build artifacts
	$(RUN_NO_ENV) cargo clean

.PHONY:clean-bin
clean-bin: $(DOCKER_ENV) ## Cleanup the binaries added by aura
	$(RUN_NO_ENV) rm -f $(NEXTEST_BIN) $(GRCOV_BIN)

.PHONY:fmt-rust
fmt-rust:: $(REPORT_DIR)                 ## Format code with rustfmt
	$(RUN_NO_ENV) cargo +nightly fmt --all $(if $(IS_CI),-- --emit checkstyle > $(REPORT_DIR)/fmt.tmp,)
	@echo '<?xml version="1.0" encoding="utf-8"?><checkstyle version="4.3">' > $(REPORT_DIR)/rustfmt.xml
	@sed 's/></>\n</g' $(REPORT_DIR)/fmt.tmp | grep -E '^<(file|error|/file)' >> $(REPORT_DIR)/rustfmt.xml || true
	@echo '</checkstyle>' >> $(REPORT_DIR)/rustfmt.xml

$(NEXTEST_BIN): $(CARGO_BIN_DIR)
	@echo "Setting up cargo-nextest in $(CARGO_BIN_DIR)"
	@case "$$(uname -s)" in \
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
	@echo "Setting up grcov"
	@case "$$(uname -s)" in \
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

