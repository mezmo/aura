
BUILD_TAG ?= 1

.PHONY:start
start: ## Start the docker compose setup
	docker compose -p aura-${BUILD_TAG:-1} -f compose/base.yml -f compose/dev.yml up --remove-orphans -d

.PHONY:stop
stop: ## Stop the docker compose setup
	docker compose -p aura-${BUILD_TAG:-1} -f compose/base.yml -f compose/dev.yml down

.PHONY:test-integration
test-integration: $(REPORT_DIR) ## run CI test suite via docker compose
	docker compose -p aura-test-${BUILD_TAG:-1} -f compose/base.yml -f compose/test.yml up --remove-orphans --exit-code-from aura-integration-test --build

.PHONY:test-integration-down
test-integration-down:  ## Cleanup integration test containers
	-docker compose -p aura-test-${BUILD_TAG:-1} \
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
