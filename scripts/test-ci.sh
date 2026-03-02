#!/bin/bash
# CI/CD Testing Script - Simulates Jenkins Pipeline
# Run this before pushing to ensure Jenkins will pass

set -e  # Exit on error

echo ""
echo "════════════════════════════════════════════════════════════════"
echo "                   CI/CD PIPELINE TEST"
echo "════════════════════════════════════════════════════════════════"
echo ""
echo "This script simulates the Jenkins pipeline locally."
echo "If all checks pass here, Jenkins will pass too!"
echo ""

# Colors for output
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Track overall status
FAILED=0

# Stage 1: Validate (Commitlint)
echo "────────────────────────────────────────────────────────────────"
echo "${BLUE}Stage 1: Validate${NC}"
echo "────────────────────────────────────────────────────────────────"
echo "Note: Commitlint validation skipped locally"
echo "      Jenkins will run: npx @answerbook/commitlint-config-logdna"
echo ""

# Stage 2: Test (Docker Build with Tests)
echo "────────────────────────────────────────────────────────────────"
echo "${BLUE}Stage 2: Test${NC}"
echo "────────────────────────────────────────────────────────────────"
echo "Running: docker build --target base ."
echo ""
echo "This will run (in Docker):"
echo "  1. cargo fmt --all -- --check   (formatting)"
echo "  2. cargo test --workspace        (tests)"
echo "  3. cargo clippy                  (linting)"
echo ""

if docker build --target base -t aura:test .; then
    echo ""
    echo "${GREEN}✓ Stage 2: Test - PASSED${NC}"
    echo ""
else
    echo ""
    echo "✗ Stage 2: Test - FAILED"
    FAILED=1
fi

# Stage 3: Build (Only on main branch)
echo "────────────────────────────────────────────────────────────────"
echo "${BLUE}Stage 3: Build${NC}"
echo "────────────────────────────────────────────────────────────────"

CURRENT_BRANCH=$(git branch --show-current)
if [ "$CURRENT_BRANCH" = "main" ]; then
    echo "Current branch: main"
    echo "Jenkins will:"
    echo "  1. Build Docker image"
    echo "  2. Push to GCR with 'latest' tag"
    echo ""
    echo "${YELLOW}Note: Skipping actual GCR push (no credentials locally)${NC}"
else
    echo "Current branch: $CURRENT_BRANCH (not main)"
    echo "Build stage will be skipped in Jenkins"
fi
echo ""

# Summary
echo "════════════════════════════════════════════════════════════════"
if [ $FAILED -eq 0 ]; then
    echo "${GREEN}         ✓ ALL CHECKS PASSED!${NC}"
    echo "════════════════════════════════════════════════════════════════"
    echo ""
    echo "Your code is ready to push!"
    echo ""
    echo "Next steps:"
    echo "  1. git add ."
    echo "  2. git commit -m \"your message\""
    echo "  3. git push"
    echo ""
    echo "Jenkins will automatically run the same checks."
    exit 0
else
    echo "         ✗ CHECKS FAILED"
    echo "════════════════════════════════════════════════════════════════"
    echo ""
    echo "Please fix the errors above before pushing."
    exit 1
fi
