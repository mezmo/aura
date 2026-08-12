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
	@# Terminate the shell on a failed compile: make only sees the last status.
	export RUSTFLAGS="--allow=warnings -Cinstrument-coverage"; \
		export LLVM_PROFILE_FILE=$(PROJECT_ROOT)/$(COVERAGE_DIR)/build-%p-%m.profraw; \
		cargo build --all-targets --workspace --frozen || exit 1; \
		export LLVM_PROFILE_FILE=$(PROJECT_ROOT)/$(COVERAGE_DIR)/profile-%p-%m.profraw; \
		$(MAKE) nextest || touch $(TARGET_DIR)/.nextest-failed
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

.PHONY:lint-rust-ci
lint-rust-ci: | $(REPORT_DIR)  ## lint rust code via clippy against the prebuilt AURA_TEST_IMAGE (CI only; needs `make build-images` first)
	$(if $(AURA_TEST_IMAGE),,$(error AURA_TEST_IMAGE is not set; run make build-images first))
	$(DOCKER) run --rm $(AURA_TEST_IMAGE) cargo clippy -q --all-targets --all-features --message-format=json -- -D warnings > $(REPORT_DIR)/clippy.json

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

# Linux target triples. cargo-zigbuild links these with zig, so both build from
# any Linux host arch — no matching cross-gcc toolchain required.
LINUX_AMD64_TARGET := x86_64-unknown-linux-gnu
LINUX_ARM64_TARGET := aarch64-unknown-linux-gnu

.PHONY: build-binary-linux-amd64
build-binary-linux-amd64: $(DIST_DIR) $(DOCKER_ENV) ## Build binaries for linux/amd64 (PROFILE=release|debug)
	$(RUN) bash -c "\
		$(call require_host,Linux,) \
		rustup target add $(LINUX_AMD64_TARGET) 2>/dev/null; \
		cargo zigbuild $(CARGO_PROFILE_FLAG) --target $(LINUX_AMD64_TARGET) --bin aura-web-server && \
		cargo zigbuild $(CARGO_PROFILE_FLAG) --target $(LINUX_AMD64_TARGET) -p aura-cli --bin aura; \
		rc=\$$?; \
		if [ -n \"\$$RUSTC_WRAPPER\" ]; then sccache --show-stats || true; fi; \
		exit \$$rc"
	cp target/$(LINUX_AMD64_TARGET)/$(PROFILE)/aura-web-server $(DIST_DIR)/aura-web-server-linux-amd64
	cp target/$(LINUX_AMD64_TARGET)/$(PROFILE)/aura $(DIST_DIR)/aura-linux-amd64

.PHONY: build-binary-linux-arm64
build-binary-linux-arm64: $(DIST_DIR) $(DOCKER_ENV) ## Build binaries for linux/arm64 (PROFILE=release|debug)
	$(RUN) bash -c "\
		$(call require_host,Linux,) \
		rustup target add $(LINUX_ARM64_TARGET) 2>/dev/null; \
		cargo zigbuild $(CARGO_PROFILE_FLAG) --target $(LINUX_ARM64_TARGET) --bin aura-web-server && \
		cargo zigbuild $(CARGO_PROFILE_FLAG) --target $(LINUX_ARM64_TARGET) -p aura-cli --bin aura; \
		rc=\$$?; \
		if [ -n \"\$$RUSTC_WRAPPER\" ]; then sccache --show-stats || true; fi; \
		exit \$$rc"
	cp target/$(LINUX_ARM64_TARGET)/$(PROFILE)/aura-web-server $(DIST_DIR)/aura-web-server-linux-arm64
	cp target/$(LINUX_ARM64_TARGET)/$(PROFILE)/aura $(DIST_DIR)/aura-linux-arm64

.PHONY: build-binary-darwin-amd64
build-binary-darwin-amd64: $(DIST_DIR) $(DOCKER_ENV) ## Build binaries for darwin/amd64 (PROFILE=release|debug)
	$(RUN) bash -c "\
		$(call require_host,Darwin,) \
		rustup target add x86_64-apple-darwin 2>/dev/null; \
		cargo build $(CARGO_PROFILE_FLAG) --target x86_64-apple-darwin --bin aura-web-server && \
		cargo build $(CARGO_PROFILE_FLAG) --target x86_64-apple-darwin -p aura-cli --bin aura; \
		rc=\$$?; \
		if [ -n \"\$$RUSTC_WRAPPER\" ]; then sccache --show-stats || true; fi; \
		exit \$$rc"
	cp target/x86_64-apple-darwin/$(PROFILE)/aura-web-server $(DIST_DIR)/aura-web-server-darwin-amd64
	cp target/x86_64-apple-darwin/$(PROFILE)/aura $(DIST_DIR)/aura-darwin-amd64

.PHONY: build-binary-darwin-arm64
build-binary-darwin-arm64: $(DIST_DIR) $(DOCKER_ENV) ## Build binaries for darwin/arm64 (PROFILE=release|debug)
	$(RUN) bash -c "\
		$(call require_host,Darwin,) \
		rustup target add aarch64-apple-darwin 2>/dev/null; \
		cargo build $(CARGO_PROFILE_FLAG) --target aarch64-apple-darwin --bin aura-web-server && \
		cargo build $(CARGO_PROFILE_FLAG) --target aarch64-apple-darwin -p aura-cli --bin aura; \
		rc=\$$?; \
		if [ -n \"\$$RUSTC_WRAPPER\" ]; then sccache --show-stats || true; fi; \
		exit \$$rc"
	cp target/aarch64-apple-darwin/$(PROFILE)/aura-web-server $(DIST_DIR)/aura-web-server-darwin-arm64
	cp target/aarch64-apple-darwin/$(PROFILE)/aura $(DIST_DIR)/aura-darwin-arm64

.PHONY: build-binaries-linux
build-binaries-linux: ## Build binaries for linux (amd64 + arm64, PROFILE=release|debug)
	$(MAKE) build-binary-linux-amd64 build-binary-linux-arm64

.PHONY: build-binaries-darwin
build-binaries-darwin: ## Build binaries for darwin (amd64 + arm64, PROFILE=release|debug)
	$(MAKE) build-binary-darwin-amd64 build-binary-darwin-arm64

# Signing identity and notarytool credentials for the darwin binaries.
# Supply on the make command line or via the environment. CODESIGN_IDENTITY is
# the SHA-1 hash of the Developer ID Application certificate in the keychain.
CODESIGN_IDENTITY ?=
NOTARY_APPLE_ID ?=
NOTARY_PASSWORD ?=
NOTARY_TEAM_ID ?=

# The darwin binaries produced by build-release-binaries-darwin.
DARWIN_RELEASE_BINARIES := \
	aura-darwin-amd64 aura-web-server-darwin-amd64 \
	aura-darwin-arm64 aura-web-server-darwin-arm64

.PHONY: sign-release-binaries-darwin
sign-release-binaries-darwin: $(DIST_DIR) $(DOCKER_ENV) ## Codesign and notarize the darwin release binaries (requires CODESIGN_IDENTITY, NOTARY_APPLE_ID, NOTARY_PASSWORD, NOTARY_TEAM_ID)
	@[ -n "$(CODESIGN_IDENTITY)" ] || { echo "error: CODESIGN_IDENTITY is required" >&2; exit 1; }
	@[ -n "$(NOTARY_APPLE_ID)" ] || { echo "error: NOTARY_APPLE_ID is required" >&2; exit 1; }
	@[ -n "$(NOTARY_PASSWORD)" ] || { echo "error: NOTARY_PASSWORD is required" >&2; exit 1; }
	@[ -n "$(NOTARY_TEAM_ID)" ] || { echo "error: NOTARY_TEAM_ID is required" >&2; exit 1; }
	$(RUN) bash -c "\
		$(call require_host,Darwin,) \
		set -eo pipefail; \
		cd $(DIST_DIR) && for bin in $(DARWIN_RELEASE_BINARIES); do \
			echo signing \$$bin; \
			codesign --force --timestamp --options runtime --sign '$(CODESIGN_IDENTITY)' \$$bin; \
			echo notarizing \$$bin; \
			ditto -c -k \$$bin \$$bin.zip; \
			xcrun notarytool submit \$$bin.zip --apple-id '$(NOTARY_APPLE_ID)' --password '$(NOTARY_PASSWORD)' --team-id '$(NOTARY_TEAM_ID)' --wait 2>&1 | tee \$$bin.notary.log; \
			grep -q 'status: Accepted' \$$bin.notary.log || { echo error: notarization not accepted for \$$bin >&2; exit 1; }; \
			rm -f \$$bin.zip \$$bin.notary.log; \
		done"

.PHONY: build-checksums
build-checksums: ## Write sha256 checksums for the release artifacts (binaries + any packages) in dist
	@# The *.deb/*.rpm globs overlap aura-* (nfpm's rpm and web-server package
	@# names also start with "aura-"), so sort -u is load-bearing: it collapses
	@# the duplicate matches before hashing.
	@cd $(DIST_DIR) && \
		files=$$(ls aura-* *.deb *.rpm 2>/dev/null | sort -u); \
		[ -n "$$files" ] || { echo "error: no release artifacts found in $(DIST_DIR)" >&2; exit 1; }; \
		printf '%s\n' "$$files" | xargs sha256sum > checksums.txt

# ARCHS and PACKAGERS reach the script through env, which $(RUN) does not carry
# into the container on its own.
.PHONY: build-packages
build-packages: $(DIST_DIR) $(DOCKER_ENV) ## Build .deb/.rpm packages from the linux binaries in dist (PACKAGE_VERSION overrides the version)
	$(RUN) env ARCHS="$(ARCHS)" PACKAGERS="$(PACKAGERS)" ./scripts/build-packages.sh $(PACKAGE_VERSION)

.PHONY: publish-packages
publish-packages: $(DOCKER_ENV) ## Publish the .deb/.rpm packages in dist to Cloudsmith (requires CLOUDSMITH_API_KEY)
	$(RUN) env PACKAGERS="$(PACKAGERS)" ./scripts/publish-packages.sh

# Every binary a complete release must contain, across all platforms.
EXPECTED_BINARIES := \
	aura-linux-amd64 aura-web-server-linux-amd64 \
	aura-linux-arm64 aura-web-server-linux-arm64 \
	aura-darwin-amd64 aura-web-server-darwin-amd64 \
	aura-darwin-arm64 aura-web-server-darwin-arm64

.PHONY: verify-binaries
verify-binaries: $(DOCKER_ENV) ## Verify every expected binary is present, then smoke-test the runnable pair
	@cd $(DIST_DIR) && \
	for f in $(EXPECTED_BINARIES); do \
		[ -f "$$f" ] || { echo "error: missing binary: $$f" >&2; exit 1; }; \
	done
	@# Execution smoke for the pair the runner container can actually run;
	@# cross-arch artifacts are covered by presence only. The chmod restores
	@# the executable bit, which stash/unstash can drop.
	@if [ "$(ENABLE_DOCKER)" = "true" ]; then \
		chmod +x $(DIST_DIR)/aura-linux-amd64 $(DIST_DIR)/aura-web-server-linux-amd64 && \
		$(RUN) ./dist/aura-linux-amd64 --version && \
		$(RUN) ./dist/aura-web-server-linux-amd64 --version; \
	else \
		echo "skipping execution smoke: runner container disabled"; \
	fi

.PHONY: release-artifacts
release-artifacts: ## Assemble a complete release in dist/: verify binaries, build packages, write checksums
	$(MAKE) verify-binaries
	$(MAKE) build-packages
	$(MAKE) build-checksums

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
