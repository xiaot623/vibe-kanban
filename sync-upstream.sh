#!/bin/bash

# Sync Upstream Script
# Automates synchronization from upstream repository to local main and release branches
#
# Usage:
#   ./sync-upstream.sh [options]
#
# Options:
#   -y, --yes                   Auto-update release branch without prompting
#   -n, --no                    Skip release branch update
#   --skip-uncommitted-check    Skip uncommitted changes check
#   -h, --help                  Show this help message

set -e  # Exit on error

# Parse command line arguments
UPDATE_RELEASE=""
SKIP_UNCOMMITTED_CHECK=false

while [[ $# -gt 0 ]]; do
    case $1 in
        -y|--yes)
            UPDATE_RELEASE="yes"
            shift
            ;;
        -n|--no)
            UPDATE_RELEASE="no"
            shift
            ;;
        --skip-uncommitted-check)
            SKIP_UNCOMMITTED_CHECK=true
            shift
            ;;
        -h|--help)
            echo "Usage: $0 [options]"
            echo ""
            echo "Options:"
            echo "  -y, --yes                   Auto-update release branch without prompting"
            echo "  -n, --no                    Skip release branch update"
            echo "  --skip-uncommitted-check    Skip uncommitted changes check"
            echo "  -h, --help                  Show this help message"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            echo "Use -h or --help for usage information"
            exit 1
            ;;
    esac
done

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Function to print colored output
print_status() {
    echo -e "${BLUE}==>${NC} $1"
}

print_success() {
    echo -e "${GREEN}✓${NC} $1"
}

print_warning() {
    echo -e "${YELLOW}⚠${NC} $1"
}

print_error() {
    echo -e "${RED}✗${NC} $1"
}

# Check if we're in a git repository
if ! git rev-parse --git-dir > /dev/null 2>&1; then
    print_error "Not a git repository"
    exit 1
fi

# Store current branch
CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
print_status "Current branch: $CURRENT_BRANCH"

# Check for uncommitted changes
if [ "$SKIP_UNCOMMITTED_CHECK" = false ] && ! git diff-index --quiet HEAD --; then
    print_warning "You have uncommitted changes"
    read -p "Continue anyway? (y/n) " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        print_error "Aborted"
        exit 1
    fi
fi

# Fetch from upstream
print_status "Fetching from upstream..."
if git fetch upstream; then
    print_success "Fetched from upstream"
else
    print_error "Failed to fetch from upstream"
    exit 1
fi

# Sync main branch
print_status "Syncing main branch with upstream/main..."
git checkout main

# Try fast-forward merge
if git merge upstream/main --ff-only; then
    print_success "Main branch synced with upstream/main (fast-forward)"

    # Push to origin
    print_status "Pushing main to origin..."
    if git push origin main; then
        print_success "Pushed main to origin"
    else
        print_error "Failed to push main to origin"
        git checkout "$CURRENT_BRANCH"
        exit 1
    fi
else
    print_error "Cannot fast-forward main branch"
    print_warning "Your main branch has diverged from upstream"
    print_warning "This should not happen - main should be kept clean"
    git checkout "$CURRENT_BRANCH"
    exit 1
fi

# Ask if user wants to update release branch (if not specified via args)
if [ -z "$UPDATE_RELEASE" ]; then
    echo
    read -p "Update release branch with latest main? (y/n) " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        UPDATE_RELEASE="yes"
    else
        UPDATE_RELEASE="no"
    fi
fi

if [ "$UPDATE_RELEASE" = "yes" ]; then
    print_status "Updating release branch..."

    # Check if release branch exists
    if git rev-parse --verify release > /dev/null 2>&1; then
        git checkout release

        if git merge main; then
            print_success "Merged main into release"

            # Push to origin
            print_status "Pushing release to origin..."
            if git push origin release; then
                print_success "Pushed release to origin"
            else
                print_error "Failed to push release to origin"
                git checkout "$CURRENT_BRANCH"
                exit 1
            fi
        else
            print_error "Merge conflicts detected"
            print_warning "Please resolve conflicts manually and run:"
            print_warning "  git add <resolved-files>"
            print_warning "  git commit"
            print_warning "  git push origin release"
            exit 1
        fi
    else
        print_error "Release branch does not exist"
    fi
else
    print_status "Skipping release branch update"
fi

# Return to original branch
if [ "$CURRENT_BRANCH" != "$(git rev-parse --abbrev-ref HEAD)" ]; then
    print_status "Returning to $CURRENT_BRANCH..."
    git checkout "$CURRENT_BRANCH"
fi

echo
print_success "Sync complete!"
echo
print_status "Summary:"
git log --oneline -5
