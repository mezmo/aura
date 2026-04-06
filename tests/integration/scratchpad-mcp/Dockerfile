FROM python:3.12-alpine

WORKDIR /app

# Install curl for healthcheck
RUN apk add --no-cache curl

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY server.py .

EXPOSE 8083

# Protocol-level healthcheck: verify MCP JSON-RPC responds to initialize
HEALTHCHECK --interval=5s --timeout=5s --retries=10 --start-period=15s \
    CMD curl -sf http://localhost:8083/mcp -X POST \
        -H "Content-Type: application/json" \
        -H "Accept: application/json, text/event-stream" \
        -d '{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"healthcheck","version":"1.0"}},"id":1}' \
        > /dev/null || exit 1

CMD ["python", "server.py"]
