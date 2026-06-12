# Agent Skills

On-demand skill loading keeps your agent's system prompt small. Detailed workflow instructions load only when the agent actually needs them.

Skills follow the [Agent Skills specification](https://agentskills.io/specification). Each skill is a directory containing a `SKILL.md` file with YAML frontmatter. When skills are configured, agents receive two tools:

- `load_skill(name)` retrieves a skill's instructions and lists available resource files
- `read_skill_file(skill, path)` fetches individual resource files on demand

This progressive disclosure pattern avoids bloating the base prompt with instructions the agent may never use.

## Configuration

Add skill source directories to your agent config:

```toml
[agent]
name = "Assistant"
system_prompt = "You are a helpful assistant."

[[agent.skills.local]]
source = "./skills"
```

The `source` path can be absolute or relative to the config file's directory. Aura scans each source directory for subdirectories containing a `SKILL.md` file.

### Environment fallback

If you don't configure any skill sources, Aura checks the `AURA_SKILLS_DIR` environment variable:

```bash
export AURA_SKILLS_DIR=/app/skills
```

The Docker image sets this to `/app/skills` by default.

## SKILL.md format

Each skill directory must contain a `SKILL.md` file with YAML frontmatter:

```markdown
---
name: incident-response
description: Guide the agent through incident triage and resolution workflows
---

# Incident Response

When investigating an incident, follow these steps:

1. Gather initial context from alerts and metrics
2. Identify the affected services and dependencies
3. Assess impact and escalate if necessary
4. Document findings and next steps

## Triage checklist

- Check recent deployments
- Review error rates and latency metrics
- Identify affected user segments
```

### Frontmatter fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Skill identifier. Must match the directory name. Lowercase alphanumeric and hyphens only, 1-64 characters. Must not start or end with a hyphen or contain consecutive hyphens. |
| `description` | Yes | Brief summary of what the skill does and when to use it. Maximum 1024 characters. Appears in the system prompt catalog and `load_skill` tool description. |
| `license` | No | Ignored by Aura (spec-defined, available for skill distribution). |
| `compatibility` | No | Ignored by Aura (spec-defined). |
| `metadata` | No | Ignored by Aura (spec-defined). |
| `allowed-tools` | No | Ignored by Aura (spec-defined). |

### Resource directories

Skills can include supporting files in three subdirectories:

- `references/` for detailed documentation, API references, runbooks
- `scripts/` for example scripts, templates, automation snippets
- `assets/` for images, diagrams, or other binary files

When `load_skill` returns the skill content, it appends a listing of available resources. The agent can then fetch specific files with `read_skill_file`:

```
## Skill resources
Load any of these with the `read_skill_file` tool when needed:
- references/runbook.md
- scripts/deploy.sh
```

Resource paths must stay within the skill directory. Absolute paths, `..` components, and symlinks that escape the skill directory are rejected.

## How it works

When skills are configured, Aura:

1. Discovers skills by scanning source directories at startup
2. Appends a skill catalog to the agent's system prompt:
   ```
   Available skills (use the `load_skill` tool to load before answering):
   - incident-response: Guide the agent through incident triage and resolution workflows
   - code-review: Perform structured code reviews following team standards
   ```
3. Registers the `load_skill` and `read_skill_file` tools

The agent sees the catalog and calls `load_skill("incident-response")` when a user asks about incident handling. The tool returns the SKILL.md body (without frontmatter) plus the resource listing.

## Orchestration

In orchestration mode, the coordinator inherits skills from `[agent.skills]`. Workers inherit the same skills by default.

### Per-worker overrides

Override skills for a specific worker with `[orchestration.worker.<name>.skills]`:

```toml
[agent]
name = "Coordinator"
system_prompt = "You route requests to specialized workers."

[[agent.skills.local]]
source = "./skills/general"

[orchestration]
enabled = true

[orchestration.worker.sre]
description = "Handles operational issues"
preamble = "You are an SRE specialist."

[[orchestration.worker.sre.skills.local]]
source = "./skills/sre"

[orchestration.worker.writer]
description = "Writes documentation"
preamble = "You are a technical writer."
skills.local = []  # Explicitly disable skills for this worker
```

When a worker has no `[orchestration.worker.<name>.skills]` section, it inherits the agent-level skills. An explicit empty `skills.local = []` disables skills for that worker.

## Example skill

Directory structure:

```
skills/
└── incident-response/
    ├── SKILL.md
    └── references/
        ├── escalation-matrix.md
        └── severity-definitions.md
```

`skills/incident-response/SKILL.md`:

```markdown
---
name: incident-response
description: Guide the agent through incident triage and resolution workflows
---

# Incident Response

This skill helps you investigate and resolve production incidents.

## Process

1. **Acknowledge** the incident and assess initial severity
2. **Investigate** using available tools to gather context
3. **Mitigate** the immediate impact if possible
4. **Escalate** if severity warrants or resolution is unclear
5. **Document** findings and follow-up actions

## Severity levels

Reference the escalation matrix for severity definitions and response expectations.
```

## Validation errors

Aura validates skills at config load and reports errors for:

- Missing or invalid `name` field
- Name that doesn't match the directory name
- Empty or missing `description` field
- Description exceeding 1024 characters
- Source directory not found or unreadable
- Duplicate skill names across sources (first wins, duplicates warn)

## Size considerations

Skills are loaded in full when the agent calls `load_skill`. Large skills can consume significant context window. Aura logs a warning when skill content exceeds 512 KB:

```
WARN Skill 'my-skill' content is large (600000 bytes). This may consume significant LLM context window.
```

Consider splitting large skills into smaller, focused skills or moving detailed content into resource files.
