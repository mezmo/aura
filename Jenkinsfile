import groovy.transform.Field
library 'magic-butler-catalogue'

def PROJECT_NAME = 'aura'
def DEFAULT_BRANCH = 'main'
def CURRENT_BRANCH = [env.CHANGE_BRANCH, env.BRANCH_NAME]?.find{branch -> branch != null}
def DOCKER_REPO = "docker.io/mezmo"
def BUILD_SLUG = slugify(env.BUILD_TAG)


def RELEASE_CREDENTIALS = [
   usernamePassword(
     credentialsId: 'github-app-key-mezmo',
     passwordVariable: 'GITHUB_TOKEN',
     usernameVariable: 'GITHUB_APP'
   )
]

pipeline {
  agent {
    node {
      label 'ec2-fleet-oss'
      customWorkspace("/tmp/workspace/${BUILD_SLUG}")
    }
  }

  parameters {
    string(name: 'SANITY_BUILD', defaultValue: '', description: 'This a scheduled sanity build that skips releasing.')
  }

  tools {
    nodejs 'NodeJS 24'
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
    HELP_URL = 'https://github.com/mezmo/aura/blob/main/CONTRIBUTING.md'
    GIT_AUTHOR_NAME = 'Mezmo Bot'
    GIT_AUTHOR_EMAIL = 'bot@mezmo.com'
    GIT_COMMITTER_NAME = 'Mezmo Bot'
    GIT_COMMITTER_EMAIL = 'bot@mezmo.com'
    ENABLE_DOCKER = 'true'
    GITHUB_ACTION = 'yes'
  }

  post {
    always {
      script {
        jiraSendBuildInfo site: 'logdna.atlassian.net'
        sh 'ls -alh -R report'
        archiveArtifacts allowEmptyArchive: true, artifacts: 'report/ci/**', caseSensitive: false, followSymlinks: false
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
                if (fileExists('report/ci/checkstyle.json')) {
                  def report = readJSON file: 'report/ci/checkstyle.json'
                  publishChecks(
                    name: report.name,
                    title: report.title,
                    summary: report.summary,
                    text: report.text,
                    conclusion: report.conclusion,
                    status: 'COMPLETED',
                  )

                  // Fail the Jenkins step if there are any ERROR severity issues
                  def errorCount = report.issues?.count { it.severity == 'ERROR' } ?: 0
                  if (errorCount > 0) {
                    throw new ManualCheckException("Commitlint check failed with ${errorCount} error(s)")
                  }
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
                tool: checkStyle(pattern: 'report/ci/rustfmt.xml'),
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
              sh script: "make lint-rust", returnStatus: true
              recordIssues( // needs to be in same block as withChecks
                tool: cargo(pattern: 'report/ci/clippy.json'),
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
            stage('Integration Tests') {
              environment {
                MOCK_MCP_IMAGE = 'mezmo/aura-mock-mcp:latest'
              }
              steps {
                sh 'mkdir -p report'
                withCredentials([
                  string(credentialsId: 'openai-api-key', variable: 'OPENAI_API_KEY'),
                ]) {
                  withReport('coverage', 'make test-integration') {
                    junit testResults: 'report/ci/junit.xml', allowEmptyResults: true
                    sh 'make clean-profile'
                    recordCoverage(
                      tools: [[parser: 'COBERTURA', pattern: 'report/ci/cobertura.xml']],
                      checksAnnotationScope: 'MODIFIED_LINES',
                      sourceDirectories: [[path: '.']],
                      enabledForFailure: true,
                      ignoreParsingErrors: true,
                      id: 'coverage',
                      name: 'coverage'
                      // qualityGates: [[threshold: 80.0, metric: 'LINE', baseline: 'PROJECT', unstable: false]]
                    )
                  }
                }
              }
              post {
                always {
                  publishHTML target: [
                    allowMissing: false,
                    alwaysLinkToLastBuild: false,
                    keepAll: true,
                    reportDir: 'report/ci/html',
                    reportFiles: '*.html',
                    reportName: "coverage-${BUILD_TAG}"
                  ]
                }
              }
            }

            stage('Release Test') {
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
                script {
                  docker.withRegistry(
                      'https://index.docker.io/v1/',
                      'dockerhub-token-mezmo'
                  ) {
                    withCredentials(RELEASE_CREDENTIALS) {
                      buildx {
                        withReport('Release Test', 'npm run release:dry')
                      }
                    }
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

    stage('Feature Build') {
      when {
        expression {
          CURRENT_BRANCH ==~ /feature\/(([A-Z]{2,5}-\d+.*)|aura-next(-.*)?)/
        }
      }

      steps {
        script {
          docker.withRegistry(
              'https://index.docker.io/v1/',
              'dockerhub-token-mezmo'
          ) {
            withReport('Feature Container Build', null, {
              buildx.build(
                  project: PROJECT_NAME
                , push: true
                , tags: [FEATURE_TAG]
                , dockerfile: "Dockerfile"
                , args: [RELEASE_VERSION: FEATURE_TAG]
                , docker_repo: DOCKER_REPO
                , tooling: true
              )
              publishChecks(
                  name: 'Feature Container Build'
                , conclusion: 'SUCCESS'
                , summary: 'Publish succeeded!'
              )
            })
          }
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
          docker.withRegistry(
              'https://index.docker.io/v1/',
              'dockerhub-token-mezmo'
          ) {
            withCredentials(RELEASE_CREDENTIALS) {
              buildx(
                tooling: true,
                project: PROJECT_NAME,
                versionFn: { -> npm.semver().version }
              ) {
                withReport('Release', 'npm run release')
              }
            }
          }
        }
      }
    }
  }
}

// Custom exception to signal that a check has already been published manually
// and withReport should not publish a console output check
class ManualCheckException extends Exception {
  ManualCheckException(String message) {
    super(message)
  }
}

/**
 * Execute a command and publish GitHub check results based on the outcome.
 *
 * This function wraps command execution with automatic GitHub check publishing.
 * It captures command output and publishes appropriate check results based on
 * success or failure.
 *
 * @param checkName String - The name of the GitHub check to publish
 * @param command String - The shell command to execute
 * @param callback Closure (optional) - A callback to execute after the command succeeds.
 *                 Used for custom check publishing when structured reports are available.
 *
 * Usage:
 *
 * 1. Simple usage (automatic check publishing):
 *    withReport('My Check', 'make test')
 *    // Publishes SUCCESS check if command succeeds, FAILURE with console output if it fails
 *
 * 2. Custom check publishing (with callback):
 *    withReport('Lint Check', 'make lint', {
 *      def report = readJSON file: 'report.json'
 *      publishChecks(
 *        name: report.name,
 *        summary: report.summary,
 *        conclusion: report.conclusion
 *      )
 *      // To fail the step after publishing a custom check, throw ManualCheckException
 *      if (report.hasErrors) {
 *        throw new ManualCheckException("Check failed with ${report.errorCount} errors")
 *      }
 *    })
 *
 * Error Handling:
 * - If the command fails, publishes a FAILURE check with console output and re-throws
 * - If the callback throws ManualCheckException, re-throws without publishing (check already published)
 * - If the callback throws any other exception, publishes FAILURE check with console output
 *
 * @throws ManualCheckException when callback signals check was already published
 * @throws Exception when command or callback fails
 */
def withReport(checkName, command, callback = null) {
  def logFile = "output-${checkName.replaceAll(/[^a-zA-Z0-9]/, "_")}.log"
  publishChecks(
    name: checkName,
    status: 'IN_PROGRESS',
    title: 'Running...',
    detailsURL: "${env.HELP_URL}"
  )

  try {
    if (command) {
      sh script: "${command} 2>&1 | tee ${logFile}"
    }
    if(callback) {
      callback()
    } else {
      publishChecks(
        name: checkName,
        conclusion: 'SUCCESS',
        summary: 'Check passed!',
        detailsURL: "${env.HELP_URL}"
      )
    }
  } catch (ManualCheckException e) {
    // Check was already published by callback, just re-throw to fail the step
    throw e
  } catch (Exception e) {
    def consoleOutput = readFile(logFile).trim()
    publishChecks(
      name: checkName,
      conclusion: 'FAILURE',
      summary: "Command failed: ${e.message}",
      text: "### Console Output\n```\n${consoleOutput}\n```",
      detailsURL: "${env.HELP_URL}"
    )

    // throwing will trigger the FAILURE check state
    throw e
  } finally {
    sh "rm -f ${logFile}"
  }
}
