# Skills (On-Demand Instructions)

Skills package task-specific instructions that the agent pulls in only when a task calls for them. Each skill is a directory in the [Agent Skills format](https://agentskills.io/specification): a `SKILL.md` file with YAML frontmatter (`name`, `description`) followed by the instructions, plus optional `references/`, `scripts/`, and `assets/` subdirectories for supporting files.

Rather than inlining every skill into the system prompt, AURA appends only a catalog of names and descriptions. The LLM calls the `load_skill` tool to fetch a skill's full instructions on demand, and `read_skill_file` to fetch individual resource files. `read_skill_file` resolves symlinks and rejects any path that escapes the skill directory.

## Configuration

See [`[agent.skills]`](configuration-reference.md#agentskills) in the configuration reference for the full field table.

```toml
[agent.skills]
local = [
  { source = "./skills" },               # relative paths resolve from the process CWD
  { source = "/opt/aura/shared-skills" }
]
```

Each `source` is a directory containing skill subdirectories:

```text
skills/
└── code-review/
    ├── SKILL.md       # required: frontmatter (name, description) + instructions
    ├── references/    # optional resources, fetched via read_skill_file
    ├── scripts/
    └── assets/
```

## Discovery and validation

Discovery runs at agent build time and validates each skill against the specification; the frontmatter `name` must match the directory name. Directories without a `SKILL.md` are skipped. When two sources provide the same skill name, the first one loaded wins and a warning is logged. Relative sources resolve from the process current working directory in every mode (web server, standalone CLI, and A2A). `CONFIG_PATH` / `--config` locate the TOML file only; they do not change how paths inside TOML are resolved.

## Orchestration inheritance

In orchestration mode the coordinator inherits `[agent.skills]`. Workers inherit it too, unless `[orchestration.worker.<name>.skills]` provides their own sources; an explicit empty list disables skills for that worker (see [Per-Worker Skills Override](configuration-reference.md#per-worker-skills-override) for the exact inheritance/override rules):

```toml
[orchestration.worker.knowledge.skills]
local = [{ source = "./knowledge-skills" }]   # worker-specific skills

[orchestration.worker.operations.skills]
local = []                                    # no skills for this worker
```
