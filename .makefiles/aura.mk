.PHONY:start
start: ## Start the docker compose setup
	docker compose -p aura-$(BUILD_SLUG) -f compose/base.yml -f compose/dev.yml up --remove-orphans -d

.PHONY:stop
stop: ## Stop the docker compose setup
	docker compose -p aura-$(BUILD_SLUG) -f compose/base.yml -f compose/dev.yml down

.PHONY:test-integration
test-integration: $(REPORT_DIR) ## run CI test suite via docker compose
	docker compose -p aura-test-$(BUILD_SLUG) -f compose/base.yml -f compose/test.yml up --remove-orphans --exit-code-from aura-integration-test --build

.PHONY:test-integration-down
test-integration-down:  ## Cleanup integration test containers
	-docker compose -p aura-test-$(BUILD_SLUG) \
		-f compose/base.yml \
		-f compose/test.yml \
		down --remove-orphans --volumes --rmi=local 2>/dev/null || true

.PHONY:test-integration-local-up
test-integration-local-up:  ## Start local aura infra for testing
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
test-integration-local-down:  ## Stop local aura infra
	@echo "Stopping local aura infra..."
	docker compose -f compose/base.yml -f compose/dev.yml down

.PHONY:test-integration-local
test-integration-local:: $(REPORT_DIR) ## Start local aura infra, run integration tests, then cleanup
	@$(MAKE) test-integration-local-up
	@echo "Running integration tests..."
	@trap '$(MAKE) test-integration-local-down; exit 130' INT TERM; \
	cargo test --package aura-web-server --features integration --no-fail-fast -- --test-threads=1; \
	test_exit=$$?; \
	$(MAKE) test-integration-local-down; \
	exit $$test_exit

# --- STDIO integration tests (no Docker infra needed; local child process) ---

.PHONY:test-stdio-local
test-stdio-local: $(REPORT_DIR) ## Run STDIO MCP integration tests locally
	@echo "Running STDIO MCP integration tests..."
	@cargo test --package aura --features integration-stdio --no-fail-fast -- --test-threads=1

# --- Orchestration integration tests ---

.PHONY:test-integration-orchestration
test-integration-orchestration: $(REPORT_DIR) ## Run orchestration integration tests via Docker Compose
	@echo "Starting orchestration integration test environment..."
	docker compose -p aura-test-orchestration-$(BUILD_SLUG) \
		-f compose/base.yml \
		-f compose/orchestration.yml \
		-f compose/orchestration-test.yml \
		up --build --force-recreate --exit-code-from aura-orchestration-test
	@$(MAKE) test-integration-orchestration-down

.PHONY:test-integration-orchestration-down
test-integration-orchestration-down:  ## Cleanup orchestration integration test containers
	-docker compose -p aura-test-orchestration-$(BUILD_SLUG) \
		-f compose/base.yml \
		-f compose/orchestration.yml \
		-f compose/orchestration-test.yml \
		down --remove-orphans --volumes --rmi=local 2>/dev/null || true

.PHONY:test-integration-orchestration-local-up
test-integration-orchestration-local-up:  ## Start local aura infra for orchestration testing
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
test-integration-orchestration-local-down:  ## Stop local aura infra for orchestration
	@echo "Stopping local aura infra for orchestration..."
	docker compose -f compose/base.yml -f compose/orchestration.yml -f compose/dev.yml down

.PHONY:test-integration-orchestration-local
test-integration-orchestration-local: $(REPORT_DIR) ## Start local orchestration infra, run tests, then cleanup
	@$(MAKE) test-integration-orchestration-local-up
	@echo "Running orchestration integration tests..."
	@trap '$(MAKE) test-integration-orchestration-local-down; exit 130' INT TERM; \
	cargo test --package aura-web-server --features integration-orchestration --no-fail-fast -- --test-threads=1; \
	test_exit=$$?; \
	$(MAKE) test-integration-orchestration-local-down; \
	exit $$test_exit

# --- SRE Orchestration integration tests ---

.PHONY:test-integration-sre-orchestration
test-integration-sre-orchestration: $(REPORT_DIR) ## Run SRE orchestration integration tests via Docker Compose
	@echo "Starting SRE orchestration integration test environment..."
	docker compose -p aura-test-sre-orch-$(BUILD_SLUG) \
		-f compose/base.yml \
		-f compose/sre-orchestration.yml \
		-f compose/sre-orchestration-test.yml \
		up --build --force-recreate --exit-code-from aura-sre-orchestration-test
	@$(MAKE) test-integration-sre-orchestration-down

.PHONY:test-integration-sre-orchestration-down
test-integration-sre-orchestration-down:  ## Cleanup SRE orchestration integration test containers
	-docker compose -p aura-test-sre-orch-$(BUILD_SLUG) \
		-f compose/base.yml \
		-f compose/sre-orchestration.yml \
		-f compose/sre-orchestration-test.yml \
		down --remove-orphans --volumes --rmi=local 2>/dev/null || true

.PHONY:test-integration-sre-orchestration-local-up
test-integration-sre-orchestration-local-up:  ## Start local aura infra for SRE orchestration testing
	@echo "Starting local aura infra for SRE orchestration testing..."
	docker compose -f compose/base.yml -f compose/sre-orchestration.yml -f compose/dev.yml up -d --build --force-recreate
	@echo "Waiting for services to be healthy..."
	@timeout=120; while [ $$timeout -gt 0 ]; do \
		sre_status=$$(docker compose -f compose/base.yml -f compose/sre-orchestration.yml -f compose/dev.yml ps k8s-sre-mcp --format '{{.Health}}' 2>/dev/null); \
		aura_status=$$(docker compose -f compose/base.yml -f compose/sre-orchestration.yml -f compose/dev.yml ps aura-web-server --format '{{.Health}}' 2>/dev/null); \
		if [ "$$sre_status" = "healthy" ] && [ "$$aura_status" = "healthy" ]; then \
			echo "✅ k8s-sre-mcp is healthy"; \
			echo "✅ aura-web-server is healthy"; \
			exit 0; \
		fi; \
		echo "Waiting... k8s-sre-mcp: $$sre_status, aura-web-server: $$aura_status ($$timeout s remaining)"; \
		sleep 2; \
		timeout=$$((timeout - 2)); \
	done; \
	echo "❌ Timeout waiting for services to become healthy"; \
	exit 1

.PHONY:test-integration-sre-orchestration-local-down
test-integration-sre-orchestration-local-down:  ## Stop local aura infra for SRE orchestration
	@echo "Stopping local aura infra for SRE orchestration..."
	docker compose -f compose/base.yml -f compose/sre-orchestration.yml -f compose/dev.yml down

.PHONY:test-integration-sre-orchestration-local
test-integration-sre-orchestration-local: $(REPORT_DIR) ## Start local SRE orchestration infra, run tests, then cleanup
	@$(MAKE) test-integration-sre-orchestration-local-up
	@echo "Running SRE orchestration integration tests..."
	@trap '$(MAKE) test-integration-sre-orchestration-local-down; exit 130' INT TERM; \
	cargo test --package aura-web-server --features integration-orchestration-sre --no-fail-fast -- --test-threads=1; \
	test_exit=$$?; \
	$(MAKE) test-integration-sre-orchestration-local-down; \
	exit $$test_exit

# --- Scratchpad integration tests ---

.PHONY:test-integration-scratchpad
test-integration-scratchpad: $(REPORT_DIR) ## Run scratchpad integration tests via Docker Compose
	@echo "Starting scratchpad integration test environment..."
	docker compose -p aura-test-scratchpad-$(BUILD_SLUG) \
		-f compose/base.yml \
		-f compose/scratchpad.yml \
		-f compose/scratchpad-test.yml \
		up --build --force-recreate --exit-code-from aura-scratchpad-test
	@$(MAKE) test-integration-scratchpad-down

.PHONY:test-integration-scratchpad-down
test-integration-scratchpad-down:  ## Cleanup scratchpad integration test containers
	-docker compose -p aura-test-scratchpad-$(BUILD_SLUG) \
		-f compose/base.yml \
		-f compose/scratchpad.yml \
		-f compose/scratchpad-test.yml \
		down --remove-orphans --volumes --rmi=local 2>/dev/null || true

.PHONY:test-integration-scratchpad-local-up
test-integration-scratchpad-local-up:  ## Start local aura infra for scratchpad testing
	@echo "Starting local aura infra for scratchpad testing..."
	docker compose -f compose/base.yml -f compose/scratchpad.yml -f compose/dev.yml up -d --build --force-recreate
	@echo "Waiting for services to be healthy..."
	@timeout=120; while [ $$timeout -gt 0 ]; do \
		sp_status=$$(docker compose -f compose/base.yml -f compose/scratchpad.yml -f compose/dev.yml ps scratchpad-test-mcp --format '{{.Health}}' 2>/dev/null); \
		aura_status=$$(docker compose -f compose/base.yml -f compose/scratchpad.yml -f compose/dev.yml ps aura-web-server --format '{{.Health}}' 2>/dev/null); \
		if [ "$$sp_status" = "healthy" ] && [ "$$aura_status" = "healthy" ]; then \
			echo "✅ scratchpad-test-mcp is healthy"; \
			echo "✅ aura-web-server is healthy"; \
			exit 0; \
		fi; \
		echo "Waiting... scratchpad-test-mcp: $$sp_status, aura-web-server: $$aura_status ($$timeout s remaining)"; \
		sleep 2; \
		timeout=$$((timeout - 2)); \
	done; \
	echo "❌ Timeout waiting for services to become healthy"; \
	exit 1

.PHONY:test-integration-scratchpad-local-down
test-integration-scratchpad-local-down:  ## Stop local aura infra for scratchpad
	@echo "Stopping local aura infra for scratchpad..."
	docker compose -f compose/base.yml -f compose/scratchpad.yml -f compose/dev.yml down

.PHONY:test-integration-scratchpad-local
test-integration-scratchpad-local: $(REPORT_DIR) ## Start local scratchpad infra, run tests, then cleanup
	@$(MAKE) test-integration-scratchpad-local-up
	@echo "Running scratchpad integration tests..."
	@trap '$(MAKE) test-integration-scratchpad-local-down; exit 130' INT TERM; \
	cargo test --package aura-web-server --features integration-scratchpad --no-fail-fast -- --test-threads=1; \
	test_exit=$$?; \
	$(MAKE) test-integration-scratchpad-local-down; \
	exit $$test_exit
