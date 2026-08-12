#!/usr/bin/env node
// Print the version semantic-release would release next, or nothing when there
// is no releasable change.
//
// Only commit-analyzer is loaded, so no release lifecycle command runs.
// semantic-release's logging goes to stderr, leaving stdout as the version.
//
// Usage: next-version.mjs [repository-url]

import semanticRelease from 'semantic-release'

const [repositoryUrl] = process.argv.slice(2)

// Mirrors the release:dry script: no CI detection, and the branch under test
// is the one that releases.
const options = {
  dryRun: true,
  ci: false,
  branches: [process.env.BRANCH_NAME || 'main'],
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
