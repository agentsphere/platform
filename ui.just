# ui.just — UI build & test recipes

[group('ui')]
build:
    cd ui && npm run build

[group('ui')]
types:
    cd ui && npx tsc --noEmit --skipLibCheck 2>&1 | grep -v "path.*IntrinsicAttributes" || true

# `just ui test` or `just ui test --grep "login"` (pass playwright args)
[group('ui')]
test *args="":
    @echo "Requires running server: just dev"
    @kubectl exec -n platform pod/valkey -- valkey-cli DEL rate:login:admin >/dev/null 2>&1 || true
    cd ui && npx playwright test {{args}}

# Start platform server, run Playwright tests, stop server
[group('ui')]
test-headless port="8090":
    #!/usr/bin/env bash
    set -uo pipefail
    echo "Starting platform server on port {{port}}..."
    PLATFORM_DEV=true PLATFORM_LISTEN="0.0.0.0:{{port}}" cargo run &
    SERVER_PID=$!
    for i in $(seq 1 30); do
      curl -sf "http://localhost:{{port}}/healthz" >/dev/null 2>&1 && break
      sleep 2
    done
    curl -sf "http://localhost:{{port}}/healthz" >/dev/null 2>&1 || { echo "Server failed to start"; kill "$SERVER_PID" 2>/dev/null; exit 1; }
    kubectl exec -n platform pod/valkey -- valkey-cli DEL rate:login:admin >/dev/null 2>&1 || true
    cd ui && PLATFORM_URL="http://localhost:{{port}}" npx playwright test; STATUS=$?
    kill "$SERVER_PID" 2>/dev/null || true
    exit "$STATUS"
