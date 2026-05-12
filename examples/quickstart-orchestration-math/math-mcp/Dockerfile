FROM node:20-alpine

WORKDIR /app

# Install mcp-proxy globally
RUN npm install -g mcp-proxy

# Clone and build math-mcp
RUN apk add --no-cache git curl \
    && git clone https://github.com/EthanHenrickson/math-mcp.git \
    && cd math-mcp \
    && npm install \
    && npm run build:stdio \
    && apk del git

EXPOSE 8081

# Protocol-level healthcheck: verify MCP JSON-RPC responds to initialize
HEALTHCHECK --interval=5s --timeout=5s --retries=10 --start-period=15s \
    CMD curl -sf http://localhost:8081/mcp -X POST \
        -H "Content-Type: application/json" \
        -H "Accept: application/json, text/event-stream" \
        -d '{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"healthcheck","version":"1.0"}},"id":1}' \
        > /dev/null || exit 1

CMD ["mcp-proxy", "--port", "8081", "--", "node", "/app/math-mcp/build/index.js"]
