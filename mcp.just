# mcp.just — MCP server recipes

[group('mcp')]
test:
    cd mcp && npm test

# Package MCP servers into a tarball
[group('mcp')]
tarball dir="/tmp/platform-e2e/main":
    @mkdir -p {{dir}}
    tar -czf {{dir}}/mcp-servers.tar.gz -C mcp \
      servers lib package.json package-lock.json node_modules
    @echo "MCP tarball: {{dir}}/mcp-servers.tar.gz"
