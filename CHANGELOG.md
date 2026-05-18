## Changelog

## [1.20.6](https://github.com/mezmo/aura/compare/v1.20.5...v1.20.6) (2026-05-18)


### Chores

* **owners**: add codeowners [9593804](https://github.com/mezmo/aura/commit/95938042165a61f7b32bfbdf6e928781236653cc) - Phil Ciampini

## [1.20.5](https://github.com/mezmo/aura/compare/v1.20.4...v1.20.5) (2026-05-18)


### Bug Fixes

* **test**: mount quickstart.toml in test container [401d991](https://github.com/mezmo/aura/commit/401d991c75ce1830d03ba1bb7e49851117b889ef) - Eric Lake

### Documentation

* add sane defaults for optional MCP server credentials [b62002c](https://github.com/mezmo/aura/commit/b62002cd4842390cffc17c681c1193fd1200b0d7) - Eric Lake* address inline review feedback [060789f](https://github.com/mezmo/aura/commit/060789f26f088c56982ff9c3f5f01efbe5da5276) - Eric Lake [#139](https://github.com/mezmo/aura/issues/139)* audit fixes, cross-reference consistency, and internal doc cleanup [917134a](https://github.com/mezmo/aura/commit/917134aeec9ca18e09b37faaca405f1fea4274d7) - Eric Lake [#100](https://github.com/mezmo/aura/issues/100)* move quickstart to repo root and streamline onboarding [873880f](https://github.com/mezmo/aura/commit/873880fd89b5f160478f84673957decba88ed4a0) - Eric Lake* **readme**: directory structure and makefiles [7376bd4](https://github.com/mezmo/aura/commit/7376bd42b30cb0c0eaba0a0864a76899e304c0f8) - Dominic McAllister* revert CODE_OF_CONDUCT.md change, defer to separate PR [ce609ae](https://github.com/mezmo/aura/commit/ce609ae25f88e9f214423378185d1fd1b5d07ed4) - Eric Lake* **streaming**: fix stale quality-scoring references and event flow diagram [c2bc7ac](https://github.com/mezmo/aura/commit/c2bc7ac3f0572c3221f88289cf8c0c391eb8fcc9) - Eric Lake [#146](https://github.com/mezmo/aura/issues/146) [#147](https://github.com/mezmo/aura/issues/147)* use env_file for .env loading and document provider-specific keys [f50107b](https://github.com/mezmo/aura/commit/f50107beb23cad3997869178202ff1859c95fda4) - Eric Lake

## [1.20.4](https://github.com/mezmo/aura/compare/v1.20.3...v1.20.4) (2026-05-17)


### Bug Fixes

* **toolchain**: add rust-analyzer to components [ea04dbc](https://github.com/mezmo/aura/commit/ea04dbce69f6d04ab5c9e8dcdcf68dcc299c54a0) - Mike Shearer [#151](https://github.com/mezmo/aura/issues/151)

## [1.20.3](https://github.com/mezmo/aura/compare/v1.20.2...v1.20.3) (2026-05-16)


### Chores

* set mezmobot as author of release commits [b375945](https://github.com/mezmo/aura/commit/b375945acfa52e61dc44d2bdcc9279758d904e45) - Eric Satterwhite

### Service

* **setup**: add allcontributors bot to cla allow list [9749bf7](https://github.com/mezmo/aura/commit/9749bf760bb0ee928306d61c6ad00c7fa33dfd1d) - Eric Satterwhite

## [1.20.2](https://github.com/mezmo/aura/compare/v1.20.1...v1.20.2) (2026-05-15)


### Bug Fixes

* **build**: update magic butler catalog for improved functions [5068cdc](https://github.com/mezmo/aura/commit/5068cdc1e2ec581c09bfa4911c038910f972ed45) - Phil Ciampini* **scratchpad**: bound recursive json walkers with depth cap [c1f5ad0](https://github.com/mezmo/aura/commit/c1f5ad05252faa72697f289151bda368b1943acb) - Justin Gross [LOG-23842](https://mezmo.atlassian.net/browse/LOG-23842)

### Service

* **setup**: remove trigger build stage [a416786](https://github.com/mezmo/aura/commit/a416786537cebf6652a1e02819dea89040b6a849) - Eric Satterwhite

## [1.20.1](https://github.com/mezmo/aura/compare/v1.20.0...v1.20.1) (2026-05-14)


### Bug Fixes

* make sure to regenerate cargo lock file during release [c1de102](https://github.com/mezmo/aura/commit/c1de1029dc1f2a0171782a872986139cc9ae9f5f) - Eric Satterwhite [#137](https://github.com/mezmo/aura/issues/137)

### Service

* **setup**: remove build trigger gate [0eccd5d](https://github.com/mezmo/aura/commit/0eccd5db0a4702091d7902cbd1486276d0bbb146) - Eric Satterwhite

# [1.20.0](https://github.com/mezmo/aura/compare/v1.19.8...v1.20.0) (2026-05-14)


### Bug Fixes

* bump rig-core to include reasoning round-trip fix [c1e38cf](https://github.com/mezmo/aura/commit/c1e38cfe4417ee697b37f6d494e6c767b474aa91) - Mike Shearer [LOG-23438](https://mezmo.atlassian.net/browse/LOG-23438)* **configs**: remove temperature from GPT-5.1 configs [e0c3d3c](https://github.com/mezmo/aura/commit/e0c3d3c365280f3b3176abc3cf3d70153c496a14) - Mike Shearer [LOG-23458](https://mezmo.atlassian.net/browse/LOG-23458)* **deps**: correct mezmo rig fork commit hash in Cargo.toml [62f50d2](https://github.com/mezmo/aura/commit/62f50d217b07f258f794fc6276dd2580e3cb2455) - Mike Shearer [LOG-23790](https://mezmo.atlassian.net/browse/LOG-23790)* **eval**: broaden T4 turn matcher for coordinator rephrasing [4fdf941](https://github.com/mezmo/aura/commit/4fdf94192d5dfbe72240f24a877272bc373814fd) - Mike Shearer [LOG-23470](https://mezmo.atlassian.net/browse/LOG-23470)* **eval**: priority-ordered turn matchers for cross-model goals [da43804](https://github.com/mezmo/aura/commit/da4380429be90a6827317863c299194289b47368) - Mike Shearer [LOG-23470](https://mezmo.atlassian.net/browse/LOG-23470)* **eval**: review cleanup — dead code, stale comments, runner [2158f52](https://github.com/mezmo/aura/commit/2158f52b8a68b1f2b3b82b7d7d1fe7e83928634a) - Mike Shearer [LOG-23470](https://mezmo.atlassian.net/browse/LOG-23470)* **eval**: use null model field for single-config compat [e821cf2](https://github.com/mezmo/aura/commit/e821cf22ca733df16f949a66fb253a703e5cc9e7) - Mike Shearer [LOG-23461](https://mezmo.atlassian.net/browse/LOG-23461)* **lint**: remove unused test_env_lock module [fb18f17](https://github.com/mezmo/aura/commit/fb18f17ec58f92e72d2683f1897fb77ff5de95bf) - Mike Shearer [LOG-23607](https://mezmo.atlassian.net/browse/LOG-23607)* **mcp**: extract inline content from embedded MCP resources [439738b](https://github.com/mezmo/aura/commit/439738b1ed24ecc17acb98aafb61d3d127d85bbd) - Mike Shearer [LOG-23505](https://mezmo.atlassian.net/browse/LOG-23505)* **orchestration**: always synthesize single-task plans [888359e](https://github.com/mezmo/aura/commit/888359e0cf8290ac9ddaebdc2285e077012b12e8) - Mike Shearer [LOG-23540](https://mezmo.atlassian.net/browse/LOG-23540)* **orchestration**: classify size limit errors as context overflow [2543931](https://github.com/mezmo/aura/commit/254393186b726d0015724b99344f87d9ceb1d75c) - Mike Shearer [#120](https://github.com/mezmo/aura/issues/120)* **orchestration**: emit real aura.usage tokens and cost [988b9e7](https://github.com/mezmo/aura/commit/988b9e7144d2280840354a978a28b8fc8e75e78c) - Mike Shearer [#73](https://github.com/mezmo/aura/issues/73) [LOG-23759](https://mezmo.atlassian.net/browse/LOG-23759)* **orchestration**: grade escalated workers as failed [c188acf](https://github.com/mezmo/aura/commit/c188acf53c24c4f13ee7e062deba187273e1626d) - Mike Shearer [LOG-23735](https://mezmo.atlassian.net/browse/LOG-23735)* **orchestration**: hide vestigial tool_calls, warn on text reuse [66ab985](https://github.com/mezmo/aura/commit/66ab9856e9ed6161e1bf121591bbb6d198c79048) - Mike Shearer [LOG-23615](https://mezmo.atlassian.net/browse/LOG-23615)* **orchestration**: persist routing tool response as planning output [81784b2](https://github.com/mezmo/aura/commit/81784b24c483aace920aa9e79e6dc3034260752f) - Mike Shearer [LOG-23549](https://mezmo.atlassian.net/browse/LOG-23549)* **orchestration**: prevent panic when memory_dir unset [90fdd10](https://github.com/mezmo/aura/commit/90fdd106bc5acdfeec40ca1e21f222f24a9186e4) - Mike Shearer [#line](https://github.com/mezmo/aura/issues/line) [LOG-23507](https://mezmo.atlassian.net/browse/LOG-23507)* **orchestration**: route MCP progress via factory pattern [5dec67d](https://github.com/mezmo/aura/commit/5dec67d554068a73649a18536cc63a37dd4121e0) - Mike Shearer [LOG-23565](https://mezmo.atlassian.net/browse/LOG-23565)* **orchestration**: soften overfitted prompt constraints [012db9c](https://github.com/mezmo/aura/commit/012db9c02435a1bd874e3ccefad532b117432948) - Mike Shearer [LOG-23612](https://mezmo.atlassian.net/browse/LOG-23612) [LOG-23619](https://mezmo.atlassian.net/browse/LOG-23619)* **orchestration**: stop double-counting tokens on agent.stream span [98a7470](https://github.com/mezmo/aura/commit/98a7470b50f30fc75c15a1c11ba25ad29cd7f446) - Mike Shearer [#73](https://github.com/mezmo/aura/issues/73) [LOG-23759](https://mezmo.atlassian.net/browse/LOG-23759)* **orchestration**: unify planning and execution in same iteration dir [7112385](https://github.com/mezmo/aura/commit/7112385c8b714b1d4c8facb55bc9ab770851ebe6) - Mike Shearer [LOG-23549](https://mezmo.atlassian.net/browse/LOG-23549)* **otel**: truncate oversized span data to stay under gRPC 4MB limit [ce391a5](https://github.com/mezmo/aura/commit/ce391a51204d385dc3633adf487f8b40af3f82c5) - Mike Shearer* **otel**: wrap force_flush in spawn_blocking to avoid deadlock [5ebb528](https://github.com/mezmo/aura/commit/5ebb5284bd73ed727acdda6b24df5bc2bcb25567) - Mike Shearer [LOG-23607](https://mezmo.atlassian.net/browse/LOG-23607)* prevent identical tool looping [dcad244](https://github.com/mezmo/aura/commit/dcad2442f60274ae5b32599f5de952b27ee66010) - Mike Shearer [LOG-23411](https://mezmo.atlassian.net/browse/LOG-23411)* **rebase**: post-rebase fixups for main merge compatibility [9ab4c2a](https://github.com/mezmo/aura/commit/9ab4c2ac13fba352e59438b82ae84ec712ac80f3) - Mike Shearer [LOG-23461](https://mezmo.atlassian.net/browse/LOG-23461)* reconcile rebase resolution across parallel work streams [69fe071](https://github.com/mezmo/aura/commit/69fe071fc9cbe9a49bf60363089ab7a9ff51ae4c) - Mike Shearer [#51](https://github.com/mezmo/aura/issues/51) [LOG-23351](https://mezmo.atlassian.net/browse/LOG-23351)* reflection planning prompt summary fix [91a4835](https://github.com/mezmo/aura/commit/91a48351fe21d4cd7fe3d1a5f5f86dcf59879bec) - Mike Shearer [LOG-23405](https://mezmo.atlassian.net/browse/LOG-23405)* remove phase-aware evaluation Qwen regression [7189575](https://github.com/mezmo/aura/commit/7189575a279fe52138814be8390d476c9f035386) - Mike Shearer [LOG-23252](https://mezmo.atlassian.net/browse/LOG-23252)* remove unused initial assignment in orchestration loop [e999c66](https://github.com/mezmo/aura/commit/e999c661f639f4b3b8db6e43ff14ba932da2f91e) - Mike Shearer [LOG-23358](https://mezmo.atlassian.net/browse/LOG-23358)* rig react loops continues to churt through planning [aec7996](https://github.com/mezmo/aura/commit/aec7996f02ae339cdc569778bb202b34fc8f086d) - Mike Shearer [LOG-23252](https://mezmo.atlassian.net/browse/LOG-23252)* **test**: update plan_created assertion for tasks array [7a10666](https://github.com/mezmo/aura/commit/7a1066647f08a94330f8a6657fa98056141ce960) - Mike Shearer [#63](https://github.com/mezmo/aura/issues/63) [LOG-23544](https://mezmo.atlassian.net/browse/LOG-23544)* **test**: update SRE plan_created assertion for tasks array [174c96d](https://github.com/mezmo/aura/commit/174c96db41a28d123084539358ca48a9f5545835) - Mike Shearer [LOG-23544](https://mezmo.atlassian.net/browse/LOG-23544)* **test**: use distinct multi-step queries in integration tests [d976e6e](https://github.com/mezmo/aura/commit/d976e6ebf8f350590d676de91ccf4adf6efe5b84) - Mike Shearer [LOG-23465](https://mezmo.atlassian.net/browse/LOG-23465)* **tracing**: capture per-turn token usage on orchestration spans [705eed0](https://github.com/mezmo/aura/commit/705eed0a3c956dd5b068c13c5e24fec79aaed27d) - Mike Shearer [LOG-23471](https://mezmo.atlassian.net/browse/LOG-23471)* **web**: single-config servers accept any model field value [03a4090](https://github.com/mezmo/aura/commit/03a40905ea9056bf9ee77337df4852dcead00bb6) - Mike Shearer [LOG-23501](https://mezmo.atlassian.net/browse/LOG-23501)* worker preamble use and error recovery guidence [0a59f99](https://github.com/mezmo/aura/commit/0a59f9921b0fce100b457b3d6c3b0fdaf8ff42d8) - Mike Shearer [LOG-21951](https://mezmo.atlassian.net/browse/LOG-21951)

### Chores

* adapt CI and tests for orchestration feature branch [74a9034](https://github.com/mezmo/aura/commit/74a90348dd5d492f4476d2a3ec08ba5f728193ec) - Mike Shearer* add sre test target to makefile [be369c3](https://github.com/mezmo/aura/commit/be369c307866502a161ca7ad1cf9194b6e38fd2e) - Mike Shearer [LOG-23252](https://mezmo.atlassian.net/browse/LOG-23252)* ads sre tests as feature in web cargo.toml [4deb895](https://github.com/mezmo/aura/commit/4deb8957f66b1477d477d4f21f8cc67e3a6c6976) - Mike Shearer [LOG-22753](https://mezmo.atlassian.net/browse/LOG-22753)* **cleanup**: remove todo_tool [b15e731](https://github.com/mezmo/aura/commit/b15e731f6dc0fdeaf39dab042a1059d3a2e5e8b7) - Mike Shearer [LOG-23553](https://mezmo.atlassian.net/browse/LOG-23553)* clippy fmt [b76b10e](https://github.com/mezmo/aura/commit/b76b10ea2a99aec99ebfcf7d514a417084cf8222) - Mike Shearer [LOG-22924](https://mezmo.atlassian.net/browse/LOG-22924)* **config**: moved llm specific configs [989f237](https://github.com/mezmo/aura/commit/989f237a2d12a72d196f52ecc3a31484f24bddd4) - Mike Shearer [LOG-23546](https://mezmo.atlassian.net/browse/LOG-23546)* enriched StreamingAgent to reduce main diff [25f3efe](https://github.com/mezmo/aura/commit/25f3efebb1a18943eb5b4bcd59a221554f91d709) - Mike Shearer [LOG-23252](https://mezmo.atlassian.net/browse/LOG-23252)* **eval**: remove stale scripts and tighten gitignore [852f2ab](https://github.com/mezmo/aura/commit/852f2ab4d3208ca883fd0be0b3a20f0c9d6b9913) - Mike Shearer [LOG-23461](https://mezmo.atlassian.net/browse/LOG-23461)* **eval**: rename temp-prompt-eval → e2e-eval [e229b8d](https://github.com/mezmo/aura/commit/e229b8deac4c5db9d303d6f0e961ef09654054af) - Mike Shearer [LOG-23461](https://mezmo.atlassian.net/browse/LOG-23461)* fix flakey tests that write to env vars [b8f83b3](https://github.com/mezmo/aura/commit/b8f83b3cc61c1178d784dead5758c381c74dac66) - Mike Shearer [LOG-23806](https://mezmo.atlassian.net/browse/LOG-23806)* gitignore accidentally-committed cluster test config [9822f07](https://github.com/mezmo/aura/commit/9822f070dd66288763facca8d015a006ea28e4bb) - Mike Shearer [LOG-23616](https://mezmo.atlassian.net/browse/LOG-23616) [LOG-23616](https://mezmo.atlassian.net/browse/LOG-23616)* gitignore session eval output files [bbf16c7](https://github.com/mezmo/aura/commit/bbf16c76ca3baec412581c3395957d39ebee60fe) - Mike Shearer [LOG-23470](https://mezmo.atlassian.net/browse/LOG-23470)* **orchestration**: drop dead tool_calls write path in persistence [5b217a6](https://github.com/mezmo/aura/commit/5b217a657d087f112c9568fd2727b65017e1ff40) - Mike Shearer [LOG-23616](https://mezmo.atlassian.net/browse/LOG-23616)* **orchestration**: remove dead code from trait cleanup [e2a82e4](https://github.com/mezmo/aura/commit/e2a82e435339ebec4cb2b165a37fc161eea9f225) - Mike Shearer [LOG-23565](https://mezmo.atlassian.net/browse/LOG-23565)* remove E2E eval suite and per-model configs [925807c](https://github.com/mezmo/aura/commit/925807c6a957a1ca62be2390cd47f6c35b6d9666) - Mike Shearer [LOG-23839](https://mezmo.atlassian.net/browse/LOG-23839)* remove old cli [4443fd4](https://github.com/mezmo/aura/commit/4443fd4a316026a1d18dd51fdfcae183fe75a7e3) - Mike Shearer [LOG-23311](https://mezmo.atlassian.net/browse/LOG-23311)* remove old vendored sanitiation and move to aura [d5f3d8b](https://github.com/mezmo/aura/commit/d5f3d8b03664fadeea6916f20758cc12f8033079) - Mike Shearer [LOG-23293](https://mezmo.atlassian.net/browse/LOG-23293)* **sse**: consolidate properties and cleanup docs [ed95285](https://github.com/mezmo/aura/commit/ed95285c6dc6784a3b6d6f4bfdf702c0df50305f) - Mike Shearer [LOG-23544](https://mezmo.atlassian.net/browse/LOG-23544)* update clippy rules to match main [3888583](https://github.com/mezmo/aura/commit/388858364193e41a154869a57c1f31aa7c19dd56) - Mike Shearer [LOG-22753](https://mezmo.atlassian.net/browse/LOG-22753)* update model configs for e2e examples [9017dde](https://github.com/mezmo/aura/commit/9017dde6925502e9ae23c9164c081e388c3e812f) - Mike Shearer [LOG-22815](https://mezmo.atlassian.net/browse/LOG-22815)

### Code Refactoring

* **orchestration**: add CallOutcome enum [30b8393](https://github.com/mezmo/aura/commit/30b839304baa2f1e398517fdd8ae53f710066b6a) - Mike Shearer [LOG-23743](https://mezmo.atlassian.net/browse/LOG-23743)* **orchestration**: eliminate lossy synthesis [6a74510](https://github.com/mezmo/aura/commit/6a745109a11ef4154b6ff6c4e5927210d7630405) - Mike Shearer [LOG-23635](https://mezmo.atlassian.net/browse/LOG-23635)* **orchestration**: remove Orchestrated variant and phase types [94c5ce7](https://github.com/mezmo/aura/commit/94c5ce7ac4fdbf482f3633c2f65235c45b278baa) - Mike Shearer [LOG-23616](https://mezmo.atlassian.net/browse/LOG-23616)* **orchestration**: remove phase execution runtime [2ac9952](https://github.com/mezmo/aura/commit/2ac9952bf9238ea36c3e64a915ad3e8eb0c5a764) - Mike Shearer [LOG-23616](https://mezmo.atlassian.net/browse/LOG-23616)* **orchestration**: tagged StepInput enum with ReuseTask variant [6d465e9](https://github.com/mezmo/aura/commit/6d465e95e2e4c238da232d828832479396c10a66) - Mike Shearer [LOG-23754](https://mezmo.atlassian.net/browse/LOG-23754)* **orchestration**: two-stage duplicate call guard [32c6fdf](https://github.com/mezmo/aura/commit/32c6fdfca77df75e1be6886268717a14ccabc03a) - Mike Shearer [LOG-23736](https://mezmo.atlassian.net/browse/LOG-23736)* **orchestration**: typed task state and failure classification [75df912](https://github.com/mezmo/aura/commit/75df9126e9bb2c4a2cda814fb496e797fcd90006) - Mike Shearer [LOG-23773](https://mezmo.atlassian.net/browse/LOG-23773)* **streaming**: thread request_id through stream trait [107e581](https://github.com/mezmo/aura/commit/107e5811216750a70c60c70fa6410588c717a591) - Mike Shearer [LOG-23565](https://mezmo.atlassian.net/browse/LOG-23565)* **webserver**: swap actix-web for axum [0c355d7](https://github.com/mezmo/aura/commit/0c355d79e0431cb2dfeddb86ce46440134f2d344) - Mike Shearer [LOG-23816](https://mezmo.atlassian.net/browse/LOG-23816)

### Documentation

* add open-alpha notice to README [fd4c9d8](https://github.com/mezmo/aura/commit/fd4c9d8206ed1179aecfd14aaea3dee16e8dabd8) - Mike Shearer [LOG-23358](https://mezmo.atlassian.net/browse/LOG-23358)* add orchestration events to streaming guide, fix stale references [0a0361b](https://github.com/mezmo/aura/commit/0a0361b245eedbecdfe6d776995adb8abc789c5b) - Mike Shearer [LOG-22815](https://mezmo.atlassian.net/browse/LOG-22815)* **orchestration**: fix stale comments and trim verbosity [9a54608](https://github.com/mezmo/aura/commit/9a546085f3773d0a1ad8fc15c55aa8247ec12a9b) - Mike Shearer [LOG-23565](https://mezmo.atlassian.net/browse/LOG-23565)* **orchestration**: scrub stale phase/Orchestrated references [78ca80b](https://github.com/mezmo/aura/commit/78ca80b434f9d7c89fd87561cfe9c9b939851991) - Mike Shearer [LOG-23616](https://mezmo.atlassian.net/browse/LOG-23616)* remove orchestration-flow-diagram from README docs list [380ae25](https://github.com/mezmo/aura/commit/380ae25c4a2896dc43e15d96d223bfbf9bdd6c7c) - Mike Shearer [LOG-23358](https://mezmo.atlassian.net/browse/LOG-23358)* rewrite README for orchestration workflows [05c38a5](https://github.com/mezmo/aura/commit/05c38a521b47e3e6161f6762b0ef920788bf8a86) - Mike Shearer [LOG-23358](https://mezmo.atlassian.net/browse/LOG-23358)

### Features

* add E2E model comparison scripts with loop detection [a85c318](https://github.com/mezmo/aura/commit/a85c31812ec7fb91ee9628abc0d548e1b6dadc02) - Mike Shearer [LOG-23448](https://mezmo.atlassian.net/browse/LOG-23448)* add GPT-5.1 thinking + Opus Bedrock configs [20cfd67](https://github.com/mezmo/aura/commit/20cfd67cbeaf4774cd2c8aa8ad966834bb2252e2) - Mike Shearer [LOG-23448](https://mezmo.atlassian.net/browse/LOG-23448)* **cli**: add aura-cli crate with HTTP + standalone backends [a6e15c0](https://github.com/mezmo/aura/commit/a6e15c026d148761f8fdcae07b2c8c8f46c31982) - Mike Shearer [LOG-23587](https://mezmo.atlassian.net/browse/LOG-23587)* **config**: nest llm under agent with per-worker overrides [f281f19](https://github.com/mezmo/aura/commit/f281f19b3829d2105322b7c06c21cef8093208f9) - Mike Shearer [LOG-23439](https://mezmo.atlassian.net/browse/LOG-23439) [LOG-23606](https://mezmo.atlassian.net/browse/LOG-23606)* **e2e-eval**: add plan execution analyzer [554e923](https://github.com/mezmo/aura/commit/554e9239c083022c7d3db706bb864c39fc295890) - Mike Shearer [LOG-23615](https://mezmo.atlassian.net/browse/LOG-23615) [LOG-23615](https://mezmo.atlassian.net/browse/LOG-23615)* enrich evaluation prompt with task execution evidence [f4011c0](https://github.com/mezmo/aura/commit/f4011c0232cec4aeea6ea416b5976c4787a5573c) - Mike Shearer [LOG-23425](https://mezmo.atlassian.net/browse/LOG-23425)* **eval**: dedupe sse parsing code [44c0c44](https://github.com/mezmo/aura/commit/44c0c447ea623949f21216f1b34d63fea87b9fab) - Mike Shearer [LOG-23552](https://mezmo.atlassian.net/browse/LOG-23552)* **helm**: add structured YAML config rendering to TOML [31aea6c](https://github.com/mezmo/aura/commit/31aea6c1d2aa5dacb06f85460d1d75528deb5eca) - Mike Shearer [LOG-23231](https://mezmo.atlassian.net/browse/LOG-23231)* orchestration mode — multi-agent task decomposition and synthesis [d8d5749](https://github.com/mezmo/aura/commit/d8d5749d62f96c8d8fc547da5eb359c687fa2ad8) - Mike Shearer [LOG-21951](https://mezmo.atlassian.net/browse/LOG-21951)* **orchestration**: add OTel tracing spans to orchestration mode [93727b6](https://github.com/mezmo/aura/commit/93727b6b2a35326498b3afa69829d853974f9115) - Mike Shearer [LOG-23471](https://mezmo.atlassian.net/browse/LOG-23471)* **orchestration**: add reuse_result_from to StepInput::LeafTask [f92f8aa](https://github.com/mezmo/aura/commit/f92f8aaef129a3050908be7cedb304be6a8ba18b) - Mike Shearer [LOG-23615](https://mezmo.atlassian.net/browse/LOG-23615)* **orchestration**: artifact infrastructure [6138d92](https://github.com/mezmo/aura/commit/6138d92dff4013339f66729ef484566d38a6adc5) - Mike Shearer [LOG-23607](https://mezmo.atlassian.net/browse/LOG-23607) [LOG-23776](https://mezmo.atlassian.net/browse/LOG-23776)* **orchestration**: coordinator inspection tools and manifest [2750391](https://github.com/mezmo/aura/commit/275039165cf50613351e3aeaca9634d04897d654) - Mike Shearer [LOG-23620](https://mezmo.atlassian.net/browse/LOG-23620)* **orchestration**: coordinator session context injection (LOG-23470) [dc7d99f](https://github.com/mezmo/aura/commit/dc7d99f3ad571064051fe9490201f1aa2b63ae5c) - Mike Shearer [LOG-23470](https://mezmo.atlassian.net/browse/LOG-23470) [LOG-23470](https://mezmo.atlassian.net/browse/LOG-23470)* **orchestration**: enrich replan events + readability [52f66dd](https://github.com/mezmo/aura/commit/52f66dd6306f6a62f035196a9f323fc8f6455100) - Mike Shearer [LOG-23458](https://mezmo.atlassian.net/browse/LOG-23458)* **orchestration**: explicit routing mode in SSE events [91f9aa4](https://github.com/mezmo/aura/commit/91f9aa4a37f934cb0e63b9442c3a394161843154) - Mike Shearer [LOG-23500](https://mezmo.atlassian.net/browse/LOG-23500)* **orchestration**: observability, resilience, and prompt cleanup [17a065b](https://github.com/mezmo/aura/commit/17a065b326eda6d5850f3c7636a6a74a783838d5) - Mike Shearer [LOG-23607](https://mezmo.atlassian.net/browse/LOG-23607)* **orchestration**: per-worker reasoning attribution [a515d8f](https://github.com/mezmo/aura/commit/a515d8f953573cd7df2c7b094d0fdbd8708996d8) - Mike Shearer [LOG-23459](https://mezmo.atlassian.net/browse/LOG-23459)* **orchestration**: persistent coordinator conversation with ReAct loop [0df56b9](https://github.com/mezmo/aura/commit/0df56b9c521f4e4d07eab4882e9e9fef45d18af7) - Mike Shearer [LOG-23607](https://mezmo.atlassian.net/browse/LOG-23607) [LOG-23776](https://mezmo.atlassian.net/browse/LOG-23776) [LOG-23800](https://mezmo.atlassian.net/browse/LOG-23800) [LOG-23801](https://mezmo.atlassian.net/browse/LOG-23801)* **orchestration**: session history eval framework [15d5ee2](https://github.com/mezmo/aura/commit/15d5ee2b4865ec8ddf923e4061bd09ac1c8bd140) - Mike Shearer [LOG-23470](https://mezmo.atlassian.net/browse/LOG-23470)* **orchestration**: session-scoped persistence + RunManifest [5a0d52e](https://github.com/mezmo/aura/commit/5a0d52e008aff04dcf4e749b949a131d9e176e63) - Mike Shearer [LOG-23461](https://mezmo.atlassian.net/browse/LOG-23461)* **orchestration**: smart replan gating [d5202f0](https://github.com/mezmo/aura/commit/d5202f0916d6cc238fc330708a2b5fafe70ff6f0) - Mike Shearer [LOG-23465](https://mezmo.atlassian.net/browse/LOG-23465)* **orchestration**: tool reasoning surface and frame validation [dcca56c](https://github.com/mezmo/aura/commit/dcca56c9ec38d2a102c13b273aee8a624247aeb7) - Mike Shearer [LOG-23621](https://mezmo.atlassian.net/browse/LOG-23621)* persist original steps in plan.json and add Ollama config [5d71482](https://github.com/mezmo/aura/commit/5d71482c6d4386ed8a849eea0e1e5b4b7ac9b3f4) - Mike Shearer [LOG-23434](https://mezmo.atlassian.net/browse/LOG-23434)* phased planning base execution loop [4109622](https://github.com/mezmo/aura/commit/41096224059fe9db92e6c2b0b0f1597bc2de3b05) - Mike Shearer [LOG-23252](https://mezmo.atlassian.net/browse/LOG-23252)* phased planning refinements [863caa5](https://github.com/mezmo/aura/commit/863caa526ae77ceeb4e46849e5416fe9a9612b59) - Mike Shearer [LOG-23252](https://mezmo.atlassian.net/browse/LOG-23252)* phased planning SSE events [720d935](https://github.com/mezmo/aura/commit/720d935b8c8289b2ffe8484284217fc5902a0e14) - Mike Shearer [LOG-23252](https://mezmo.atlassian.net/browse/LOG-23252)* **scratchpad**: context window management for large MCP tool outputs [2d000d1](https://github.com/mezmo/aura/commit/2d000d17f2910e0bfee9dd230da366997cef3dc1) - Mike Shearer [LOG-23439](https://mezmo.atlassian.net/browse/LOG-23439)* sequential-by-default steps plan schema for local models [d64df60](https://github.com/mezmo/aura/commit/d64df60a10db97d3f7d27f95359ee7ea3581f106) - Mike Shearer [LOG-23434](https://mezmo.atlassian.net/browse/LOG-23434)* stream worker/synthesis/phase-continuation [91d1585](https://github.com/mezmo/aura/commit/91d15850f30f94ea2d72707e60809e5439993177) - Mike Shearer [LOG-23435](https://mezmo.atlassian.net/browse/LOG-23435)* types for phased planning [cf38430](https://github.com/mezmo/aura/commit/cf384306215c23edbf3cced72d924630dbd02209) - Mike Shearer [LOG-23252](https://mezmo.atlassian.net/browse/LOG-23252)* used heirarchical vs dag planning [1327054](https://github.com/mezmo/aura/commit/132705430a0390c21e28fa945f3077cd89d9e029) - Mike Shearer [LOG-23434](https://mezmo.atlassian.net/browse/LOG-23434)

### Miscellaneous

* Merge pull request #119 from mezmo/feature/orchestration-mode [b39d7dc](https://github.com/mezmo/aura/commit/b39d7dc59430b905a21f12cb45cd370205444ce2) - GitHub [#119](https://github.com/mezmo/aura/issues/119)

### Style

* **orchestration**: cargo fmt after bulk test deletions [fa68ad8](https://github.com/mezmo/aura/commit/fa68ad8b8a5240fea463e7fb7203f646e637cace) - Mike Shearer [LOG-23616](https://mezmo.atlassian.net/browse/LOG-23616)

### Tests

* add SRE orchestration integration tests [0a19315](https://github.com/mezmo/aura/commit/0a19315705df643ed35f64b2a50041fcf049a177) - Mike Shearer [LOG-22753](https://mezmo.atlassian.net/browse/LOG-22753)* **orchestration**: add progress notification integration test [b3dd6b3](https://github.com/mezmo/aura/commit/b3dd6b354bf8b76a45eecf7fc2b81e04e1e0f518) - Mike Shearer [LOG-23565](https://mezmo.atlassian.net/browse/LOG-23565)* **orchestration**: add test coverage for replan gating [5d1cad6](https://github.com/mezmo/aura/commit/5d1cad61e58cb45e6aa8841f7efd919241a4c4ad) - Mike Shearer [LOG-23465](https://mezmo.atlassian.net/browse/LOG-23465)* **orchestration**: cover enforce_routing_config StepsPlan rewrite [e65f813](https://github.com/mezmo/aura/commit/e65f813d08e34daa69db6b00e4cc1e53f165efde) - Mike Shearer [LOG-23616](https://mezmo.atlassian.net/browse/LOG-23616)

## [1.19.8](https://github.com/mezmo/aura/compare/v1.19.7...v1.19.8) (2026-05-13)


### Bug Fixes

* **ci**: wrap release step with appropiate credentials [e8733e8](https://github.com/mezmo/aura/commit/e8733e8c71565c668a3f98f7905a4568cd0641e1) - Eric Satterwhite

### Chores

* **ci**: migrate to oss worker pool [5f24d1f](https://github.com/mezmo/aura/commit/5f24d1f28f24d502a61ed57795901fc259c0e887) - Eric Satterwhite

### Service

* **setup**: add .cargo to gitignore [a9a3a6d](https://github.com/mezmo/aura/commit/a9a3a6d1476360009a35a92c38f86fcc68d0109c) - Eric Satterwhite* **setup**: make a correction to the build tag variable scope [d74d26a](https://github.com/mezmo/aura/commit/d74d26a4fae6bb6f5f26a69699b36f98cf1245fc) - Eric Satterwhite* **setup**: remove erroneously commited .cargo directory [0a04f60](https://github.com/mezmo/aura/commit/0a04f60f6a5a23fa15c816f4112ba140909f51ac) - Eric Satterwhite

## [1.19.7](https://github.com/mezmo/aura/compare/v1.19.6...v1.19.7) (2026-05-11)


### Chores

* **build**: simplify multistage docker setup [d099a49](https://github.com/mezmo/aura/commit/d099a492f722086e4b751b3dfd23e1f6d457a436) - Mike Shearer

### Service

* **setup**: add rich commitlint reporting [10c4cd4](https://github.com/mezmo/aura/commit/10c4cd443f053dbfc019c55919836815c42b3530) - Mike Shearer* **setup**: add strcutured test and coverage reporting [cb233a5](https://github.com/mezmo/aura/commit/cb233a55e259fe0758914cde12ca36cc228f0830) - Mike Shearer* **setup**: docker based test setup via nextest [83a4a7e](https://github.com/mezmo/aura/commit/83a4a7e0d2a3d51eaf9da0c575da8afa90c24e6b) - Mike Shearer [LOG-1791](https://mezmo.atlassian.net/browse/LOG-1791)* **setup**: remove old development directory [db4b760](https://github.com/mezmo/aura/commit/db4b7602bf95ab92fac28e6110d6f76bf0797f9e) - Mike Shearer* **setup**: remove test-ci scripts and reference [2e45520](https://github.com/mezmo/aura/commit/2e455209134c8869486f49031ee0eb96c800cbb9) - Mike Shearer

### Style

* apply cargo fmt [db33c3a](https://github.com/mezmo/aura/commit/db33c3ab0adc33c8bb4e8938101b1618788f7f80) - Mike Shearer

## [1.19.6](https://github.com/mezmo/aura/compare/v1.19.5...v1.19.6) (2026-05-09)


### Chores

* **cla**: Allow Promptess to pass CLA [3a8e790](https://github.com/mezmo/aura/commit/3a8e79006426bb3438e2d825c76c0f83165506f0) - Gregory Janco [LOG-23837](https://mezmo.atlassian.net/browse/LOG-23837)

## [1.19.5](https://github.com/mezmo/aura/compare/v1.19.4...v1.19.5) (2026-05-08)


### Chores

* **cla**: Allow promptless PRs [a956848](https://github.com/mezmo/aura/commit/a9568488d7a4d03e1affb5ecd8c74469ab69ac02) - Gregory Janco [LOG-23837](https://mezmo.atlassian.net/browse/LOG-23837)

## [1.19.4](https://github.com/mezmo/aura/compare/v1.19.3...v1.19.4) (2026-05-08)


### Chores

* Rename Aura to AURA [f381444](https://github.com/mezmo/aura/commit/f38144487429cbc955013a4f637d7e4f8e333766) - Gregory Janco [LOG-23836](https://mezmo.atlassian.net/browse/LOG-23836)

## [1.19.3](https://github.com/mezmo/aura/compare/v1.19.2...v1.19.3) (2026-05-06)


### Bug Fixes

* **internal**: remove stale k8s artifacts [89aac80](https://github.com/mezmo/aura/commit/89aac80865fcec48d6fd906add905a131d8e80d1) - Phil Ciampini

### Documentation

* reframe readme intro as sre agentic harness [e6eb139](https://github.com/mezmo/aura/commit/e6eb13971d1e5aa6607949c752ff8919e448f436) - Andre Elizondo [LOG-000000](https://mezmo.atlassian.net/browse/LOG-000000)

## [1.19.2](https://github.com/mezmo/aura/compare/v1.19.1...v1.19.2) (2026-05-01)


### Chores

* **setup**: rework ci and release setup [a5b7a0d](https://github.com/mezmo/aura/commit/a5b7a0d449481f21184274cb7a469f7bc8dd084e) - Eric Satterwhite [LOG-23601](https://mezmo.atlassian.net/browse/LOG-23601)* **setup**: update commitlint setup [41cab5f](https://github.com/mezmo/aura/commit/41cab5f790ddd08ce65762eba99b2fe92ef5acdc) - Eric Satterwhite [LOG-23601](https://mezmo.atlassian.net/browse/LOG-23601)

### Style

* **lint**: fix deployment yaml to pass lint [4180b87](https://github.com/mezmo/aura/commit/4180b8794798f3c15df39a13e31a314fabed06e6) - Eric Satterwhite [LOG-23601](https://mezmo.atlassian.net/browse/LOG-23601)

## [1.19.1](https://github.com/mezmo/aura/compare/v1.19.0...v1.19.1) (2026-04-23)


### Chores

* **deps**: bump rig-core to d7e9d92 [8fc66e7](https://github.com/mezmo/aura/commit/8fc66e7f1e5de9954e2ee839b3a6849de679f38e) - Mike Shearer [LOG-23732](https://logdna.atlassian.net/browse/LOG-23732)

# [1.19.0](https://github.com/mezmo/aura/compare/v1.18.1...v1.19.0) (2026-04-22)


### Bug Fixes

* rewrite metrics_analyst preamble to stop loop [224f79f](https://github.com/mezmo/aura/commit/224f79fba8193522c379d0de35f1ee1be68b256f) - Andre Elizondo [LOG-00000](https://logdna.atlassian.net/browse/LOG-00000)


### Features

* add Kubernetes SRE orchestration quickstart [ee0f746](https://github.com/mezmo/aura/commit/ee0f74698aeb32911ef0ca46120cf9d313acc833) - Andre Elizondo [LOG-00000](https://logdna.atlassian.net/browse/LOG-00000)

## [1.18.1](https://github.com/mezmo/aura/compare/v1.18.0...v1.18.1) (2026-04-21)


### Documentation

* add commit and contribution rules to claude.md [94e52ad](https://github.com/mezmo/aura/commit/94e52adbf2099ce6033e67e3a4288cc8defd3378) - Andre Elizondo [LOG-000000](https://logdna.atlassian.net/browse/LOG-000000)

# [1.18.0](https://github.com/mezmo/aura/compare/v1.17.3...v1.18.0) (2026-04-20)


### Bug Fixes

* **bedrock**: collapse redundant default_ndims branches [01835b3](https://github.com/mezmo/aura/commit/01835b3896477221de204683045a1140a6f52ace) - Mike Shearer [LOG-00000](https://logdna.atlassian.net/browse/LOG-00000)


### Code Refactoring

* **config**: replace VectorStoreConfig flat struct with tagged enum [6ff3894](https://github.com/mezmo/aura/commit/6ff3894c3316a96786a4ca86b084681ac27218f6) - Mike Shearer [LOG-00000](https://logdna.atlassian.net/browse/LOG-00000)


### Documentation

* document Bedrock embedding and Knowledge Base support [22ad872](https://github.com/mezmo/aura/commit/22ad872947944cbebeb95b700d5b2fcfc7112528) - Mike Shearer [LOG-00000](https://logdna.atlassian.net/browse/LOG-00000)


### Features

* add Bedrock embeddings and Knowledge Base support [9ddcb0c](https://github.com/mezmo/aura/commit/9ddcb0c4e5840ebf12ac416fb30966e4373a30f4) - Mike Shearer [LOG-00000](https://logdna.atlassian.net/browse/LOG-00000)
* **bedrock**: native embedding path bypassing rig-bedrock [bf3bfcc](https://github.com/mezmo/aura/commit/bf3bfcc2259a25861e6e28c7cdeec05cc1ede8df) - Mike Shearer [LOG-23628](https://logdna.atlassian.net/browse/LOG-23628)

## [1.17.3](https://github.com/mezmo/aura/compare/v1.17.2...v1.17.3) (2026-04-07)


### Chores

* **build**: Create custom check [3540da3](https://github.com/mezmo/aura/commit/3540da329e770588eb921bbebedd971812bafeb9) - Gregory Janco [LOG-23558](https://logdna.atlassian.net/browse/LOG-23558)

## [1.17.2](https://github.com/mezmo/aura/compare/v1.17.1...v1.17.2) (2026-04-06)


### Chores

* **errors**: add hint to agent name collision [8baac1c](https://github.com/mezmo/aura/commit/8baac1c54779b3abcfcd63c0ea366ca5921fcaca) - Justin Gross [LOG-23571](https://logdna.atlassian.net/browse/LOG-23571)

## [1.17.1](https://github.com/mezmo/aura/compare/v1.17.0...v1.17.1) (2026-04-03)


### Continuous Integration

* **deploy**: add multi-config directory support [751129f](https://github.com/mezmo/aura/commit/751129fec9d622b6563374305ceb298c9fc27d47) - Tony Rogers [LOG-23574](https://logdna.atlassian.net/browse/LOG-23574)

# [1.17.0](https://github.com/mezmo/aura/compare/v1.16.4...v1.17.0) (2026-04-03)


### Features

* **examples**: add orchestration mode quickstart with math MCP [6ec5f21](https://github.com/mezmo/aura/commit/6ec5f2164e347dc9caef4352702a31ba4d721642) - Gregory Janco [LOG-00000](https://logdna.atlassian.net/browse/LOG-00000)

## [1.16.4](https://github.com/mezmo/aura/compare/v1.16.3...v1.16.4) (2026-04-03)


### Bug Fixes

* **build**: change dry run parameters [f9a5b4d](https://github.com/mezmo/aura/commit/f9a5b4d0752c4f7fbf840dec549e0246479e24f8) - Gregory Janco [LOG-23558](https://logdna.atlassian.net/browse/LOG-23558)

## [1.16.3](https://github.com/mezmo/aura/compare/v1.16.2...v1.16.3) (2026-03-27)


### Bug Fixes

* **ci**: clean up stale containers before integration tests [6a1d67a](https://github.com/mezmo/aura/commit/6a1d67a9d0d2d2d598acee4ab0bd682e98c7beb2) - Mike Shearer [LOG-23413](https://logdna.atlassian.net/browse/LOG-23413)
* **quickstart**: simplify provider config and fix UX issues [e10b2e3](https://github.com/mezmo/aura/commit/e10b2e370d2456878af24842474761a4483815e5) - Mike Shearer [LOG-23503](https://logdna.atlassian.net/browse/LOG-23503)

## [1.16.2](https://github.com/mezmo/aura/compare/v1.16.1...v1.16.2) (2026-03-25)


### Documentation

* Update Readme for quickstart link [91755ef](https://github.com/mezmo/aura/commit/91755efceb73315eb91b5bac6d5eb400c4ae2df3) - Terry Moore [LOG-23494](https://logdna.atlassian.net/browse/LOG-23494)

## [1.16.1](https://github.com/mezmo/aura/compare/v1.16.0...v1.16.1) (2026-03-25)


### Bug Fixes

* **jenkins**: fix intermittent ci failures due to instance re-use [b1cc8bb](https://github.com/mezmo/aura/commit/b1cc8bb8e897ebc1f4d17632b42e47561285246c) - Justin Gross [LOG-23413](https://logdna.atlassian.net/browse/LOG-23413)

# [1.16.0](https://github.com/mezmo/aura/compare/v1.15.2...v1.16.0) (2026-03-23)


### Features

* print the version of aura web server at start [19019c4](https://github.com/mezmo/aura/commit/19019c4003cbcf27afe0a6ea3613a3a7e20434b3) - Justin Gross [LOG-22963](https://logdna.atlassian.net/browse/LOG-22963)

## [1.15.2](https://github.com/mezmo/aura/compare/v1.15.1...v1.15.2) (2026-03-17)


### Tests

* **dep**: reference mock-mcp from docker [8623f01](https://github.com/mezmo/aura/commit/8623f01270c561c08eb33a5f2188d49a19b07ddf) - Gregory Janco [LOG-23406](https://logdna.atlassian.net/browse/LOG-23406)

## [1.15.1](https://github.com/mezmo/aura/compare/v1.15.0...v1.15.1) (2026-03-17)


### Documentation

* use inline table syntax for embedding_model in reference config [6d6b102](https://github.com/mezmo/aura/commit/6d6b102f9a3988a70a9d822dc087eb9dbd5cf4f6) - Mike Shearer [LOG-23445](https://logdna.atlassian.net/browse/LOG-23445)

# [1.15.0](https://github.com/mezmo/aura/compare/v1.14.13...v1.15.0) (2026-03-12)


### Features

* add support for aliased agents listed/selected via /v1/models [4530733](https://github.com/mezmo/aura/commit/4530733e0726886b044158652a7abcc21d2bd0e0) - Justin Gross [LOG-22925](https://logdna.atlassian.net/browse/LOG-22925)

## [1.14.13](https://github.com/mezmo/aura/compare/v1.14.12...v1.14.13) (2026-03-12)


### Chores

* **build**: allow feature branch name flexibility [8f37182](https://github.com/mezmo/aura/commit/8f37182db9e71e3e1dcb5ef248932f6a63e9f140) - Gregory Janco [LOG-23418](https://logdna.atlassian.net/browse/LOG-23418)
* **context-window**: use TOML configured window not mapping [a283973](https://github.com/mezmo/aura/commit/a283973d6469a5468b6b3692a2f149dee7ed57f3) - Justin Gross [LOG-23394](https://logdna.atlassian.net/browse/LOG-23394)

## [1.14.12](https://github.com/mezmo/aura/compare/v1.14.11...v1.14.12) (2026-03-11)


### Chores

* **project**: change from aura-oss to aura [08a5eed](https://github.com/mezmo/aura/commit/08a5eed5471ff6dec49263589b9950d96d0fdb57) - Gregory Janco [LOG-23283](https://logdna.atlassian.net/browse/LOG-23283)

## [1.14.11](https://github.com/mezmo/aura/compare/v1.14.10...v1.14.11) (2026-03-11)


### Bug Fixes

* strip system role and empty messages [3cf3580](https://github.com/mezmo/aura/commit/3cf3580a3b8f952aa684588fed24d33ca33b5982) - Mike Shearer [LOG-23402](https://logdna.atlassian.net/browse/LOG-23402)

## [1.14.10](https://github.com/mezmo/aura/compare/v1.14.9...v1.14.10) (2026-03-11)


### Chores

* **build**: remove GH actions for Jenkins [8be0576](https://github.com/mezmo/aura/commit/8be0576d3eb6ea7eb1209d3df661f09b75732e79) - Gregory Janco [LOG-23283](https://logdna.atlassian.net/browse/LOG-23283)

## [1.14.9](https://github.com/mezmo/aura/compare/v1.14.8...v1.14.9) (2026-03-11)


### Bug Fixes

* stop breaking early on pre tool text [1c73989](https://github.com/mezmo/aura/commit/1c739897ee006f912e46728ed11eb540388d1675) - Mike Shearer [LOG-23401](https://logdna.atlassian.net/browse/LOG-23401)

## [1.14.8](https://github.com/mezmo/aura/compare/v1.14.7...v1.14.8) (2026-03-10)


### Bug Fixes

* **ci**: use public mezmo/aura-mock-mcp for tests [670c02e](https://github.com/mezmo/aura/commit/670c02edfd5865ccb554e47f84857130274149c0) - Dominic McAllister [LOG-23365](https://logdna.atlassian.net/browse/LOG-23365)
* **tests**: target available build [c716d81](https://github.com/mezmo/aura/commit/c716d81b7b0d3395173eb5a8d2a71d673cc15597) - Dominic McAllister [LOG-23391](https://logdna.atlassian.net/browse/LOG-23391)


### Chores

* **ci**: build container with GitHub Actions [73b3442](https://github.com/mezmo/aura/commit/73b3442b6ff0098873a519653e67ba066fde67d2) - Kristof Mattei
* Rust 1.93.1, and run Clippy [755da80](https://github.com/mezmo/aura/commit/755da80975aceb11f9e89e7720a5ba92a2208109) - Kristof Mattei

## [1.14.7](https://github.com/mezmo/aura/compare/v1.14.6...v1.14.7) (2026-03-05)


### Documentation

* add quickstart with Docker Compose (AURA + LibreChat + Phoenix) [8081471](https://github.com/mezmo/aura/commit/80814713cc8277b43c11785e40d660002d2024b6) - Gregory Janco [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)

## [1.14.6](https://github.com/mezmo/aura/compare/v1.14.5...v1.14.6) (2026-03-04)


### Chores

* **release**: fix the release version numbers [baf28be](https://github.com/mezmo/aura/commit/baf28becab4470ba5bbd05573810dddb9dd633d5) - Justin Gross

# 1.0.0 (2026-03-04)


### Chores

* **release**: 1.14.3 [skip ci] [3a86d8b](https://github.com/mezmo/aura/commit/3a86d8b6fa3242e2a178ed1fc2123b4e7107ab4e) - LogDNA Bot [LOG-23309](https://logdna.atlassian.net/browse/LOG-23309) [LOG-23309](https://logdna.atlassian.net/browse/LOG-23309)
* **release**: 1.14.4 [skip ci] [b539aa0](https://github.com/mezmo/aura/commit/b539aa0270593c328444a7d4ec5eb1037a2000a3) - LogDNA Bot [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* **release**: 1.14.4 [skip ci] [627664a](https://github.com/mezmo/aura/commit/627664a0f555e0ca5bb86987fc1efe949aead05c) - LogDNA Bot [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* **release**: 1.14.4 [skip ci] [cd11efd](https://github.com/mezmo/aura/commit/cd11efd64fa665a45b61acd5bc132f520edb3a4c) - LogDNA Bot [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* **release**: 1.14.4 [skip ci] [f7038f1](https://github.com/mezmo/aura/commit/f7038f1f6287c39be53e144779760bee39ed5b49) - Mike Shearer [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349)
* **release**: 1.14.5 [skip ci] [b0e6eac](https://github.com/mezmo/aura/commit/b0e6eac816c4f15cfdb100244c128680f517920b) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* **release**: bump the version to the correct version [bd3efec](https://github.com/mezmo/aura/commit/bd3efec32d285e632994629c2005905cc38022f0) - Justin Gross
* **release**: initialize repo [f183e13](https://github.com/mezmo/aura/commit/f183e137cf37110216428f7eacaf559b5ec7517d) - Mike Shearer
* **release**: trigger first automated build/release [91b3059](https://github.com/mezmo/aura/commit/91b30591816fc453ba1e0af77384dc727e8b7938) - Justin Gross [LOG-23309](https://logdna.atlassian.net/browse/LOG-23309)


### Documentation

* add Apache 2.0 license and update project metadata [388b3f1](https://github.com/mezmo/aura/commit/388b3f16be05c3b0d224f92647bcf366dadf5f9a) - Mike Shearer [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349)
* add open alpha notice and orchestration branch reference [35dcd17](https://github.com/mezmo/aura/commit/35dcd1792d94805079c7db8212a0c6134015f96a) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* development prereq and librechat model default [6e3883a](https://github.com/mezmo/aura/commit/6e3883aacb32f355a7c48cb2e9be6ee53fb92d56) - Gregory Janco [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* move configs/ to examples/ and reorganize [b6ac194](https://github.com/mezmo/aura/commit/b6ac1942f707d5d1c9eb9b023135784284d15c56) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* rename providers/ to minimal/ and agents/ to complete/ [38f506d](https://github.com/mezmo/aura/commit/38f506d4aaaf2ce545776b1b26f77c6cc955ff4b) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* restructure configs/ for open source [d2e96d8](https://github.com/mezmo/aura/commit/d2e96d877a1ce6f6f99c8fdedca19d1f6acafb67) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* update README orchestration callout for open alpha [ce89789](https://github.com/mezmo/aura/commit/ce89789a4de7835c54288bf19837f8ed0702c7a8) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)


### Miscellaneous

* add contribution guidelines, CLA and code of conduct [e85f2ea](https://github.com/mezmo/aura/commit/e85f2eabe20ff3439e64cbdff518d75fb11b6c85) - Justin Gross [LOG-23350](https://logdna.atlassian.net/browse/LOG-23350)

## [1.14.4](https://github.com/mezmo/aura/compare/v1.14.3...v1.14.4) (2026-03-04)


### Chores

* **release**: 1.14.4 [skip ci] [9a61ba3](https://github.com/mezmo/aura/commit/9a61ba3f64644736384fb9a0d2aea809a1026d2a) - LogDNA Bot [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* **release**: 1.14.4 [skip ci] [1ce3dd2](https://github.com/mezmo/aura/commit/1ce3dd259b6e1395bb18b5db50203f1281b29766) - LogDNA Bot [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* **release**: 1.14.4 [skip ci] [51fade9](https://github.com/mezmo/aura/commit/51fade965bd8031572c3e454df0c671cecda257d) - Mike Shearer [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349)
* **release**: 1.14.5 [skip ci] [84bc8d3](https://github.com/mezmo/aura/commit/84bc8d3e7d5bbb25a2df3a50e384c690951d71c7) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)


### Documentation

* add Apache 2.0 license and update project metadata [a95c39c](https://github.com/mezmo/aura/commit/a95c39c35aa451408a22422907345c448773d3ee) - Mike Shearer [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349)
* add open alpha notice and orchestration branch reference [cb42e54](https://github.com/mezmo/aura/commit/cb42e54fc17614913f2f7d3e884beafda2666e0d) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* development prereq and librechat model default [4939eee](https://github.com/mezmo/aura/commit/4939eee9bdc7ce791dca347591033e61871c1a1b) - Gregory Janco [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* move configs/ to examples/ and reorganize [05bd173](https://github.com/mezmo/aura/commit/05bd17347f12daadad6e88170695df1898150e3d) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* rename providers/ to minimal/ and agents/ to complete/ [023ab2b](https://github.com/mezmo/aura/commit/023ab2bfed2a6f97f8ef28b51a803a2b81a029cd) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* restructure configs/ for open source [c09c2fa](https://github.com/mezmo/aura/commit/c09c2fa94c7b348120e922aa237880ab50f4ece6) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* update README orchestration callout for open alpha [d8f10e0](https://github.com/mezmo/aura/commit/d8f10e0d7af5666df67daee11c5828691d831528) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)

## [1.14.4](https://github.com/mezmo/aura/compare/v1.14.3...v1.14.4) (2026-03-04)


### Chores

* **release**: 1.14.4 [skip ci] [1ce3dd2](https://github.com/mezmo/aura/commit/1ce3dd259b6e1395bb18b5db50203f1281b29766) - LogDNA Bot [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* **release**: 1.14.4 [skip ci] [51fade9](https://github.com/mezmo/aura/commit/51fade965bd8031572c3e454df0c671cecda257d) - Mike Shearer [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349)
* **release**: 1.14.5 [skip ci] [84bc8d3](https://github.com/mezmo/aura/commit/84bc8d3e7d5bbb25a2df3a50e384c690951d71c7) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)


### Documentation

* add Apache 2.0 license and update project metadata [a95c39c](https://github.com/mezmo/aura/commit/a95c39c35aa451408a22422907345c448773d3ee) - Mike Shearer [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349)
* add open alpha notice and orchestration branch reference [cb42e54](https://github.com/mezmo/aura/commit/cb42e54fc17614913f2f7d3e884beafda2666e0d) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* move configs/ to examples/ and reorganize [05bd173](https://github.com/mezmo/aura/commit/05bd17347f12daadad6e88170695df1898150e3d) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* rename providers/ to minimal/ and agents/ to complete/ [023ab2b](https://github.com/mezmo/aura/commit/023ab2bfed2a6f97f8ef28b51a803a2b81a029cd) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* restructure configs/ for open source [c09c2fa](https://github.com/mezmo/aura/commit/c09c2fa94c7b348120e922aa237880ab50f4ece6) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* update README orchestration callout for open alpha [d8f10e0](https://github.com/mezmo/aura/commit/d8f10e0d7af5666df67daee11c5828691d831528) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)

## [1.14.4](https://github.com/mezmo/aura/compare/v1.14.3...v1.14.4) (2026-03-03)


### Chores

* **release**: 1.14.4 [skip ci] [51fade9](https://github.com/mezmo/aura/commit/51fade965bd8031572c3e454df0c671cecda257d) - Mike Shearer [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349) [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349)
* **release**: 1.14.5 [skip ci] [84bc8d3](https://github.com/mezmo/aura/commit/84bc8d3e7d5bbb25a2df3a50e384c690951d71c7) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815) [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)


### Documentation

* add Apache 2.0 license and update project metadata [a95c39c](https://github.com/mezmo/aura/commit/a95c39c35aa451408a22422907345c448773d3ee) - Mike Shearer [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349)
* add open alpha notice and orchestration branch reference [cb42e54](https://github.com/mezmo/aura/commit/cb42e54fc17614913f2f7d3e884beafda2666e0d) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* move configs/ to examples/ and reorganize [05bd173](https://github.com/mezmo/aura/commit/05bd17347f12daadad6e88170695df1898150e3d) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* rename providers/ to minimal/ and agents/ to complete/ [023ab2b](https://github.com/mezmo/aura/commit/023ab2bfed2a6f97f8ef28b51a803a2b81a029cd) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* restructure configs/ for open source [c09c2fa](https://github.com/mezmo/aura/commit/c09c2fa94c7b348120e922aa237880ab50f4ece6) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)

## [1.14.5](https://github.com/mezmo/aura/compare/v1.14.4...v1.14.5) (2026-03-03)


### Documentation

* add open alpha notice and orchestration branch reference [cb42e54](https://github.com/mezmo/aura/commit/cb42e54fc17614913f2f7d3e884beafda2666e0d) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)
* restructure configs/ for open source [c09c2fa](https://github.com/mezmo/aura/commit/c09c2fa94c7b348120e922aa237880ab50f4ece6) - Mike Shearer [LOG-22815](https://logdna.atlassian.net/browse/LOG-22815)


### Miscellaneous

* Merge branch 'main' of github.com:mezmo/aura [59b6bd5](https://github.com/mezmo/aura/commit/59b6bd55accd33df14cd3d1151fdb1e142ad867f) - Mike Shearer

## [1.14.4](https://github.com/mezmo/aura/compare/v1.14.3...v1.14.4) (2026-03-03)


### Documentation

* add Apache 2.0 license and update project metadata [a95c39c](https://github.com/mezmo/aura/commit/a95c39c35aa451408a22422907345c448773d3ee) - Mike Shearer [LOG-23349](https://logdna.atlassian.net/browse/LOG-23349)

## [1.14.3](https://github.com/mezmo/aura/compare/v1.14.2...v1.14.3) (2026-03-02)


### Chores

* **release**: trigger first automated build/release [f581889](https://github.com/mezmo/aura/commit/f58188936308873ec1087bc329ebf291eeb5009e) - Justin Gross [LOG-23309](https://logdna.atlassian.net/browse/LOG-23309)

## [1.14.3](https://github.com/answerbook/aura/compare/v1.14.2...v1.14.3) (2026-03-02)


### Documentation

* modernize README and add supporting docs (#85) [0541002](https://github.com/answerbook/aura/commit/05410024b40310d040396a5de78c6dbd64de3fba) - GitHub [LOG-23358](https://logdna.atlassian.net/browse/LOG-23358)

## [1.14.2](https://github.com/answerbook/aura/compare/v1.14.1...v1.14.2) (2026-03-02)


### Chores

* remove old cli [a122114](https://github.com/answerbook/aura/commit/a1221146320e53638cedc1e9bffc6725d0712048) - Mike Shearer [LOG-23311](https://logdna.atlassian.net/browse/LOG-23311)

## [1.14.1](https://github.com/answerbook/aura/compare/v1.14.0...v1.14.1) (2026-03-02)


### Bug Fixes

* hung string detection on provider error [9952d25](https://github.com/answerbook/aura/commit/9952d253d13ccca9f6ee450cae047c25e5431027) - Mike Shearer [LOG-23334](https://logdna.atlassian.net/browse/LOG-23334)

# [1.14.0](https://github.com/answerbook/aura/compare/v1.13.5...v1.14.0) (2026-02-27)


### Features

* **helm**: add structured YAML config rendering to TOML [a169632](https://github.com/answerbook/aura/commit/a16963246d419facffa6e216e96f27768654949c) - Tony Rogers [LOG-23231](https://logdna.atlassian.net/browse/LOG-23231)

## [1.13.5](https://github.com/answerbook/aura/compare/v1.13.4...v1.13.5) (2026-02-26)


### Bug Fixes

* show provider stream errors in SSE response [b85f051](https://github.com/answerbook/aura/commit/b85f0510b78646c5d728910f223975b11dcf195a) - Mike Shearer [LOG-23315](https://logdna.atlassian.net/browse/LOG-23315)

## [1.13.4](https://github.com/answerbook/aura/compare/v1.13.3...v1.13.4) (2026-02-24)


### Chores

* remove old vendored sanitiation and move to aura [2ff2e43](https://github.com/answerbook/aura/commit/2ff2e43b40b69a7e8ec1f0b4b24b5292335cc239) - Mike Shearer [LOG-23293](https://logdna.atlassian.net/browse/LOG-23293)

## [1.13.3](https://github.com/answerbook/aura/compare/v1.13.2...v1.13.3) (2026-02-24)


### Chores

* remove internal docs cruft [92e40e3](https://github.com/answerbook/aura/commit/92e40e349e36acf480ec3a78245f8927ed8aab06) - Mike Shearer [LOG-23288](https://logdna.atlassian.net/browse/LOG-23288)

## [1.13.2](https://github.com/answerbook/aura/compare/v1.13.1...v1.13.2) (2026-02-22)


### Chores

* **code-cleanup**: consolidate streaming/non-streaming handlers [84b59d1](https://github.com/answerbook/aura/commit/84b59d11edb5874dc2762c2b679f901d34810305) - Mike Shearer [LOG-23183](https://logdna.atlassian.net/browse/LOG-23183)
* use mezmo branch in rig fork [ff6db91](https://github.com/answerbook/aura/commit/ff6db912f40515f034d84865464b67a292859c33) - Mike Shearer [LOG-23285](https://logdna.atlassian.net/browse/LOG-23285)

## [1.13.1](https://github.com/answerbook/aura/compare/v1.13.0...v1.13.1) (2026-02-19)


### Bug Fixes

* SSE graceful shutdown [27649fd](https://github.com/answerbook/aura/commit/27649fd5f3829ca0d934dd26eb2fcb567bc49b65) - Mike Shearer [LOG-23232](https://logdna.atlassian.net/browse/LOG-23232)

# [1.13.0](https://github.com/answerbook/aura/compare/v1.12.1...v1.13.0) (2026-02-19)


### Bug Fixes

* instrument tokio::spawn with agent.stream span [a4c83cb](https://github.com/answerbook/aura/commit/a4c83cbb423630577bd0d0b63d47d73711f1c6fa) - Mike Shearer [LOG-22580](https://logdna.atlassian.net/browse/LOG-22580)
* remove strategy and user id from otel [241e0ce](https://github.com/answerbook/aura/commit/241e0cedf595d459e440c6984cc7bf51e8f62759) - Mike Shearer [LOG-22580](https://logdna.atlassian.net/browse/LOG-22580)
* resolve OTel shutdown deadlock and mark tool error spans [d22d381](https://github.com/answerbook/aura/commit/d22d381e3c237694c3399dc980585556f9c20918) - Mike Shearer [LOG-22580](https://logdna.atlassian.net/browse/LOG-22580)
* rustfmt and PR review feedback [b0b7b67](https://github.com/answerbook/aura/commit/b0b7b67c201ea813c58bef5ef52a60821ca6b72f) - Mike Shearer [LOG-22580](https://logdna.atlassian.net/browse/LOG-22580)


### Features

* add OTel tracing with OpenInference exporter [9587353](https://github.com/answerbook/aura/commit/9587353c2f4196d3205b0d8ab415c403476735ae) - Mike Shearer [LOG-22580](https://logdna.atlassian.net/browse/LOG-22580)
* better tool call tokens for OTEL [bc999f7](https://github.com/answerbook/aura/commit/bc999f7b8263de48f8444e7fd4fe9b46810b978f) - Mike Shearer [LOG-22580](https://logdna.atlassian.net/browse/LOG-22580)
* fix io values from root span [59cf26f](https://github.com/answerbook/aura/commit/59cf26f48d2c64d8da323adf7577022e01370494) - Mike Shearer [LOG-22580](https://logdna.atlassian.net/browse/LOG-22580)
* reclassify agent.turn as LLM span with output messages [f3e35e8](https://github.com/answerbook/aura/commit/f3e35e88da92601e869150397a646e3672cb2a10) - Mike Shearer [LOG-22580](https://logdna.atlassian.net/browse/LOG-22580)
* stream reasoning deltas and map turn.reasoning in exporter [be83e26](https://github.com/answerbook/aura/commit/be83e26f26758017e1c9a0c245b617c78c0d4385) - Mike Shearer [LOG-22580](https://logdna.atlassian.net/browse/LOG-22580)

## [1.12.1](https://github.com/answerbook/aura/compare/v1.12.0...v1.12.1) (2026-02-18)


### Chores

* update docstrings and docs pointing to pool and cache [d306449](https://github.com/answerbook/aura/commit/d3064492e0f10201aa5dc86cfd1888f0feecb3da) - Mike Shearer [LOG-23257](https://logdna.atlassian.net/browse/LOG-23257)

# [1.12.0](https://github.com/answerbook/aura/compare/v1.11.2...v1.12.0) (2026-02-17)


### Features

* **mcp-headers**: configure headers to be forwarded to mcps [ac858be](https://github.com/answerbook/aura/commit/ac858be1730b9212978caa6f23a1d7ea7a72f2a7) - Mike Shearer [LOG-23155](https://logdna.atlassian.net/browse/LOG-23155)

## [1.11.2](https://github.com/answerbook/aura/compare/v1.11.1...v1.11.2) (2026-02-12)


### Chores

* add Helm chart for AURA deployment [b81db21](https://github.com/answerbook/aura/commit/b81db213d2ed996cc011d9f51b37d18d554c2de0) - Mike Shearer [LOG-23231](https://logdna.atlassian.net/browse/LOG-23231)

## [1.11.1](https://github.com/answerbook/aura/compare/v1.11.0...v1.11.1) (2026-02-11)


### Chores

* Remove temporary files [139004a](https://github.com/answerbook/aura/commit/139004a53d8296b51a858a3fb21af00c478e4536) - Mike Shearer [LOG-23196](https://logdna.atlassian.net/browse/LOG-23196)

# [1.11.0](https://github.com/answerbook/aura/compare/v1.10.0...v1.11.0) (2026-02-09)


### Chores

* add LlmConfig::model_name() and shared SSE test utilities [cc5397a](https://github.com/answerbook/aura/commit/cc5397ac236c97e3f2ff8a62925e76700992a77f) - Mike Shearer [LOG-23158](https://logdna.atlassian.net/browse/LOG-23158)
* bump rig fork to fix ollama tool id and model name [0e36d1e](https://github.com/answerbook/aura/commit/0e36d1ec3e7cabdbc99f1498a38dcd8082cb35f1) - Mike Shearer [LOG-23081](https://logdna.atlassian.net/browse/LOG-23081)


### Features

* add num_ctx, num_predict for ollama [6e497bd](https://github.com/answerbook/aura/commit/6e497bdb760f5fe73a7028902be68bae2cc0e81d) - Mike Shearer [LOG-23081](https://logdna.atlassian.net/browse/LOG-23081)

# [1.10.0](https://github.com/answerbook/aura/compare/v1.9.0...v1.10.0) (2026-02-03)


### Bug Fixes

* add shared volume for cancellation test marker files [c710c23](https://github.com/answerbook/aura/commit/c710c23d7076c61a7c5b91bbfd4340e5b72a63bb) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)


### Chores

* add ENV for aura-mock-mcp src dir and one-command local tests [8bf1f39](https://github.com/answerbook/aura/commit/8bf1f39d146e59a4c02d87e84a7d1f24fe77a9c7) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)
* bump to stable release for mock-mcp [d25ffbd](https://github.com/answerbook/aura/commit/d25ffbd3f9416a079b87729b2698d2b001a2fc77) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)
* migrate cancellation test to feature flag [8275899](https://github.com/answerbook/aura/commit/8275899f8f0691c65fbea0e3fd0c5d183ae8ee60) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)
* move mock server code aura-mock-mcp [9f03527](https://github.com/answerbook/aura/commit/9f035276813c8c06b660ee62f025569dfc382c8e) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)
* reconcile tests with main's correlation tests [9efb18c](https://github.com/answerbook/aura/commit/9efb18cf607bc83cc50977fe2fe7ce6d1cf04272) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)
* remove legacy test shell scripts [f8ea2bd](https://github.com/answerbook/aura/commit/f8ea2bd08695dc02a121d0d71f2be9dadf5f0394) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)


### Features

* add Docker Compose infrastructure for integration tests [6f45c06](https://github.com/answerbook/aura/commit/6f45c06ade02993fa6a6e97e974ff875f1494b0a) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)
* add integration test feature flags [d9d7a2f](https://github.com/answerbook/aura/commit/d9d7a2f0dbbdcf8d98ddf21704f9d8bd46a1e52d) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)
* add Integration Tests stage to CI pipeline [403b6f8](https://github.com/answerbook/aura/commit/403b6f84f5465cc4fccb5e116d90ba89ccc40dcd) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)
* integration test reliability improvements [59468cb](https://github.com/answerbook/aura/commit/59468cb9a2fcdeb6d869a22d19cd61ce2d8d5995) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)
* migrate cancellation notification test to feature flag [c1c2b27](https://github.com/answerbook/aura/commit/c1c2b27abcd89ce758d1a83ae6655b3cd6d081e5) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)
* migrate events tests to feature flag [ff4139b](https://github.com/answerbook/aura/commit/ff4139bc62574d3d5ba0e82eb61502bcf119542f) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)
* migrate MCP tests to feature flag [3d14a02](https://github.com/answerbook/aura/commit/3d14a02e8437f82d732d98b012e626f0b527a997) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)
* migrate progress tests to feature flag [76a8784](https://github.com/answerbook/aura/commit/76a878428828833d6ae76e257994f4e749d685ae) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)
* migrate session tests to feature flag [af1e254](https://github.com/answerbook/aura/commit/af1e254dfa1c7025937564bdbb63fe2dd8fe27a5) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)
* migrate streaming tests to feature flag [f38f60d](https://github.com/answerbook/aura/commit/f38f60d2e5b09c35f63b1286c8bfa262bb0a39b9) - Mike Shearer [LOG-22984](https://logdna.atlassian.net/browse/LOG-22984)

# [1.9.0](https://github.com/answerbook/aura/compare/v1.8.0...v1.9.0) (2026-01-29)


### Features

* accurate context window and tool usage metrics [7215319](https://github.com/answerbook/aura/commit/72153195cb4a6b55fd79b36235004ba8c9d84f06) - Mike Shearer [LOG-23006](https://logdna.atlassian.net/browse/LOG-23006)

# [1.8.0](https://github.com/answerbook/aura/compare/v1.7.1...v1.8.0) (2026-01-29)


### Features

* ollama support [7097266](https://github.com/answerbook/aura/commit/709726635e1bd87329d81675406c38c24d386d5a) - Mike Shearer [LOG-22998](https://logdna.atlassian.net/browse/LOG-22998)

## [1.7.1](https://github.com/answerbook/aura/compare/v1.7.0...v1.7.1) (2026-01-27)


### Bug Fixes

* always use finish_reason: stop [14b6846](https://github.com/answerbook/aura/commit/14b684664ca2c557ef329c1939f413bf8e56defb) - Mike Shearer [LOG-23009](https://logdna.atlassian.net/browse/LOG-23009)

# [1.7.0](https://github.com/answerbook/aura/compare/v1.6.0...v1.7.0) (2026-01-27)


### Features

* log short id - not long [ab7c4f7](https://github.com/answerbook/aura/commit/ab7c4f701f3ff5a7c1b320f7f7c1257ca3a5311f) - Mike Shearer [LOG-23016](https://logdna.atlassian.net/browse/LOG-23016)

# [1.6.0](https://github.com/answerbook/aura/compare/v1.5.0...v1.6.0) (2026-01-26)


### Features

* max_tokens in config [bcd9088](https://github.com/answerbook/aura/commit/bcd908874efd0bba1c0f5b3dc79349b81048eb3a) - Mike Shearer [LOG-22999](https://logdna.atlassian.net/browse/LOG-22999)

# [1.5.0](https://github.com/answerbook/aura/compare/v1.4.0...v1.5.0) (2026-01-26)


### Features

* gemini model support [2cebe76](https://github.com/answerbook/aura/commit/2cebe7649af99d8bc2ce647f19e3c71d668891bf) - Mike Shearer [LOG-22998](https://logdna.atlassian.net/browse/LOG-22998)

# [1.4.0](https://github.com/answerbook/aura/compare/v1.3.0...v1.4.0) (2026-01-26)


### Features

* adds gpt5 reasoning [da6dd73](https://github.com/answerbook/aura/commit/da6dd73a3fbb1b2869880a2ce635b47dc511d5bd) - Mike Shearer [LOG-22790](https://logdna.atlassian.net/browse/LOG-22790)

# [1.3.0](https://github.com/answerbook/aura/compare/v1.2.6...v1.3.0) (2026-01-22)


### Features

* add basic docs for running librechat and openwebui (#42) [2776e93](https://github.com/answerbook/aura/commit/2776e9331ac8ac4598b850eb2fbd7ddf348349e1) - GitHub [LOG-22975](https://logdna.atlassian.net/browse/LOG-22975)

## [1.2.6](https://github.com/answerbook/aura/compare/v1.2.5...v1.2.6) (2026-01-21)


### Bug Fixes

* return token usage in responses (#40) [2cdbca3](https://github.com/answerbook/aura/commit/2cdbca3141bfb3cd5410e2f7c88299b5d0529697) - GitHub [LOG-22932](https://logdna.atlassian.net/browse/LOG-22932)

## [1.2.5](https://github.com/answerbook/aura/compare/v1.2.4...v1.2.5) (2026-01-20)


### Bug Fixes

* uses rig 0.28 tool id to fix correlation [8a813d7](https://github.com/answerbook/aura/commit/8a813d715af2c11f24057cbc4f69216074069e49) - Mike Shearer [LOG-22930](https://logdna.atlassian.net/browse/LOG-22930)

## [1.2.4](https://github.com/answerbook/aura/compare/v1.2.3...v1.2.4) (2026-01-15)


### Chores

* rig 0.28 bump [c2bbfc4](https://github.com/answerbook/aura/commit/c2bbfc4c2eade4668a204c5da02bd1e8fb471590) - Mike Shearer [LOG-22823](https://logdna.atlassian.net/browse/LOG-22823)

## [1.2.3](https://github.com/answerbook/aura/compare/v1.2.2...v1.2.3) (2026-01-14)


### Chores

* remove mcp aura-config dead code [3bf58c5](https://github.com/answerbook/aura/commit/3bf58c593f5026c4ab1caeb3b5580c3e4fff4f11) - Mike Shearer [LOG-22929](https://logdna.atlassian.net/browse/LOG-22929)

## [1.2.2](https://github.com/answerbook/aura/compare/v1.2.1...v1.2.2) (2026-01-14)


### Chores

* cargo.lock crate version [d04ef60](https://github.com/answerbook/aura/commit/d04ef608799893f0ce78a8cda01516149e544662) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* remove legacy SSE code for MCP [bb45a5d](https://github.com/answerbook/aura/commit/bb45a5d0e197bbaa86af07baf8fb15c24dfaa293) - Mike Shearer [LOG-22927](https://logdna.atlassian.net/browse/LOG-22927)

## [1.2.1](https://github.com/answerbook/aura/compare/v1.2.0...v1.2.1) (2026-01-12)


### Bug Fixes

* restore context overflow error handling (#35) [828157f](https://github.com/answerbook/aura/commit/828157f8141b1e27936cba4375dff522c5c546dc) - GitHub [LOG-22897](https://logdna.atlassian.net/browse/LOG-22897)

# [1.2.0](https://github.com/answerbook/aura/compare/v1.1.2...v1.2.0) (2026-01-08)


### Bug Fixes

* add timeout to notifications/cancelled [b0d3264](https://github.com/answerbook/aura/commit/b0d32640c6fcd80061d0a5845ca4f732422e68c7) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* env args not parsing true [0b4eaf6](https://github.com/answerbook/aura/commit/0b4eaf606277848b17771ce8479ec0b6c2f6d723) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* rig 0.26 upgrade [686c62b](https://github.com/answerbook/aura/commit/686c62bbcca71712ca19cef0a2db1f1206b3587b) - Mike Shearer [LOG-22629](https://logdna.atlassian.net/browse/LOG-22629)
* trunicate on non ascii may panic [6128549](https://github.com/answerbook/aura/commit/6128549bebfcb92432c37e2821fa877d4cf63f04) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)


### Chores

* auto comments reduction [a2baffa](https://github.com/answerbook/aura/commit/a2baffa682efc41df621dd7e7dfd66aa150bb547) - Mike Shearer [LOG-22582](https://logdna.atlassian.net/browse/LOG-22582)
* change Jenkinsfile to work with aura-next [ab3e0aa](https://github.com/answerbook/aura/commit/ab3e0aaf946d3b4c533500ab727b1afb58763679) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* Claudefile cleanup [528865e](https://github.com/answerbook/aura/commit/528865e5e2f553f641ce3c08f76ad15d64f8232c) - Mike Shearer [LOG-22744](https://logdna.atlassian.net/browse/LOG-22744)
* comment and new line fix in Cargo.toml [d1e8a2d](https://github.com/answerbook/aura/commit/d1e8a2d49d7215cffce7a376ab0f3f9877ee44d6) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* consolidate test env consts [fa69c8a](https://github.com/answerbook/aura/commit/fa69c8ab776c3236d3eed1d58a3f07abca9e1f37) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* debloat builder.rs [358835e](https://github.com/answerbook/aura/commit/358835ed449daf0e284c680c8b6df4e5ea4d2187) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* emoji reduction [404621f](https://github.com/answerbook/aura/commit/404621f9acb2956948d4526e10849fa9572b2caa) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* feature branch regex [d4c8312](https://github.com/answerbook/aura/commit/d4c831245fb03fa0c09ab1a667f462689bd47380) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* fix flaky tests and consolidate config [ac708ed](https://github.com/answerbook/aura/commit/ac708edaa1db992827d1f82672d297ee8f5b1f6d) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* fmt + clippy + test vars [f31f174](https://github.com/answerbook/aura/commit/f31f174dc2d524cb637d2f00f26f369bcf0acf00) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* handler docs de-verbose [66d58b8](https://github.com/answerbook/aura/commit/66d58b8475e1b50b0ba7369ec310fdfefa83e459) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* llm test determinism [49b8684](https://github.com/answerbook/aura/commit/49b868495b671d73da1b8a577d1f1639818be02c) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* more verbose docs removal [3008436](https://github.com/answerbook/aura/commit/30084369656b751145020d8ec09c30a9255f166a) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* move openai stream types [cd94a82](https://github.com/answerbook/aura/commit/cd94a82350ae38983776da31786be84bd7fe42e1) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* pr feedback ref rust analyzer and Agent::new [9ae19f4](https://github.com/answerbook/aura/commit/9ae19f490954add0e1a033028a134862e493a482) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* refactor streaming handlers [a27cd85](https://github.com/answerbook/aura/commit/a27cd85dc5f5524455690ea0598ac85275f79a42) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* remove complex string optimization [53ff4b6](https://github.com/answerbook/aura/commit/53ff4b697f656ee68c1ab45bf480a918a5823acb) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* remove old thread local cancellation map [a733139](https://github.com/answerbook/aura/commit/a73313903dae504fc6e7d9e99149634bdb723d9b) - Mike Shearer [LOG-22743](https://logdna.atlassian.net/browse/LOG-22743)
* self pr cleanup [259e816](https://github.com/answerbook/aura/commit/259e816a80b8c7001926e2c57dcab163edb3b4c1) - Mike Shearer [LOG-22753](https://logdna.atlassian.net/browse/LOG-22753)
* update branchs in CLAUDE.md [0b3863c](https://github.com/answerbook/aura/commit/0b3863c5745bf4c8e2ab1ad46c35f99ec0ff973e) - Mike Shearer [LOG-22744](https://logdna.atlassian.net/browse/LOG-22744)


### Code Refactoring

* PR #31 review - spirit not letter [b997871](https://github.com/answerbook/aura/commit/b9978711f2007b94e0012345bfe347ccb357fdf3) - Mike Shearer [LOG-22753](https://logdna.atlassian.net/browse/LOG-22753)


### Features

* bounded streaming with aura events [b8ff729](https://github.com/answerbook/aura/commit/b8ff729ce7ef3e644d253e8212b0ef50a4140e3f) - Mike Shearer [LOG-22628](https://logdna.atlassian.net/browse/LOG-22628)
* MCP tool discovery caching and cancellation propagation [4a4effa](https://github.com/answerbook/aura/commit/4a4effa4c9058926018c233dedaf2f4d01f7f67d) - Mike Shearer [LOG-22693](https://logdna.atlassian.net/browse/LOG-22693) [LOG-22629](https://logdna.atlassian.net/browse/LOG-22629)
* request-scoped MCP progress and cancellation [20c0a40](https://github.com/answerbook/aura/commit/20c0a4073e9e529b2a9c968e9e846a6f0e913607) - Mike Shearer [LOG-22629](https://logdna.atlassian.net/browse/LOG-22629)

## [1.1.2](https://github.com/answerbook/aura/compare/v1.1.1...v1.1.2) (2025-12-04)


### Chores

* ignore internal claude planning docs [413b094](https://github.com/answerbook/aura/commit/413b0947c394a889a484018cab0f6fd4834e7d8d) - Mike Shearer [LOG-22582](https://logdna.atlassian.net/browse/LOG-22582)

## [1.1.1](https://github.com/answerbook/aura/compare/v1.1.0...v1.1.1) (2025-11-18)


### Bug Fixes

* temperature in agent config [448367f](https://github.com/answerbook/aura/commit/448367fa5834247794dc10056633fd243e1315c6) - Mike Shearer [LOG-22477](https://logdna.atlassian.net/browse/LOG-22477)

# [1.1.0](https://github.com/answerbook/aura/compare/v1.0.4...v1.1.0) (2025-11-18)


### Bug Fixes

* consolidate tool/mcp construction [47a9b7b](https://github.com/answerbook/aura/commit/47a9b7b90929d804990682cb398245cd5a8014f4) - Mike Shearer [LOG-22597](https://logdna.atlassian.net/browse/LOG-22597)
* tool_call sanitization spam [3ce629d](https://github.com/answerbook/aura/commit/3ce629d86da81184fd3754d9dbe2e743050dbd3f) - Mike Shearer [LOG-22597](https://logdna.atlassian.net/browse/LOG-22597)


### Chores

* docs consolidation [975ea64](https://github.com/answerbook/aura/commit/975ea643beb3156c18e716e625ef82690ae496f2) - Mike Shearer [LOG-22599](https://logdna.atlassian.net/browse/LOG-22599)
* pr feedback/docs reduction [b16d428](https://github.com/answerbook/aura/commit/b16d42838c5c4e1b44b7bed1f0c027d05eb9f68e) - Mike Shearer [LOG-22599](https://logdna.atlassian.net/browse/LOG-22599)
* remove internal planning docs [306e382](https://github.com/answerbook/aura/commit/306e382cec4bb0946dd2fcc78bf99550f05a610a) - Mike Shearer [LOG-22599](https://logdna.atlassian.net/browse/LOG-22599)
* streaming release env [42c4834](https://github.com/answerbook/aura/commit/42c4834f5b219b1ff87050185566647a1a8485d1) - Mike Shearer [LOG-22477](https://logdna.atlassian.net/browse/LOG-22477)


### Features

* OpenAI-compatible SSE streaming with multi-turn tool execution [2c130c9](https://github.com/answerbook/aura/commit/2c130c9ac515e8d13683d34c921f7a4d5a34132e) - Mike Shearer [LOG-22477](https://logdna.atlassian.net/browse/LOG-22477)

## [1.0.4](https://github.com/answerbook/aura/compare/v1.0.3...v1.0.4) (2025-11-12)


### Bug Fixes

* turn down the log spam (#23) [2bee6ad](https://github.com/answerbook/aura/commit/2bee6ad78b1b282fd8cdeef5736a2f8bb09e22f6) - GitHub [INFRA-99999](https://logdna.atlassian.net/browse/INFRA-99999)

## [1.0.3](https://github.com/answerbook/aura/compare/v1.0.2...v1.0.3) (2025-10-29)


### Bug Fixes

* not fowarding all headers [37a08fd](https://github.com/answerbook/aura/commit/37a08fda28844fc1fe54ccacfb2330a70e7e8f31) - Mike Shearer [LOG-22545](https://logdna.atlassian.net/browse/LOG-22545)

## [1.0.2](https://github.com/answerbook/aura/compare/v1.0.1...v1.0.2) (2025-10-28)


### Bug Fixes

* wrong headers for gateway session hash [40659dd](https://github.com/answerbook/aura/commit/40659dda1cd676cf49a916e412b2ed0f87366c88) - Mike Shearer [LOG-22535](https://logdna.atlassian.net/browse/LOG-22535)

## [1.0.1](https://github.com/answerbook/aura/compare/v1.0.0...v1.0.1) (2025-10-15)


### Chores

* change app-name [9c7eae1](https://github.com/answerbook/aura/commit/9c7eae13aade979b53ea386f43462ad4e6957a67) - Mike Shearer [LOG-22447](https://logdna.atlassian.net/browse/LOG-22447)

# 1.0.0 (2025-10-15)


### Bug Fixes

* check in lock file [93f5a88](https://github.com/answerbook/aura/commit/93f5a883987b98be2a61d62cbb077b3d7b12d9c4) - Mike Shearer
* conversation state in cli interactive mode fixed [9b05e57](https://github.com/answerbook/aura/commit/9b05e57bf4a1c71d2880ba7cab2180e9258df2a8) - Mike Shearer
* correct implementation of turn depth for tool calls [7d1dea9](https://github.com/answerbook/aura/commit/7d1dea91914eba6bcf04df6bf46758c89d6f8ab3) - Mike Shearer
* don't drop whole MCP for one invalid tool [ec07054](https://github.com/answerbook/aura/commit/ec07054bc08514569a01dd9c4789a170fe2f7e0b) - Mike Shearer
* embedding mismatch and schema sanitization [b467fe2](https://github.com/answerbook/aura/commit/b467fe252e6a9e8520e8ebbbd292962b63e1bd49) - Mike Shearer
* interpolating commented out env vars [309c7f6](https://github.com/answerbook/aura/commit/309c7f6c1d4d8905c96e2f0437d744d9e935a38c) - Mike Shearer
* multi turn web server context fix [0a25f8a](https://github.com/answerbook/aura/commit/0a25f8ae6be812f1ae8e057fd81c12ae1d8a041d) - Mike Shearer
* setting conversation_depth to 0 disables the check [e7def4c](https://github.com/answerbook/aura/commit/e7def4cea969a95ee242d517f6f8f9bb86124fff) - Mike Shearer
* true dynamic tool registration and sanitization with openAI [bf118e1](https://github.com/answerbook/aura/commit/bf118e149136500fcffcda7406a6ab18e6918640) - Mike Shearer


### Chores

* 1st pass README overhaul [6553699](https://github.com/answerbook/aura/commit/6553699616642079e09a18cf6162cf24de30640e) - Mike Shearer
* 1st pass rename all aura over proto rig-toml-test [9b04ea8](https://github.com/answerbook/aura/commit/9b04ea89ff22fd42e1970b956af0a3df710148a4) - Mike Shearer
* better default config for testing manually [f425016](https://github.com/answerbook/aura/commit/f4250167fde53aef9c1fce275f52f24f35d1175f) - Mike Shearer
* bump rig to 0.20.0. fix: vector search rag store registration [91c3eb7](https://github.com/answerbook/aura/commit/91c3eb7ccde9be076233bad69c98040cbb2484c6) - Mike Shearer
* claude doc [eed4738](https://github.com/answerbook/aura/commit/eed473873b6884a4fb1f6d6e073405049601eb57) - Mike Shearer
* claude docs [0660f9e](https://github.com/answerbook/aura/commit/0660f9eb08e98368faaaf01aa197e699c13b5aa7) - Mike Shearer
* clippy fixes [e96937e](https://github.com/answerbook/aura/commit/e96937e4ed1c8c4efb68166c07a397ecb90dad29) - Mike Shearer
* comprehensive debug flag added [da2c3b2](https://github.com/answerbook/aura/commit/da2c3b24b02b6c6ad471d10058fe064ec865c315) - Mike Shearer
* doc in todo for claudefile and my records [71b339a](https://github.com/answerbook/aura/commit/71b339ada8f4365328de65b0993288567c8cd11c) - Mike Shearer
* docs [ff3370a](https://github.com/answerbook/aura/commit/ff3370a208ae0c56066e2a19e01498d3dce38a4d) - Mike Shearer
* docs update for CLI args [88f1a63](https://github.com/answerbook/aura/commit/88f1a63450ed0ff26e379256598f72d019eb9dde) - Mike Shearer
* documentation and config file for demo with claude [5fc0a1b](https://github.com/answerbook/aura/commit/5fc0a1b56a3494aa9e3f3642d108eb7c2bd4dc90) - Mike Shearer
* embed mcp-openai-bridge as a vendor dep and bump to newest version for better empty prop handling [8cd1295](https://github.com/answerbook/aura/commit/8cd129550cfa9a5f718fccb871d0e9a9b5fb17e9) - Mike Shearer
* enforce nightly [31e715f](https://github.com/answerbook/aura/commit/31e715feb837d9249a5041305c5f817c8b085a98) - Mike Shearer
* extra logging and docs/plans for MCP tool discovery proper [2fc067e](https://github.com/answerbook/aura/commit/2fc067ec8ad82fc05baff8a0d55ccbe67f17fb9b) - Mike Shearer
* fix warnings [0ac59db](https://github.com/answerbook/aura/commit/0ac59db89413d9d3cb757f28dbc340c1205ad1f4) - Mike Shearer
* fmt [6033b75](https://github.com/answerbook/aura/commit/6033b75f607800e16731213bb12cb2e2da7b482c) - Mike Shearer
* fmt, fix: tool fallback (hardcode) for undiscoverable names [cab5dde](https://github.com/answerbook/aura/commit/cab5ddedfeebc19474a3e2cb6f94c16a669d8ee6) - Mike Shearer
* integration test benchmark stubs [792933f](https://github.com/answerbook/aura/commit/792933f66aca314b630b9344d8229ce7437166fb) - Mike Shearer
* left over docs change for TODO [d73736a](https://github.com/answerbook/aura/commit/d73736a6e095bc7556cf25a928baffd3169b706a) - Mike Shearer
* make verbose flag with sane info logging [d94bb17](https://github.com/answerbook/aura/commit/d94bb17305ebf6cf4cab00cb759e9cc058f1afc7) - Mike Shearer
* move and clean up test scripts [27cd500](https://github.com/answerbook/aura/commit/27cd5003fac2368e60fc140565b4acd4e38b81a3) - Mike Shearer
* our logging fix was duplicated - consolidate to aura:logging [1557da0](https://github.com/answerbook/aura/commit/1557da0b10664d11097c76e42e3fea5d7a9a773d) - Mike Shearer
* quick analysis and plan for max depth bug [cc3395d](https://github.com/answerbook/aura/commit/cc3395d9f453453a95aacd841636d9cd516f6f3f) - Mike Shearer
* Remaining references to rig-toml removed [92e09db](https://github.com/answerbook/aura/commit/92e09dbd5d50dfb9e680cce4d8ee56f358f31093) - Mike Shearer
* remove documentation .md files [c685e17](https://github.com/answerbook/aura/commit/c685e17cbe10c49f4c46789fc95f98426e42575a) - Mike Shearer
* remove local env steps from ADR [2bbe3c0](https://github.com/answerbook/aura/commit/2bbe3c05aced5f6790552a6832d17fddb2ec2ddd) - Mike Shearer
* remove more rig-toml references [61a7a19](https://github.com/answerbook/aura/commit/61a7a19b8b007095e1c8691d6aec5217fe26de48) - Mike Shearer
* test config cleanup [9c3b874](https://github.com/answerbook/aura/commit/9c3b8742f2df06433a67fd23b45f813edd756e01) - Mike Shearer
* thin claudefile [61c67e6](https://github.com/answerbook/aura/commit/61c67e6d6e0d38d341e644936815fc4bdee9c25b) - Mike Shearer
* thin docks and update dump flags to dump dynamic MCP Tools [ec318b6](https://github.com/answerbook/aura/commit/ec318b697601001409e361eaa8df0a32ff4998a6) - Mike Shearer
* thin down claude file [b6d3b0e](https://github.com/answerbook/aura/commit/b6d3b0ee897aaa3e843fe5c6966e8c8ea2b2bf40) - Mike Shearer
* todo on verbose and debug flags for web server [3b0e3fd](https://github.com/answerbook/aura/commit/3b0e3fd87b969736f366bfc2661a326fbcc48084) - Mike Shearer
* TODO update [95cfd42](https://github.com/answerbook/aura/commit/95cfd42f991a32e17e352d8200a5bf882eadace0) - Mike Shearer
* todo update and todo list fixes [7894e9b](https://github.com/answerbook/aura/commit/7894e9bf06e6777d25e1add12a61a6444993ce42) - Mike Shearer
* update claudefile [878c5b3](https://github.com/answerbook/aura/commit/878c5b3692c7522d21d1a3965eeb49ea968c75a6) - Mike Shearer
* update docs todo [50f5c07](https://github.com/answerbook/aura/commit/50f5c07e97b78d1e23b1b89d518e9aea6c565a09) - Mike Shearer
* update docs, docstrings, and docker to match new crate names [b701943](https://github.com/answerbook/aura/commit/b701943bf5af823d7dab689ad5125f5503592582) - Mike Shearer
* use github for mcp-openai-bridge for now [c75d7a4](https://github.com/answerbook/aura/commit/c75d7a4db53baf630164acc37c7dc5e2162e27f5) - Mike Shearer


### Features

* add debug and verbose flag to web server [8287570](https://github.com/answerbook/aura/commit/82875706127a76cd79501d2b2532434eb6a0f7a4) - Mike Shearer
* add docker [76cd89f](https://github.com/answerbook/aura/commit/76cd89f88824eec0a28d5fd83bd42c976f53d290) - Mike Shearer
* agent pooling for to perisit stateful mcp client [d55ca8c](https://github.com/answerbook/aura/commit/d55ca8c880ebf2d76cfecc4f4461c2ed304bc81f) - Mike Shearer [LOG-22444](https://logdna.atlassian.net/browse/LOG-22444)
* filter logging so that new otel traces out of rig don't flood log buffer [c88c769](https://github.com/answerbook/aura/commit/c88c7690bd9731689b2aaa76fc8907842fed3afd) - Mike Shearer
* initial bedrock support compiling [95f7203](https://github.com/answerbook/aura/commit/95f7203270a496c058a8fc6b93d38e8df0a6d357) - Mike Shearer
* k8s config for deploy [523e87e](https://github.com/answerbook/aura/commit/523e87ee6ce0eb77b7e89647b56a0613beefcb58) - Mike Shearer [LOG-22447](https://logdna.atlassian.net/browse/LOG-22447)
* multi kb support over qdrant [48127e7](https://github.com/answerbook/aura/commit/48127e7c06f598f90eba7afb9c2f3b4fb8f0beb3) - Mike Shearer
* use mcp-openai-bridge for definitive schema sanitization [ba43cd2](https://github.com/answerbook/aura/commit/ba43cd24f681829e38dcbd6ebeb695a16a03820c) - Mike Shearer
* wip - rename conversation_depth to turn depth which is more in the spirit of the feature [c8186b1](https://github.com/answerbook/aura/commit/c8186b1b71d41846701498817f9c29b12253b680) - Mike Shearer


### Miscellaneous

* Merge pull request #10 from answerbook/mshearer/bedrock-as-a-provider-test [dde5148](https://github.com/answerbook/aura/commit/dde5148b6bfb88bc03c9959d21b7bf7a80518b95) - GitHub
* Merge branch 'main' into mshearer/bedrock-as-a-provider-test [bd8a39b](https://github.com/answerbook/aura/commit/bd8a39b60811b606edc2a6553be967790da7ba6f) - Mike Shearer
* Merge pull request #9 from answerbook/mshearer/strip-toml-comments-before-interpolation [c084c42](https://github.com/answerbook/aura/commit/c084c42cbbde72956ff8fcd0819597d18bca4d72) - GitHub
* Merge pull request #7 from answerbook/mshearer/bump-mcp-openai-bridge-empty-props [54e448b](https://github.com/answerbook/aura/commit/54e448b2a0a67c6b6af7b94581c3e34e444a3403) - GitHub
* Merge pull request #6 from answerbook/mshearer/lib-name [ae92eb4](https://github.com/answerbook/aura/commit/ae92eb47f9fde1f160af5c276268a56a7a2b853c) - GitHub
* Merge branch 'main' into mshearer/lib-name [0a0b430](https://github.com/answerbook/aura/commit/0a0b430df0c25f865b306026531c1e4a24cd3781) - Mike Shearer
* Merge pull request #5 from answerbook/mshearer/multi-turn-actual-fix [4293ef0](https://github.com/answerbook/aura/commit/4293ef08086752a7a1a50cacd579135686ff7f87) - GitHub
* Merge pull request #4 from answerbook/mshearer/disable-conversation-depth-fix [0bd40ec](https://github.com/answerbook/aura/commit/0bd40ec375539aeb26883c13045959ab1ce6445a) - GitHub
* Merge pull request #3 from answerbook/mshearer/verbose-and-debug-for-web [3420263](https://github.com/answerbook/aura/commit/34202632cfd28776308b921745919c5c355dfe68) - GitHub
* Merge pull request #2 from answerbook/msheare/mcp-openai-bridge-integration [7b6f923](https://github.com/answerbook/aura/commit/7b6f92326cff9f65464076f27c70fcb2c3da3586) - GitHub
* Merge pull request #1 from answerbook/mshearer/stateless-multi-turn-web [f185123](https://github.com/answerbook/aura/commit/f1851235b0185cf22fd2de4c59601c73acc7aac3) - GitHub
* feature adds --dump-prompt for exact llm json dump [ccffed7](https://github.com/answerbook/aura/commit/ccffed72112bd1664c50f4be2829888ac7dbafd4) - Mike Shearer
* Adds SSE transport as workaround to use UVX since we don't plan on exposing STDIO as a first class integration [1fe0b8b](https://github.com/answerbook/aura/commit/1fe0b8bd41ae404157603a93c65ccb14e145d067) - Mike Shearer
* Upgrade to RMCP 0.6.x - fixes a lot of schema issues with tool registration [fc0707a](https://github.com/answerbook/aura/commit/fc0707a126518d153452cc1b8f0184be777c5e8e) - Mike Shearer
* fixes tool context registration with LLM from streamable-http-mcp [c58620b](https://github.com/answerbook/aura/commit/c58620bde79b62a3adbfd0fa201c75a15371a05e) - Mike Shearer
* fixes our dual rmcp version problems with the raw stremable-http-mcp server [154d832](https://github.com/answerbook/aura/commit/154d832ed4dfc82d4448d7a9032d97c5d80752fa) - Mike Shearer
* fix some issues with MCP discovery [9589bb6](https://github.com/answerbook/aura/commit/9589bb614d173b958dba00c08dd28a8796429167) - Mike Shearer
* Initial implementation of Rig TOML configuration system [f29697a](https://github.com/answerbook/aura/commit/f29697adbaf57da741979e76951b46fe9aa6e54f) - Mike Shearer
* 1st pass at web server with multi turn http support and an openAI style http schema [6dc935e](https://github.com/answerbook/aura/commit/6dc935e209bb99f2f93c1539ce902ec4bc8fc1dc) - Mike Shearer
* config file checkpoint [85c0d0d](https://github.com/answerbook/aura/commit/85c0d0d38ba2fb71d6331bc1ea6743268639cf23) - Mike Shearer
* Fixing KB with mcp-proxy [a9a959c](https://github.com/answerbook/aura/commit/a9a959c06f2622cbb8c2ab2040eca92734830115) - Mike Shearer
* remote mcp direct connection pass [37c5fe0](https://github.com/answerbook/aura/commit/37c5fe01501985db8a6d398b8564c463351afee0) - Mike Shearer
