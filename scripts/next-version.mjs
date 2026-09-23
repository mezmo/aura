#!/usr/bin/env node
// Print the version semantic-release would release next, or nothing when there
// is no releasable change.
//
// Only commit-analyzer is loaded, so no release lifecycle command runs.
// semantic-release's logging goes to stderr, leaving stdout as the version.
//
// Usage: next-version.mjs [repository-url]

import {createRequire} from 'node:module'
import semanticRelease from 'semantic-release'

const branchName = process.env.BRANCH_NAME || 'main'
process.env.BRANCH_NAME = branchName

const require = createRequire(import.meta.url)
const {branches} = require('../release.config.js')

const [repositoryUrl] = process.argv.slice(2)
const configured = branches.map((branch) => {
  return typeof branch === 'string' ? branch : branch.name
})

// A channel branch is analysed against the whole list so a prerelease derives
// from the last release on main; any other branch on its own, which is what
// makes a feature branch under test releasable.
const options = {
  dryRun: true,
  ci: false,
  branches: configured.includes(branchName) ? branches : [branchName],
  plugins: ['@semantic-release/commit-analyzer'],
}
if (repositoryUrl) {
  options.repositoryUrl = repositoryUrl
}

const result = await semanticRelease(options, {
  stdout: process.stderr,
  stderr: process.stderr,
})

if (result) {
  console.log(result.nextRelease.version)
}
