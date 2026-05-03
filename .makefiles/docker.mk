setup:: setup-docker
$(BUILD_ENV):: $(DOCKER_ENV)

WITH_DOCKER_ENV ?= true

.PHONY:setup-docker
setup-docker:
	@if [ -z "$$(docker images -q $(AURA_RUNNER_IMAGE))" ]; then \
		$(DOCKER) build -t $(AURA_RUNNER_IMAGE) --target runner -f $(DOCKER_FILE) $(PROJECT_ROOT); \
	fi

.PHONY:docker-shell
shell: $(DOCKER_ENV)
	$(RUN) bash

clean-docker:
	-@docker rmi $(AURA_RUNNER_IMAGE) -f
