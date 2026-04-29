lint:: lint-commits


.PHONY:
lint-commits: ## lint commits on current branch ahead of main
	npm run commitlint
