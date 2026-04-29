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
def withReport(checkName, command) {
  def logFile = "output-${checkName.replaceAll(/[^a-zA-Z0-9]/, "_")}.log"
  publishChecks(name: checkName, status: 'IN_PROGRESS', title: 'Running...')

  try {
    sh script: "${command} 2>&1 | tee ${logFile}"
    publishChecks(name: checkName, conclusion: 'SUCCESS', summary: 'Check passed!')
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
        sh 'npm install'
      }
    }

    stage('Validate') {
      stages {
        stage("commitlint") {
          steps {
            sh "npm run commitlint"
          }
        }
      }
    }

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

          withCredentials([[
            $class: 'AmazonWebServicesCredentialsBinding',
            credentialsId: 'aws',
            accessKeyVariable: 'AWS_ACCESS_KEY_ID',
            secretKeyVariable: 'AWS_SECRET_ACCESS_KEY'
          ]]) {
            sh script: 'make clean'
            sh script: "make render RELEASE_VERSION=${FEATURE_TAG}", label: "Generate feature branch k8s Artifacts"
            sh script: "make publish RELEASE_VERSION=${FEATURE_TAG}", label: "Publish feature branch k8s Artifacts"
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

          withCredentials([[
            $class: 'AmazonWebServicesCredentialsBinding',
            credentialsId: 'aws',
            accessKeyVariable: 'AWS_ACCESS_KEY_ID',
            secretKeyVariable: 'AWS_SECRET_ACCESS_KEY'
          ]]) {
            sh script: 'make clean'
            sh script: "make render RELEASE_VERSION=${RELEASE_VERSION_PATCH}", label: "Generate k8s Artifacts"
            sh script: "make publish RELEASE_VERSION=${RELEASE_VERSION_PATCH}", label: "Publish k8s Artifacts"
          }
        }
      }
    }
  }
}
