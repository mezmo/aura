
BUILD_TAG ?= 1

.PHONY:start
start:: ## Start the docker compose setup
	docker compose -f compose/base.yml -f compose/dev.yml up

.PHONY:stop
stop:: ## Stop the docker compose setup
	docker compose -f compose/base.yml -f compose/dev.yml down

.PHONY: test-integration
test-inegration: ## run CI test suite via docker compose
	docker compose -p aura-${BUILD_TAG:-1} -f compose/base.yml -f compose/test.yml up --remove-orphans --exit-code-from aura --build
