library 'magic-butler-catalogue'
def PROJECT_NAME = 'aura'
def DEFAULT_BRANCH = 'main'
def CURRENT_BRANCH = [env.CHANGE_BRANCH, env.BRANCH_NAME]?.find{branch -> branch != null}
def TRIGGER_PATTERN = '.*@triggerbuild.*'
def DOCKER_REPO = "docker.io/mezmo"
def BUILD_SLUG = slugify(env.BUILD_TAG)

// FIXME: Temporary workaround until aura is updated
// to produce structured output / reports
// This is just here to help simplify the pipeline workflow
def withReport(checkName, command, callback = null) {
  def logFile = "output-${checkName.replaceAll(/[^a-zA-Z0-9]/, "_")}.log"
  publishChecks(name: checkName, status: 'IN_PROGRESS', title: 'Running...')

  try {
    sh script: "${command} 2>&1 | tee ${logFile}"
    if(callback) {
      callback()
    } else {
      publishChecks(name: checkName, conclusion: 'SUCCESS', summary: 'Check passed!')
    }
  } catch (Exception e) {
    def consoleOutput = readFile(logFile).trim()
    publishChecks(
      name: checkName,
      conclusion: 'FAILURE',
      summary: "Command failed: ${e.message}",
      text: "### Console Output\n```\n${consoleOutput}\n```"
    )

    // throwing will trigger the FAILURE check state
    throw e
  } finally {
    sh "rm -f ${logFile}"
  }
}

pipeline {
  agent {
    node {
      label 'ec2-fleet'
      customWorkspace("/tmp/workspace/${BUILD_SLUG}")
    }
  }

  parameters {
    string(name: 'SANITY_BUILD', defaultValue: '', description: 'This a scheduled sanity build that skips releasing.')
  }

  tools {
    nodejs 'NodeJS 24'
  }

  triggers {
    issueCommentTrigger(TRIGGER_PATTERN)
  }

  options {
    timeout time: 1, unit: 'HOURS'
    timestamps()
    ansiColor 'xterm'
  }

  environment {
    RUSTUP_HOME = "${env.WORKSPACE}/.rustup"
    CARGO_HOME = "${env.WORKSPACE}/.cargo"
    FEATURE_TAG = slugify("${CURRENT_BRANCH}-${BUILD_NUMBER}")
  }

  post {
    always {
      script {
        jiraSendBuildInfo site: 'logdna.atlassian.net'
        archiveArtifacts allowEmptyArchive: true, artifacts: 'target/ci/reports/**', caseSensitive: false, followSymlinks: false
        sh: 'make clean'
        if (env.SANITY_BUILD == 'true') {
          notifySlack(
            currentBuild.currentResult,
            [
              channel: '#proj-ai-chatbot',
              tokenCredentialId: 'qa-slack-token'
            ],
            "`${PROJECT_NAME}` sanity build took ${currentBuild.durationString.replaceFirst(' and counting', '')}."
          )
        }
      }
    }
  }

  stages {
    stage('Validate PR Source') {
      when {
        expression { env.CHANGE_FORK }
        not {
          triggeredBy 'issueCommentCause'
        }
      }
      steps {
        error("A maintainer needs to approve this PR for CI by commenting")
      }
    }

    stage('Setup') {
      steps{
        sh 'make setup'
      }
    } // end setup

    stage('ChangeSet Validation') {
      parallel {
        stage("Convention Commit Check") {
          steps {
            script {
              // There isn't an actual file in the repo for github to associate errors / annotations with
              // so we have to report a little more manually for commitlint
              withReport('Commitlint', 'make lint-commits', {
                if (fileExists('target/ci/reports/checkstyle.json')) {
                  def report = readJSON file: 'target/ci/reports/checkstyle.json'
                  publishChecks(
                    name: report.name,
                    title: report.title,
                    summary: report.summary,
                    text: report.text,
                    conclusion: report.conclusion,
                    status: 'COMPLETED',
                  )
                }
              })
            }
          }
        } // End Commitlint

        stage("Style Check") {
          steps {
            withChecks('rustfmt') {
              sh "make fmt-rust"
              recordIssues( // needs to be in the same block as withChecks
                tool: checkStyle(pattern: 'target/ci/reports/rustfmt.xml'),
                id: 'rustfmt',
                name: 'rustfmt',
                enabledForFailure: true,
                sourceDirectories: [[path: '.']],
                checksAnnotationScope: 'ALL',
                qualityGates: [[threshold: 1, type: 'TOTAL', criticality: 'FAILURE']]
              )
            }
          }
        } // end rustfmt

        stage("Rust Lint") {
          steps {
            withChecks('clippy') {
              sh "make lint-rust || true"
              recordIssues( // needs to be in same block as withChecks
                tool: cargo(pattern: 'target/ci/reports/clippy.json'),
                id: 'clippy',
                name: 'clippy lint',
                enabledForFailure: true,
                sourceDirectories: [[path: '.']],
                checksAnnotationScope: 'ALL',
                qualityGates: [[
                  threshold: 1,
                  type: 'TOTAL',
                  criticality: 'FAILURE'
                ]]
              )
            }
          }
        } // End Clippy
      } // End Parallel
    } // End Validate

    stage('Test Suite') {
      when {
        beforeAgent true
        not {
          changelog '\\[skip ci\\]'
        }
      }

      stages {
        stage('Test Suite: Parallel') {
          parallel {
            stage('Unit Tests') {
              steps {
                // FIXME. It looks like this doesn't do anything the other tests aren't doing
                // This seems frivolous
                withReport('Unit Tests', 'docker build --target release-build .')
              }
            }

            stage('Integration Tests') {
              environment {
                MOCK_MCP_IMAGE = 'mezmo/aura-mock-mcp:latest'
              }
              steps {
                withCredentials([
                  string(credentialsId: 'openai-api-key', variable: 'OPENAI_API_KEY'),
                ]) {
                  withReport('Integration Tests', 'make test-integration')
                }
              }
            }

            stage('Relese Test') {
              when {
                not {
                  expression { CURRENT_BRANCH == DEFAULT_BRANCH }
                }
              }

              environment {
                 GIT_BRANCH = "${CURRENT_BRANCH}"
                 BRANCH_NAME = "${CURRENT_BRANCH}"
                 CHANGE_ID = ''
              }

              steps {
                withCredentials([
                   string(credentialsId: 'github-api-token', variable: 'GITHUB_TOKEN'),
                ]) {
                  buildx {
                    withReport('Release Test', 'npm run release:dry')
                  }
                }
              }
            }
          }

          post {
            always {
              sh 'make test-integration-down'
            }
          }
        }
      }
    }

    // FIXME: This needs to be removed with the removal of razee stuff
    stage('Feature Build') {
      when {
        expression {
          CURRENT_BRANCH ==~ /feature\/(([A-Z]{2,5}-\d+.*)|aura-next(-.*)?)/
        }
      }

      steps {
        script {
          buildx.build(
            project: PROJECT_NAME
          , push: true
          , tags: [FEATURE_TAG]
          , dockerfile: "Dockerfile"
          , args: [RELEASE_VERSION: FEATURE_TAG]
          , docker_repo: DOCKER_REPO
          )
        }
      }
    }

    stage('Release') {
      when {
        beforeAgent true
        branch DEFAULT_BRANCH
        not {
          anyOf {
            changelog '\\[skip ci\\]';
            environment name: 'SANITY_BUILD', value: 'true'
          }
        }
      }

      steps {
        script {

          // FIXME: We shouldn't need to do this
          // once the release version has been updated running make clean render publish
          // should resolve the right version. I think this can also go
          // 1.2.3
          def RELEASE_VERSION_PATCH = sh(
            returnStdout: true,
            script: 'cargo metadata -q --no-deps --format-version 1 | jq -r \'.packages[0].version\''
          ).trim()

          withCredentials([
             string(credentialsId: 'github-api-token', variable: 'GITHUB_TOKEN'),
          ]) {
            buildx {
              withReport('Release', 'npm run release')
            }
          }
        }
      }
    }
  }
}
