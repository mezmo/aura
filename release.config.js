'use strict'

const config = require('@mezmoinc/release-config-docker')

const releaseRules = config.releaseRules.map((rule) => {
  switch (rule.release) {
    case 'major':
      return {...rule, release: 'minor'}
    case 'minor':
      return {...rule, release: 'patch'}
    default:
      return {...rule}
  }
})

const dockerTags = [
  ...config.dockerTags,
  'latest',
]

const plugins = config.plugins.map((plugin) => {
  const [name, config = {}] = plugin
  // there is a config name clash with github + git
  // we need to isolate this as to not commit binaries in the repo
  if (name === '@semantic-release/github') {

    config.assets = [
      {path: 'dist/*'}
    ]
    return [name, config]
  }
  return plugin
})

/**
 * See: https://semantic-release.gitbook.io/semantic-release/usage/configuration
 **/
module.exports = {
  // https://github.com/mezmo/release-config-docker
  extends: '@mezmoinc/release-config-docker',
  npmPublish: false,
  branches: ['main'],
  /* Override releaseRules when on a 0.x.x version */
  releaseRules,
  dockerTags,
  // https://github.com/semantic-release/exec
  prepareCmd: `./scripts/set-version.sh \${nextRelease.version}`,
  // The dry-run push needs the packages already in dist/, so a release dry run
  // has to be preceded by `make build-packages`.
  verifyReleaseCmd: `./scripts/bump-homebrew-tap.sh --dry-run \${nextRelease.version} && DRY_RUN=1 make publish-packages`,
  successCmd: `./scripts/bump-homebrew-tap.sh \${nextRelease.version} && make publish-packages`,
  // https://github.com/esatterwhite/semantic-release-docker
  dockerProject: 'mezmo',
  dockerImage: 'aura',
  dockerLogin: false,
  dockerFile: 'Dockerfile',
  dockerVerifyCmd: ['ls'],
  dockerBuildFlags: {
    target: 'release'
  },
  dockerPlatform: [
    'linux/amd64'
  , 'linux/arm64'
  ],
  plugins: plugins
}

// stderr, so a caller reading a version off stdout is not fed the config.
console.error(require('node:util').inspect(module.exports, {depth: 10}))
