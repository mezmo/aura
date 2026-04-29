lint:: lint-rust
clean:: clean-rust

.PHONY:lint-rust | .check-env-render
lint-rust: ## lint rust code via clippy
	cargo clippy $(if $(CI),-q,) --all-targets --all-features $(if $(CI),--message-format=json,) -- -D warnings $(if $(CI),> $(OUTPUT_DIR)/clippy.json,)

.PHONY:clean
clean-rust: ## Clean up rust build artifacts
	cargo clean
