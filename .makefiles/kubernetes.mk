OUTPUT_DIR ?= output
S3_BUCKET ?= logdna-artifacts-k8s
SOURCE_DIR ?= deployment/kubernetes

# These are standard defaults that rely on a prior variable
RENDERED_DIR ?= $(TMP_DIR)/rendered
S3_BASE_DIR ?= apps/$(APP_NAME)

ENVSUBST_COMMAND := $(DOCKER_RUN_BUILD_ENV) -v $(PWD):/data:Z bhgedigital/envsubst envsubst "$$(printf '$${%s} ' $$(cut -f1 -d'=' ${BUILD_ENV}))"
KUBEVAL_COMMAND := $(DOCKER_RUN) -v $(PWD):/data:Z garethr/kubeval --ignore-missing-schemas
S3_COMMAND := $(DOCKER_RUN) -v ~/.aws:/root/.aws:Z -v $(PWD):/aws:Z --env AWS_ACCESS_KEY_ID --env AWS_SECRET_ACCESS_KEY amazon/aws-cli s3
YAMLLINT_COMMAND := $(DOCKER_RUN) -v $(PWD):/data:Z cytopia/yamllint:latest

# Special S3 sub commands and release variables
S3_CP_COMMAND := $(S3_COMMAND) cp --recursive
S3_SYNC_COMMAND := $(S3_COMMAND) sync --content-type "text/yaml" --delete
S3_TARGET_PATH := s3://$(S3_BUCKET)/$(S3_BASE_DIR)

# Makefile target generations
SOURCE_ENVSUBST := $(wildcard $(SOURCE_DIR)/*.envsubst)
ENVSUBST := $(SOURCE_ENVSUBST)

OUTPUTS := $(patsubst $(SOURCE_DIR)/%.envsubst,$(OUTPUT_DIR)/%,$(SOURCE_ENVSUBST))
LOCAL_OUTPUTS := $(patsubst $(OUTPUT_DIR)/%,$(RENDERED_DIR)/%,$(OUTPUTS))

build:: render
lint:: render lint-k8s lint-yaml ## Run all linting rules
publish:: publish-s3

.PHONY:.check-env
.check-env: .check-env-render .check-env-publish

.PHONY:.check-env-render
.check-env-render:
	mkdir -p $(OUTPUT_DIR) $(RENDERED_DIR)
	@test $${SOURCE_DIR?WARN: Undefined SOURCE_DIR required as source to render}
	@echo Render environment check complete

.PHONY:.check-env-publish
.check-env-publish:
	@if [ "${AWS_PROFILE}" == "" ]; then \
		if [ "${AWS_SECRET_ACCESS_KEY}" == "" ] || [ "${AWS_ACCESS_KEY_ID}" == "" ]; then \
			echo "ERROR: AWS_PROFILE _or_ AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY must be provided"; \
			exit 1; \
		fi; \
	fi
	@test $${S3_BASE_DIR?WARN: Undefined S3_BASE_DIR required to define target for publishing}
	@test $${RELEASE_VERSION?WARN: Undefined RELEASE_VERSION required as release target}
	@echo Publishing environment check complete

.PHONY:render
render: .check-env-render $(BUILD_ENV) $(OUTPUTS)   ## Renders all of the envsubst files for publishing

$(OUTPUT_DIR) $(RENDERED_DIR):
	mkdir -p $(@)

$(GIT_INFO):: | $(RENDERED_DIR)

$(BUILD_ENV):: | $(RENDERED_DIR)

$(OUTPUT_DIR)/%: $(SOURCE_DIR)/%.envsubst $(BUILD_ENV) | $(OUTPUT_DIR)
	$(ENVSUBST_COMMAND) <$(<) > $(@)

.PHONY:lint-k8s
lint-k8s: $(OUTPUTS)   ## Run kubeval linting against the k8s resources
	$(KUBEVAL_COMMAND) -d /data/$(OUTPUT_DIR) --ignored-filename-patterns='aura-*'

.PHONY:lint-yaml
lint-yaml: $(OUTPUTS)     ## Run yaml linting against the k8s resources
	$(YAMLLINT_COMMAND) /data/$(OUTPUT_DIR)

.PHONY:publish-s3
publish-s3: .check-env render
	$(S3_CP_COMMAND) $(OUTPUT_DIR) $(S3_TARGET_PATH)/$(RELEASE_VERSION)
ifeq ("$(PUBLISH_LATEST)", "true")
	$(S3_SYNC_COMMAND) $(OUTPUT_DIR) $(S3_TARGET_PATH)/latest
endif
