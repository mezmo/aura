lint:: lint-commits


.PHONY:lint-commits
lint-commits: $(REPORT_DIR) $(DOCKER_ENV) ## lint commits on current branch ahead of main
	$(RUN_NO_ENV) npm run commitlint $(if $(IS_CI),-- --format=checkstyle --report-dir=$(REPORT_DIR),)
