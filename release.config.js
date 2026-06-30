'use strict'

const config = require('@mezmoinc/release-config-docker')

const buildReleaseBinariesCmd = 'make build-release-binaries'


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

  // https://github.com/semantic-release/exec
  prepareCmd: `./scripts/set-version.sh \${nextRelease.version}; sleep 2; ${buildReleaseBinariesCmd}`,
  // On a dry run (no prepare step), build the release binaries during verify so
  // a broken cross-compile is caught before the change reaches the release branch.
  verifyReleaseCmd: `if [ "\${options.dryRun}" = "true" ]; then ${buildReleaseBinariesCmd}; fi`,
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

console.dir(module.exports, {depth: 10})
