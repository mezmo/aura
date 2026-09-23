'use strict'

const base = require('@mezmoinc/release-config-docker')

// Required, not defaulted: env-ci resolves the release branch from GIT_BRANCH
// before BRANCH_NAME, so a missing BRANCH_NAME would leave semantic-release
// releasing nightly while this config hands it the stable plugin chain.
const branch = process.env.BRANCH_NAME
if (!branch) throw new Error('BRANCH_NAME is required to select the release channel')

// semantic-release drops a configured branch the repo lacks, so the per-cycle
// beta entry is inert while no beta exists.
const branches = [
  'main',
  {name: 'beta', prerelease: 'beta', channel: 'beta'},
  {name: 'nightly', prerelease: 'nightly', channel: 'nightly'},
]

/* Override releaseRules when on a 0.x.x version */
const releaseRules = base.releaseRules.map((rule) => {
  switch (rule.release) {
    case 'major':
      return {...rule, release: 'minor'}
    case 'minor':
      return {...rule, release: 'patch'}
    default:
      return {...rule}
  }
})

// A tag rendering empty is dropped, and `channel` is empty only on stable, so
// one list serves every channel.
const dockerTags = [
  '{{version}}',
  '{{#if (eq channel "nightly")}}nightly{{/if}}',
  '{{#unless channel}}latest{{/unless}}',
  '{{#unless channel}}{{major}}-latest{{/unless}}',
  '{{#unless channel}}{{major}}.{{minor}}-latest{{/unless}}',
  '{{#unless channel}}automated-security-scan{{/unless}}',
]

// semantic-release fixes the plugin list before it resolves the branch, so the
// drops key off BRANCH_NAME rather than nextRelease.channel. An unrecognised
// branch keeps every plugin, which is what the change-request dry run wants.
const DROPPED = {
  beta: new Set(['@semantic-release/changelog', '@semantic-release/git']),
  nightly: new Set([
    '@semantic-release/changelog',
    '@semantic-release/git',
    '@semantic-release/github',
  ]),
}

const dropped = DROPPED[branch] ?? new Set()
const prerelease = branch in DROPPED
const plugins = base.plugins
  .filter((plugin) => !dropped.has(Array.isArray(plugin) ? plugin[0] : plugin))
  .map((plugin) => {
    const [name, options] = Array.isArray(plugin) ? plugin : [plugin, {}]
    // there is a config name clash with github + git
    // we need to isolate this as to not commit binaries in the repo
    if (name !== '@semantic-release/github') return plugin
    return [name, {...options, assets: [{path: 'dist/*'}]}]
  })

// Top level, not exec's plugin options: the dry run names the plugin on the
// command line, and such a plugin is given only the top-level keys.
const execCommands = {
  // https://github.com/semantic-release/exec
  prepareCmd: './scripts/set-version.sh ${nextRelease.version}',
}
// The tap and package repo serve stable, so only its chain touches them, and
// verifyRelease dry-runs both before any prepare or publish side effect.
if (!prerelease) {
  execCommands.verifyReleaseCmd = [
    './scripts/bump-homebrew-tap.sh --dry-run ${nextRelease.version}',
    'DRY_RUN=1 make publish-packages',
  ].join(' && ')
  execCommands.successCmd =
    './scripts/bump-homebrew-tap.sh ${nextRelease.version} && make publish-packages'
}

/**
 * See: https://semantic-release.gitbook.io/semantic-release/usage/configuration
 **/
module.exports = {
  // https://github.com/mezmo/release-config-docker
  extends: '@mezmoinc/release-config-docker',
  npmPublish: false,
  branches,
  releaseRules,
  ...execCommands,
  dockerTags,
  // https://github.com/esatterwhite/semantic-release-docker
  dockerProject: 'mezmo',
  dockerImage: 'aura',
  dockerLogin: false,
  dockerFile: 'Dockerfile',
  dockerVerifyCmd: ['ls'],
  dockerBuildFlags: {
    target: 'release',
  },
  dockerPlatform: [
    'linux/amd64',
    'linux/arm64',
  ],
  plugins,
}

// stderr, so a caller reading a version off stdout is not fed the config.
console.error(`release branch: ${branch || '<unset>'}`)
console.error(require('node:util').inspect(module.exports, {depth: 10}))
