test:: test-scripts

.PHONY: test-scripts
test-scripts: ## Run the shell script test suites in scripts/tests
	@for t in scripts/tests/*.test.sh; do "$$t" || exit 1; done
