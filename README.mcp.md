# Inquest MCP Server

Inquest includes an [MCP (Model Context Protocol)](https://modelcontextprotocol.io/) server that lets AI coding assistants interact with your test repository using structured JSON instead of parsing CLI output.

## Building

The MCP server is behind an optional feature flag:

```sh
cargo build --release --features mcp
```

## Using with Claude Code

### Quick setup

The fastest way to add the inquest MCP server to a project:

```sh
claude mcp add --transport stdio --scope project inquest -- inq mcp
```

This creates a `.mcp.json` file in your project root that Claude Code reads automatically.

If `inq` is not on your `PATH`, use the full path:

```sh
claude mcp add --transport stdio --scope project inquest -- /path/to/inq mcp
```

To make it available across all your projects (user-scoped):

```sh
claude mcp add --transport stdio --scope user inquest -- inq mcp
```

### Manual configuration

Alternatively, create or edit `.mcp.json` in your project root:

```json
{
  "mcpServers": {
    "inquest": {
      "type": "stdio",
      "command": "inq",
      "args": ["mcp"]
    }
  }
}
```

To point the server at a specific project directory (instead of the working directory):

```json
{
  "mcpServers": {
    "inquest": {
      "type": "stdio",
      "command": "inq",
      "args": ["-C", "/path/to/project", "mcp"]
    }
  }
}
```

### Verifying the setup

After adding the server, start Claude Code and run `/mcp` to confirm the `inquest` server is connected and its tools are listed.

### What Claude Code can do with this

Once configured, Claude Code can directly:

- Check which tests are failing after a code change (`inq_failing`)
- Run the test suite and get structured pass/fail results (`inq_run`)
- Inspect tracebacks for failing tests to diagnose issues (`inq_log`)
- Identify slow tests that might benefit from optimization (`inq_slowest`)
- Get an overview of the test repository (`inq_stats`)

This replaces the need for Claude Code to shell out to `inq` and parse human-formatted terminal output with progress bars and color codes.

## Available Tools

| Tool | Description | Parameters |
|------|-------------|------------|
| `inq_stats` | Repository statistics (run count, latest run summary) | -- |
| `inq_failing` | Currently failing tests | -- |
| `inq_last` | Results from a test run | `run_id`: optional, supports negative indices like `-1` |
| `inq_slowest` | Slowest tests with timing info | `count`: optional (default 10) |
| `inq_log` | Test details and tracebacks | `run_id`: optional; `test_patterns`: optional glob patterns |
| `inq_run` | Execute tests and return results | `failing_only`, `concurrency`, `test_filters`: all optional |
| `inq_list_tests` | List available tests | -- |

All tools return JSON responses.

## Example Responses

### `inq_stats`

```json
{
  "run_count": 5,
  "latest_run": {
    "id": "4",
    "total_tests": 211,
    "passed": 209,
    "failed": 2,
    "duration_secs": 1.234
  }
}
```

### `inq_failing`

```json
{
  "count": 2,
  "tests": [
    "tests.test_auth.TestLogin.test_invalid_password",
    "tests.test_api.TestEndpoints.test_timeout"
  ]
}
```

### `inq_last`

```json
{
  "id": "4",
  "timestamp": "2026-03-18T10:30:00+00:00",
  "total_tests": 211,
  "passed": 209,
  "failed": 2,
  "duration_secs": 1.234,
  "failing_tests": [
    "tests.test_auth.TestLogin.test_invalid_password"
  ],
  "interruption": null
}
```

### `inq_log`

```json
{
  "run_id": "4",
  "count": 1,
  "results": [
    {
      "test_id": "tests.test_auth.TestLogin.test_invalid_password",
      "status": "failure",
      "duration_secs": 0.045,
      "message": "AssertionError",
      "details": "Traceback (most recent call last):\n  ..."
    }
  ]
}
```

## Requirements

The MCP server requires the same project setup as the `inq` CLI:

- A test repository (`.testrepository/` directory), created by `inq init` or automatically when a config file is present
- For `inq_run` and `inq_list_tests`: a configuration file (`inquest.toml` or `.testr.conf`) with a `test_command`
