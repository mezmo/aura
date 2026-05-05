lint:: lint-commits


.PHONY:lint-commits
lint-commits: $(DOCKER_ENV) ## lint commits on current branch ahead of main
	$(RUN_NO_ENV) npm run commitlint
