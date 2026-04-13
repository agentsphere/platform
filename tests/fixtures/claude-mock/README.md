# claude-mock

Drop-in mock replacement for the `claude` CLI binary. Used by platform and agent-runner integration tests.

## Modes

| Mode | Command | What it does |
|------|---------|-------------|
| `setup-token` | `claude setup-token` | Emits OAuth banner + URL, waits for code on stdin, emits token |
| `--print` | `claude --print -p "prompt"` | Emits NDJSON stream (system.init → assistant → result) |

## Usage in tests

Set `CLAUDE_CLI_PATH` to point to this mock:

```bash
export CLAUDE_CLI_PATH=/path/to/cli/claude-mock/claude
```

The platform's `test_state()` / `test_state_with_cli()` helpers set this automatically.

## Environment variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `MOCK_CLI_RESPONSE_FILE` | (none) | JSON array of response objects for multi-turn `--print` |
| `MOCK_CLI_STATE_DIR` | `/tmp/mock-cli-state` | Tracks invocation count for multi-response |
| `MOCK_CLI_OAUTH_URL` | `https://claude.ai/oauth/authorize?...` | Override OAuth URL |
| `MOCK_CLI_OAUTH_TOKEN` | `sk-ant-oat01-MockTestToken_...` | Override OAuth token |
| `MOCK_CLI_AUTH_FAIL` | `false` | Set `true` to simulate auth failure |
| `MOCK_CLI_SLOW` | (none) | Sleep N seconds before responding |

## Output format

The `setup-token` output was captured from real `claude setup-token` v2.1.87 via the platform's PTY wrapper (`script`). The `--print` NDJSON format matches the Claude CLI streaming protocol.
