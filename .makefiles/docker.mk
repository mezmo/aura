setup:: setup-docker
lint:: lint-docker
$(BUILD_ENV):: $(DOCKER_ENV)

WITH_DOCKER_ENV ?= true

SED_INPLACE :=
UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
	SED_INPLACE = -i ''
else
	SED_INPLACE = -i
endif

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

.PHONY:lint-docker
lint-docker: | $(REPORT_DIR)
	docker run --rm -i hadolint/hadolint hadolint -f json - < Dockerfile > report/ci/hadolint.json; \
	ret=$$? ; \
	sed ${SED_INPLACE} 's/"file":"-"/"file":"Dockerfile"/g' report/ci/hadolint.json ; \
	exit $$ret
