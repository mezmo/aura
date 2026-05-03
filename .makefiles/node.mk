setup:: setup-node
clean:: clean-node

.PHONY:setup-node
setup-node:  ## Setup node environment and dependencies
	@if [ -z "$$(stat node_modules)" ]; then \
		$(RUN_NO_ENV) npm install; \
	fi

.PHONY:clean-node
clean-node:  ## Remove node specific artifacts
	$(RUN_NO_ENV) rm -rf node_modules
