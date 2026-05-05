'use strict'

/**
 * See: https://semantic-release.gitbook.io/semantic-release/usage/configuration
 **/
module.exports = {
  // https://github.com/mezmo/release-config-docker
  extends: '@mezmoinc/release-config-docker',
  npmPublish: false,
  branches: ['main'],

  // https://github.com/semantic-release/exec
  prepareCmd: './scripts/set-version.sh ${nextRelease.version}; sleep 2',

  // https://github.com/esatterwhite/semantic-release-docker
  dockerProject: 'mezmo',
  dockerImage: 'aura',
  dockerLogin: false,
  dockerFile: 'Dockerfile',
  dockerVerifyCmd: ['ls'],
  dockerPlatform: [
    'linux/amd64'
  , 'linux/arm64'
  ]
}
