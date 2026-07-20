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
	$(DOCKER) build -t $(AURA_RUNNER_IMAGE) --target runner -f $(DOCKER_FILE) $(PROJECT_ROOT)

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

.PHONY: build-images
build-images: | $(REPORT_DIR) ## Build server+test images via buildx with an S3 layer cache
	@set -euo pipefail; \
	: "$${AWS_ACCESS_KEY_ID}"; \
	: "$${AWS_SECRET_ACCESS_KEY}"; \
	: "$${BUILD_CACHE_BUCKET}"; \
	: "$${BUILD_CACHE_REGION}"; \
	: "$${BUILD_CACHE_PREFIX}"; \
	: "$${CACHE_BUILDER}"; \
	: "$${AURA_SERVER_IMAGE}"; \
	: "$${AURA_TEST_IMAGE}"; \
	cleanup_builder() { \
		docker buildx rm "$${CACHE_BUILDER}" >/dev/null 2>&1 || true; \
		for log in $(REPORT_DIR)/buildx-cached-build-*.log; do \
			[ -f "$$log" ] || continue; \
			{ sed -E 's/(AKIA|ASIA)[A-Z0-9]{16}/\1_REDACTED/g' "$$log" > "$$log.tmp" && mv "$$log.tmp" "$$log"; } || true; \
		done; \
	}; \
	trap cleanup_builder EXIT; \
	trap 'exit 1' HUP INT TERM; \
	printf 'Jenkins node: %s\n' "$${NODE_NAME:-unknown}"; \
	printf 'S3 prefix: s3://%s/%s\n' "$${BUILD_CACHE_BUCKET}" "$${BUILD_CACHE_PREFIX}"; \
	docker buildx version; \
	docker buildx rm "$${CACHE_BUILDER}" >/dev/null 2>&1 || true; \
	docker buildx create \
		--name "$${CACHE_BUILDER}" \
		--driver docker-container \
		--driver-opt env.AWS_ACCESS_KEY_ID="$${AWS_ACCESS_KEY_ID}" \
		--driver-opt env.AWS_SECRET_ACCESS_KEY="$${AWS_SECRET_ACCESS_KEY}" \
		--platform linux/amd64; \
	docker buildx inspect "$${CACHE_BUILDER}" --bootstrap; \
	probe_spec="type=s3,region=$${BUILD_CACHE_REGION},bucket=$${BUILD_CACHE_BUCKET},prefix=$${BUILD_CACHE_PREFIX},name=auth-probe"; \
	printf 'FROM busybox@sha256:fd8d9aa63ba2f0982b5304e1ee8d3b90a210bc1ffb5314d980eb6962f1a9715d\nRUN echo cache-auth-probe\n' \
		| docker buildx build \
			--builder "$${CACHE_BUILDER}" \
			--platform linux/amd64 \
			--progress=plain \
			--cache-to "$${probe_spec},mode=max" \
			--output type=cacheonly \
			- 2>&1 \
		| tee $(REPORT_DIR)/buildx-cached-build-probe.log; \
	manifest_hash=$$(cat Cargo.toml Cargo.lock crates/*/Cargo.toml | { sha256sum 2>/dev/null || shasum -a 256; } | cut -d' ' -f1); \
	build_target() { \
		local target=$$1 cache_name=$$2 tag=$$3 extra_args=$${4:-}; \
		local log="$(REPORT_DIR)/buildx-cached-build-$${target}.log"; \
		local cache_spec="type=s3,region=$${BUILD_CACHE_REGION},bucket=$${BUILD_CACHE_BUCKET},prefix=$${BUILD_CACHE_PREFIX},name=$${cache_name}"; \
		local started finished; \
		started=$$(date +%s); \
		docker buildx build \
			--builder "$${CACHE_BUILDER}" \
			--platform linux/amd64 \
			--progress=plain \
			--pull \
			$$extra_args \
			--target "$${target}" \
			--cache-from "$${cache_spec}" \
			--cache-to "$${cache_spec},mode=max" \
			--load \
			-t "$${tag}" \
			-f Dockerfile . 2>&1 \
			| tee "$$log"; \
		finished=$$(date +%s); \
		local cached_steps cache_imports; \
		cached_steps=$$(grep -c 'CACHED' "$$log" || true); \
		cached_steps=$${cached_steps:-0}; \
		cache_imports=$$(grep -c 'importing cache manifest' "$$log" || true); \
		cache_imports=$${cache_imports:-0}; \
		{ \
			printf 'Target: %s\n' "$$target"; \
			printf 'Image tag: %s\n' "$$tag"; \
			printf 'Cached steps: %s\n' "$$cached_steps"; \
			printf 'Cache manifest imports: %s\n' "$$cache_imports"; \
			printf 'Deps manifest hash: %s\n' "$$manifest_hash"; \
			printf 'Build seconds: %s\n' "$$((finished - started))"; \
		} | tee -a "$$log"; \
	}; \
	build_target test test-amd64 "$${AURA_TEST_IMAGE}" \
		"--build-arg NEXTEST_VERSION=$(NEXTEST_VERSION) --build-arg GRCOV_VERSION=$(GRCOV_VERSION)"; \
	build_target server server-amd64 "$${AURA_SERVER_IMAGE}"; \
	docker image ls --format '{{.Repository}}:{{.Tag}} {{.Size}}' \
		| grep -F -e "$${AURA_SERVER_IMAGE}" -e "$${AURA_TEST_IMAGE}" || true
