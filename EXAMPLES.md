# testrepository Examples

This document provides practical examples of using testrepository with various test frameworks and configurations.

## Basic .testr.conf Examples

### Python unittest

```ini
[DEFAULT]
test_command=python -m subunit.run discover -t . -s tests $LISTOPT
test_id_list_default=tests
test_list_option=--list
```

### Python pytest with subunit

```ini
[DEFAULT]
test_command=pytest --subunit-trace $IDOPTION
test_id_option=--test-list=$IDFILE
test_list_option=--collect-only -q
```

### Rust with cargo-subunit

```ini
[DEFAULT]
test_command=cargo test --quiet -- --format=subunit $LISTOPT
test_list_option=--list
```

### Node.js with tape and tap-subunit

```ini
[DEFAULT]
test_command=node test/*.js | tap-subunit
test_id_list_default=test/
```

## Advanced Configurations

### Test Grouping by Module

```ini
[DEFAULT]
test_command=python -m subunit.run $IDOPTION
test_id_option=$IDLIST
group_regex=^(.*\.)?(?P<module>[^.]+)\.
```

This groups tests by their module name, useful for running related tests together.

### Custom Test Discovery

```ini
[DEFAULT]
test_command=./scripts/run-tests.sh $IDOPTION
test_id_option=--tests=$IDFILE
test_id_list_default=all
```

### Dynamic Concurrency

```ini
[DEFAULT]
test_command=python -m subunit.run $IDOPTION
test_id_option=$IDLIST
# Automatically detect CPU count
test_run_concurrency=nproc
```

For more complex scenarios:

```ini
[DEFAULT]
test_command=python -m subunit.run $IDOPTION
test_id_option=$IDLIST
# Custom script to determine concurrency
test_run_concurrency=./scripts/get-worker-count.sh
```

### Instance Provisioning

For tests that need isolated environments (e.g., separate databases, ports):

```ini
[DEFAULT]
test_command=python -m subunit.run $IDOPTION
test_id_option=$IDLIST
# Provision N test databases and return their IDs
instance_provision=./scripts/provision-db.sh $INSTANCE_COUNT
# Execute tests against a specific instance
instance_execute=DB_ID=$INSTANCE_ID python -m subunit.run $IDOPTION
# Clean up the test database
instance_dispose=./scripts/dispose-db.sh $INSTANCE_ID
```

The provision script should output one instance ID per line:
```bash
#!/bin/bash
# provision-db.sh
for i in $(seq 1 $1); do
    db_id="test-db-$i"
    # Create database
    createdb $db_id
    echo $db_id
done
```

The dispose script receives each instance ID:
```bash
#!/bin/bash
# dispose-db.sh
dropdb $1
```

## Common Usage Patterns

### Initial Setup

```bash
# Initialize repository
testr init

# Run all tests
testr run

# View results
testr last
```

### Debugging Failures

```bash
# Run only failing tests
testr run --failing

# Run tests in isolation to find interactions
testr run --failing --isolated

# Analyze which tests cause isolation failures
testr analyze-isolation my_module.test_flaky

# Run until failure to catch flaky tests
testr run --until-failure
```

### Performance Testing

```bash
# Run tests in parallel
testr run -j 4

# View slowest tests
testr slowest

# View all test timings
testr slowest --all
```

### Continuous Integration

```bash
# Run tests and create repository if needed
testr run --force-init

# Run subset of tests from a file
testr run --load-list changed-tests.txt

# Get statistics
testr stats
```

### Advanced Workflows

```bash
# Parallel execution until failure (stress testing)
testr run -j 8 --until-failure

# Isolated execution of failing tests
testr run --failing --isolated

# Partial runs (additive failing test tracking)
testr run --partial

# Get list of failing test IDs for scripting
testr failing --list > failing.txt
```

## Integration Examples

### GitHub Actions

```yaml
name: Tests
on: [push, pull_request]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v2
      - name: Install dependencies
        run: |
          pip install subunit python-subunit
          cargo install testr
      - name: Run tests
        run: testr run --force-init -j 4
      - name: Show results
        if: always()
        run: testr last
```

### GitLab CI

```yaml
test:
  script:
    - testr run --force-init
    - testr stats
  artifacts:
    when: always
    paths:
      - .testrepository/
```

### Pre-commit Hook

```bash
#!/bin/bash
# .git/hooks/pre-commit

# Run only changed tests
git diff --cached --name-only --diff-filter=AM | \
  grep test_ | \
  sed 's/\.py$//' | \
  tr '/' '.' > /tmp/tests.txt

if [ -s /tmp/tests.txt ]; then
    testr run --load-list /tmp/tests.txt
fi
```

## Performance Optimization

### Balancing Parallel Workers

The optimal number of workers depends on your test suite:

```bash
# CPU-bound tests: use core count
testr run -j $(nproc)

# I/O-bound tests: use more workers
testr run -j $(($(nproc) * 2))

# Mixed workload: start conservative
testr run -j 4
```

### Test Duration Tracking

testrepository automatically tracks test durations in `.testrepository/times.dbm`:

```bash
# First run (no timing data)
testr run -j 4

# Second run (uses timing for better load balancing)
testr run -j 4
```

The second run will distribute tests more evenly based on historical durations.

## Troubleshooting

### Tests Don't Run

```bash
# Check configuration
cat .testr.conf

# Test command manually
python -m subunit.run discover --list

# Check repository
testr stats
```

### Subunit Format Issues

```bash
# Verify subunit output
python -m subunit.run discover | python -m subunit.stats

# Check for binary corruption
file .testrepository/0
```

### Parallel Execution Issues

```bash
# Run in serial to isolate the issue
testr run

# Run in isolated mode to check for test interactions
testr run --isolated

# Check worker-specific failures
testr last | grep worker-
```

### Test Isolation Failures

When a test passes in isolation but fails when run with other tests:

```bash
# Step 1: Verify the test fails with others
testr run

# Step 2: Verify it passes in isolation
testr run --isolated test_module.test_flaky

# Step 3: Find the minimal set of tests causing the issue
testr analyze-isolation test_module.test_flaky

# The command will output which tests cause the failure
# Example output:
# Found minimal set of 2 tests causing isolation failure:
#   - test_module.test_setup_state
#   - test_module.test_cleanup_missing
```
