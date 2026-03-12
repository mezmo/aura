library 'magic-butler-catalogue'
def PROJECT_NAME = 'aura'
def DEFAULT_BRANCH = 'main'
def CURRENT_BRANCH = [env.CHANGE_BRANCH, env.BRANCH_NAME]?.find{branch -> branch != null}
def TRIGGER_PATTERN = '.*@logdnabot.*'
def DOCKER_REPO = "docker.io/mezmo"

pipeline {
  agent {
    node {
      label 'ec2-fleet'
      customWorkspace("/tmp/workspace/${env.BUILD_TAG}")
    }
  }

  parameters {
    string(name: 'SANITY_BUILD', defaultValue: '', description: 'This a scheduled sanity build that skips releasing.')
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
    GITHUB_TOKEN = credentials('github-api-token')
    NPM_CONFIG_CACHE = '.npm'
    NPM_CONFIG_USERCONFIG = '.npm/rc'
    SPAWN_WRAP_SHIM_ROOT = '.npm'
    RUSTUP_HOME = '/opt/rust/cargo'
    CARGO_HOME = '/opt/rust/cargo'
    PATH = """${sh(
       returnStdout: true,
       script: 'echo /opt/rust/cargo/bin:\$PATH'
    )}
    """
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
    stage('Validate') {
      tools {
        nodejs 'NodeJS 20'
      }

      steps {
        script {
          sh "mkdir -p ${NPM_CONFIG_CACHE}"
          npm.auth token: GITHUB_TOKEN
          sh "npx @answerbook/commitlint-config-logdna"
        }
      }
    }

    stage('Test') {
      when {
        beforeAgent true
        not {
          changelog '\\[skip ci\\]'
        }
      }

      parallel {
        stage('Unit Tests') {
          steps {
            script {
              sh(script: 'docker build --target release-build .')
            }
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
              sh 'make test-integration'
            }
          }
          post {
            always {
              sh 'make test-integration-down'
            }
          }
        }

        stage('Release Tests') {
          when {
            beforeAgent true
            not {
              branch DEFAULT_BRANCH
            }
          }

          environment {
            GIT_BRANCH = "${CURRENT_BRANCH}"
            BRANCH_NAME = "${CURRENT_BRANCH}"
            CHANGE_ID = ""
          }

          tools {
            nodejs 'NodeJS 20'
          }

          steps {
            script {
              sh "mkdir -p ${NPM_CONFIG_CACHE}"
              npm.auth token: GITHUB_TOKEN
              // Trigger rustup to read rust-toolchain.toml and auto-install the specified nightly
              // Running any cargo command will cause rustup to install the toolchain if not present
              sh 'echo "Cargo version:" && cargo --version'
              sh 'npm install -G semantic-release@^19.0.0 @semantic-release/git@10.0.1 @semantic-release/changelog@6.0.3 @semantic-release/exec@6.0.3 @answerbook/release-config-logdna@2.0.0'
              sh 'npx semantic-release --dry-run --no-ci --branches=${BRANCH_NAME:-main}'
            }
          }
        }
      }
    }

    stage('Feature Build') {
      when {
        expression {
          CURRENT_BRANCH ==~ /feature\/((.*)|aura-next(-.*)?)/
        }
      }

      tools {
        nodejs 'NodeJS 20'
      }

      steps {
        script {
          sh "mkdir -p ${NPM_CONFIG_CACHE}"
          npm.auth token: GITHUB_TOKEN
          sh 'npm install -G semantic-release@^19.0.0 @semantic-release/git@10.0.1 @semantic-release/changelog@6.0.3 @semantic-release/exec@6.0.3 @answerbook/release-config-logdna@2.0.0'
          sh 'npx semantic-release'

          def RELEASE_VERSION = FEATURE_TAG

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
            sh script: "make render RELEASE_VERSION=${RELEASE_VERSION}", label: "Generate feature branch k8s Artifacts"
            sh script: "make publish RELEASE_VERSION=${RELEASE_VERSION}", label: "Publish feature branch k8s Artifacts"
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

      tools {
        nodejs 'NodeJS 20'
      }

      steps {
        script {
          sh "mkdir -p ${NPM_CONFIG_CACHE}"
          npm.auth token: GITHUB_TOKEN
          sh 'npm install -G semantic-release@^19.0.0 @semantic-release/git@10.0.1 @semantic-release/changelog@6.0.3 @semantic-release/exec@6.0.3 @answerbook/release-config-logdna@2.0.0'
          sh 'npx semantic-release'

          // 1.2.3
          def RELEASE_VERSION_PATCH = sh(
            returnStdout: true,
            script: 'cargo metadata -q --no-deps --format-version 1 | jq -r \'.packages[0].version\''
          ).trim()
          // 1.2
          def RELEASE_VERSION_MINOR = RELEASE_VERSION_PATCH.tokenize('.').take(2).join('.')
          // 1
          def RELEASE_VERSION_MAJOR = RELEASE_VERSION_PATCH.tokenize('.')[0]

          buildx.build(
            project: PROJECT_NAME
          , push: true
          , tags: ['latest', RELEASE_VERSION_PATCH, RELEASE_VERSION_MINOR, RELEASE_VERSION_MAJOR]
          , dockerfile: "Dockerfile"
          , args: [RELEASE_VERSION: RELEASE_VERSION_PATCH]
          , docker_repo: DOCKER_REPO
          )

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
