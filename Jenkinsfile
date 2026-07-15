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
    string(name: 'SANITY_BUILD', defaultValue: '', description: 'This is a scheduled sanity build that skips releasing.')
  }

  tools {
    nodejs 'NodeJS 24'
  }

  options {
    timeout time: 1, unit: 'HOURS'
    timestamps()
    ansiColor 'xterm'
    disableConcurrentBuilds(abortPrevious: true)
    buildDiscarder(
      logRotator(
        numToKeepStr: env.BRANCH_NAME == DEFAULT_BRANCH ? '30' : '5',
        artifactNumToKeepStr: env.BRANCH_NAME == DEFAULT_BRANCH ? '30' : '5'
      )
    )
  }

  environment {
    RUSTUP_HOME = "${env.WORKSPACE}/.rustup"
    CARGO_HOME = "${env.WORKSPACE}/.cargo"
    FEATURE_TAG = slugify("${CURRENT_BRANCH}-${BUILD_NUMBER}")
    GIT_AUTHOR_NAME = 'Mezmo Bot'
    GIT_AUTHOR_EMAIL = 'bot@mezmo.com'
    GIT_COMMITTER_NAME = 'Mezmo Bot'
    GIT_COMMITTER_EMAIL = 'bot@mezmo.com'
    ENABLE_DOCKER = 'true'
    GITHUB_ACTION = 'yes'
    // Image tags produced by Build Images and consumed by compose; derived
    // once here so make and compose never re-derive the slug.
    AURA_SERVER_IMAGE = "local/aura-server:${BUILD_SLUG}"
    AURA_TEST_IMAGE = "local/aura-test:${BUILD_SLUG}"
    BUILD_CACHE_BUCKET = 'ci-oss-build-cache-483535019806-us-east-1-an'
    BUILD_CACHE_REGION = 'us-east-1'
  }


  post {
    always {
      script {
        jiraSendBuildInfo site: 'logdna.atlassian.net'
        archiveArtifacts allowEmptyArchive: true, artifacts: 'report/ci/**', caseSensitive: false, followSymlinks: false
        sh 'make clean'
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
    stage('Check CI run conditions') {
      when {
        beforeAgent true
        not {
          anyOf {
            branch DEFAULT_BRANCH
            changeRequest()
            triggeredBy cause: "UserIdCause"
          }
        }
      }

      steps {
        script {
          currentBuild.result = 'ABORTED'
          error("Aborting the build due to no open PR")
        }
      }
    }

    stage('Setup') {
      steps{
        sh 'make setup'
      }
    } // end setup

    stage('Build Images') {
      // Preloaded tags are consumed by the Test Suite stage.
      when {
        beforeAgent true
        not {
          changelog '\\[skip ci\\]'
        }
      }

      options {
        // Seed runs pay two cold dependency cooks plus S3 uploads.
        timeout(time: 50, unit: 'MINUTES')
      }

      environment {
        BUILD_CACHE_PREFIX = 'aura/buildkit/'
        CACHE_BUILDER = "aura-s3-${BUILD_SLUG}"
      }

      steps {
        script {
          // Authenticated pulls avoid Docker Hub rate limits on shared fleet egress.
          docker.withRegistry(
              'https://index.docker.io/v1/',
              'dockerhub-token-mezmo'
          ) {
            withCredentials([
              aws(
                credentialsId: 'oss-aws-build-cache',
                accessKeyVariable: 'AWS_ACCESS_KEY_ID',
                secretKeyVariable: 'AWS_SECRET_ACCESS_KEY'
              )
            ]) {
              sh 'make build-images'
            }
          }
        }
      }

      post {
        always {
          sh 'docker buildx rm "$CACHE_BUILDER" >/dev/null 2>&1 || true'
        }
      }
    }

    stage('ChangeSet Validation') {
      parallel {
        stage("Conventional Commit Check") {
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

        stage("Docker Lint") {
          steps {
            withChecks('hadolint') {
              sh script: "make lint-docker", returnStatus: true
              recordIssues( // needs to be in same block as withChecks
                tool: hadoLint(pattern: 'report/ci/hadolint.json'),
                id: 'hadolint',
                name: 'hadolint lint',
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
        }
      } // End Parallel
    } // End Validate

    stage('Test Suite') {
      when {
        beforeAgent true
        not {
          changelog '\\[skip ci\\]'
        }
      }

      parallel {
        stage('Integration Tests') {
          environment {
            MOCK_MCP_IMAGE = 'mezmo/aura-mock-mcp:latest'
            // The plain daemon cannot read the S3 layer cache, so a rebuild
            // would be a silent ~30min uncached build. --no-build consumes
            // the Build Images tags and fails fast if one is missing.
            COMPOSE_BUILD = '--no-build'
            // sccache for the in-container coverage compile; compose maps
            // this AURA_ toggle onto the cargo wrapper.
            AURA_RUSTC_WORKSPACE_WRAPPER = 'sccache'
            SCCACHE_BUCKET = "${BUILD_CACHE_BUCKET}"
            SCCACHE_REGION = "${BUILD_CACHE_REGION}"
            SCCACHE_S3_KEY_PREFIX = 'aura/sccache/'
          }
          steps {
            sh 'mkdir -p report'
            script {
              // Authenticated pulls avoid Docker Hub rate limits on shared fleet egress.
              docker.withRegistry(
                  'https://index.docker.io/v1/',
                  'dockerhub-token-mezmo'
              ) {
                withCredentials([
                  string(credentialsId: 'openai-api-key', variable: 'OPENAI_API_KEY'),
                  aws(
                    credentialsId: 'oss-aws-build-cache',
                    accessKeyVariable: 'AWS_ACCESS_KEY_ID',
                    secretKeyVariable: 'AWS_SECRET_ACCESS_KEY'
                  )
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

        // Cheap release preview: confirms commits parse and a version
        // computes, with no image build (that happens at main Release).
        // The file:// repo and plugin list keep it credential-free.
        stage('Release Dry Run') {
          when {
            beforeAgent true
            not {
              expression { CURRENT_BRANCH == DEFAULT_BRANCH }
            }
          }

          agent {
            node {
              label 'ec2-fleet-oss'
              customWorkspace("/tmp/workspace/${BUILD_SLUG}-dryrun")
            }
          }

          tools {
            nodejs 'NodeJS 24'
          }

          environment {
            GIT_BRANCH = "${CURRENT_BRANCH}"
            BRANCH_NAME = "${CURRENT_BRANCH}"
            CHANGE_ID = ''
            ENABLE_DOCKER = 'false'
          }

          steps {
            // package-lock=false in .npmrc means npm ci cannot be used.
            sh 'npm install'
            sh "git checkout -B ${CURRENT_BRANCH}"
            withReport('Release Test', "npm run release:dry -- --repository-url=file://${env.WORKSPACE} --plugins @semantic-release/commit-analyzer")
          }
        }
      }

      post {
        always {
          sh 'make test-integration-down'
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

      stages {
        stage('Compute Release Version') {
          steps {
            withCredentials(RELEASE_CREDENTIALS) {
              script {
                try {
                  sh 'npm run release:dry -- --plugins @semantic-release/commit-analyzer @semantic-release/exec'
                  env.NEXT_RELEASE_VERSION = fileExists('.next-release-version') ? readFile('.next-release-version').trim() : ''
                } finally {
                  sh 'rm -f .next-release-version'
                }
                echo env.NEXT_RELEASE_VERSION ? "Release version: ${env.NEXT_RELEASE_VERSION}" : 'No release version determined; skipping build'
              }
            }
          }
        }

        stage('Build Release Artifacts') {
          when {
            expression { env.NEXT_RELEASE_VERSION }
          }

          parallel {
            stage('Build Linux Artifacts') {
              environment {
                SCCACHE_BUCKET = "${BUILD_CACHE_BUCKET}"
                SCCACHE_REGION = "${BUILD_CACHE_REGION}"
                SCCACHE_S3_KEY_PREFIX = 'aura/sccache/'
              }
              steps {
                sh './scripts/set-version.sh "$NEXT_RELEASE_VERSION"'
                script {
                  withCredentials([
                    aws(
                      credentialsId: 'oss-aws-build-cache',
                      accessKeyVariable: 'AWS_ACCESS_KEY_ID',
                      secretKeyVariable: 'AWS_SECRET_ACCESS_KEY'
                    )
                  ]) {
                    // sccache reaches its S3 cache with the credentials bound
                    // above, so enable the wrapper only inside this block.
                    // Stage-wide it would route the credential-free
                    // set-version step through sccache and time out on the
                    // IMDS credential fallback.
                    withEnv(['AURA_RUSTC_WRAPPER=sccache']) {
                      sh 'make build-binaries-linux'
                    }
                  }
                }

                stash(
                  name: 'linux-release-artifacts',
                  includes: 'dist/**',
                  allowEmpty: false
                )
              }
            }

            stage('Build Darwin Artifacts') {
              agent {
                node {
                  label 'ec2-fleet-oss-macos'
                  customWorkspace("/tmp/workspace/${BUILD_SLUG}-darwin")
                }
              }

              environment {
                ENABLE_DOCKER = 'false'
              }

              steps {
                sh './scripts/set-version.sh "$NEXT_RELEASE_VERSION"'
                sh 'make build-binaries-darwin'

                stash(
                  name: 'darwin-release-artifacts',
                  includes: 'dist/**',
                  allowEmpty: false
                )
              }

              post {
                always {
                  sh 'make clean'
                }
              }
            }
          }
        }

        stage('Semantic Release') {
          when {
            expression { env.NEXT_RELEASE_VERSION }
          }

          steps {
            sh 'rm -rf dist'
            sh 'mkdir -p dist'

            unstash 'linux-release-artifacts'
            unstash 'darwin-release-artifacts'

            sh 'make verify-binaries'

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
  publishChecks(name: checkName, status: 'IN_PROGRESS', title: 'Running...')

  try {
    if (command) {
      sh script: "set -o pipefail; ${command} 2>&1 | tee ${logFile}"
    }
    if(callback) {
      callback()
    } else {
      publishChecks(name: checkName, conclusion: 'SUCCESS', summary: 'Check passed!')
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
      text: "### Console Output\n```\n${consoleOutput}\n```"
    )

    // throwing will trigger the FAILURE check state
    throw e
  } finally {
    sh "rm -f ${logFile}"
  }
}
