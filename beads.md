# Beads CLI - Workflow Guide

## Workflow 1: Create Issue

### Step 1: Ask Clarifying Questions

Before creating any issue:

```
1. Is this an epic (large feature) or a single task?
2. Are there subtasks or sub-features involved?
3. What labels/tags should organize this work?
4. Does this depend on or block other issues?
5. What priority? (low, medium, high, urgent)
```

### Step 2: Create Epic (if applicable)

```bash
# Create the epic issue
beads issue create \
  -t "Epic: Feature Name" \
  --kind task \
  -l epic,backend,v2.0 \
  --priority high \
  --description "High-level description of the feature set"

# Returns: ep-123
```

### Step 3: Create Subtasks

```bash
# Create subtask 1
beads issue create \
  -t "Subtask 1: Database schema" \
  --kind task \
  --depends-on ep-123 \
  -l backend,database \
  --priority high

# Returns: ep-124

# Create subtask 2
beads issue create \
  -t "Subtask 2: API endpoints" \
  --kind feature \
  --depends-on ep-123 \
  -l backend,api \
  --priority high

# Returns: ep-125

# Create subtask 3
beads issue create \
  -t "Subtask 3: Frontend UI" \
  --kind feature \
  --depends-on ep-123 \
  -l frontend \
  --priority medium

# Returns: ep-126
```

### Step 4: Link Dependencies Between Subtasks

```bash
# If ep-125 depends on ep-124 being done first
beads dep add ep-125 ep-124

# If ep-126 needs both ep-124 and ep-125 complete
beads dep add ep-126 ep-124
beads dep add ep-126 ep-125

# View the dependency graph
beads issue list --dep-graph
```

### Step 5: Verify Structure

```bash
# Show the epic with all its dependencies
beads dep show ep-123

# Show what a specific subtask depends on
beads dep show ep-125
```

## Workflow 2: Pick Issue to Work

### Step 1: Check for In-Progress Issues

```bash
# List all issues currently being worked on
beads issue list --status in_progress
```

### Step 2: Handle Existing In-Progress Issues

If issues are in-progress, ask user:

```
"You have the following in-progress:
- rp-123: Feature X
- rp-124: Bug Y

Do you want to:
1. Continue with one of these?
2. Switch to a different issue (the in-progress will remain)?
3. Mark one as closed?"
```

### Step 3: Find Next Issue to Work On

```bash
# Show next issues grouped by priority
beads ready

# Or search for specific issue
beads search "keyword"

# Or show specific issue details
beads issue show <id>

# List open issues created in last week
beads issue list --created-after "1 week ago"

# List all open issues in a label
beads issue list --label backend
```

### Step 4: Mark Issue as In-Progress

```bash
# Mark selected issue as being worked on
beads issue update rp-123 --status in-progress

# Confirm
beads issue show rp-123
```

## Workflow 3: Complete Issue

### Step 1: Finish Work and Create Commit

```bash
# After implementation is complete, create a commit
# Reference the issue ID in the conventional commit message

git commit -m "feat: implement user authentication (rp-123)"
git commit -m "fix: resolve login redirect bug (rp-456)"
git commit -m "refactor: optimize database queries (rp-789)"

# Get the commit hash
COMMIT_HASH=$(git rev-parse --short HEAD)
echo $COMMIT_HASH  # e.g., abc1234d
```

### Step 2: Update Issue with Commit Hash

```bash
# Add commit hash to the issue using JSON
COMMIT=$(git rev-parse --short HEAD)
beads issue update rp-123 --data "{\"description\":\"Committed as: $COMMIT\"}"

# Or with a more detailed description
beads issue update rp-123 --data "{\"description\":\"Implementation complete - merged in commit: $(git rev-parse --short HEAD)\"}"

# View the updated issue
beads issue show rp-123
```

**Note**: The `--data` flag requires valid JSON. Common fields are:
- `description`: A string describing the work done
- `title`: Update the issue title
- `priority`: Set priority (0-3)
- `status`: Change status (open|in_progress|review|closed)
- `kind`: Change kind (bug|feature|refactor|docs|chore|task)

### Step 3: Mark Issue as Ready for Review (optional)

```bash
# If code review is needed before closing
beads issue update rp-123 --status review

# View all issues ready for review
beads issue list --status review
```

### Step 4: Close Issue

```bash
# Mark issue as closed after approval/merge
beads issue update rp-123 --status closed

# If this was a subtask, check if epic is complete
beads dep show ep-123  # Shows all subtasks
```

### Step 5: Close Dependent Issues

```bash
# If closing an epic with all subtasks done
# Check for any remaining open subtasks
beads issue list --status open --label epic

# Close them systematically
beads issue update ep-124 --status closed
beads issue update ep-125 --status closed
beads issue update ep-126 --status closed

# Finally close the epic
beads issue update ep-123 --status closed
```

## Workflow 4: Git & Conventional Commits

### Commit Message Format

Always reference the beads issue ID in your commit messages:

```bash
# Feature implementation
git commit -m "feat: add user profile page (rp-123)"

# Bug fix
git commit -m "fix: prevent null pointer exception (rp-456)"

# Refactoring
git commit -m "refactor: extract database logic to service (rp-789)"

# Documentation
git commit -m "docs: add authentication guide (rp-234)"

# Chore/maintenance
git commit -m "chore: upgrade dependencies (rp-567)"

# Multiple lines for detailed commit
git commit -m "feat: implement two-factor authentication (rp-890)

- Add TOTP support with QR code generation
- Store recovery codes in database
- Add backup email verification
- Tests for all scenarios"
```

### Linking Commits to Issues

```bash
# After pushing commits, update the beads issue
LATEST_COMMIT=$(git log -1 --pretty=format:"%H" | cut -c 1-7)

beads issue update rp-123 --data "Merged in: $LATEST_COMMIT"

# Or with full commit details
beads issue update rp-123 \
  --data "Commits: $(git log --oneline ep-123..HEAD | grep rp-123)"
```

### Complete Commit Workflow Example

```bash
# 1. Pick issue and start work
beads ready
beads issue update rp-123 --status in-progress

# 2. Make code changes
# ... edit files ...

# 3. Commit with issue reference
git add .
git commit -m "feat: implement email verification (rp-123)"

# 4. Push to remote
git push origin feature-branch

# 5. Mark for review
beads issue update rp-123 --status review

# 6. After merge/approval, close issue
beads issue update rp-123 --status closed --data "Merged in: 7a2f4c1"
```

## Useful Commands

```bash
# List open issues (with date filter)
beads issue list
beads issue list --created-after "1 week ago"

# List by status
beads issue list --status open
beads issue list --status in_progress --status review

# Filter by labels
beads issue list --label epic
beads issue list --label bug,urgent

# Show issue dependencies
beads dep show <id>

# Delete issue (soft delete)
beads delete <id> --force
beads delete <id> --cascade  # Delete with dependents
```

## Valid Values

**Status**: `open`, `in-progress`, `review`, `closed`

**Priority**: `low`, `medium`, `high`, `urgent`

**Kind**: `bug`, `feature`, `refactor`, `docs`, `chore`, `task`
