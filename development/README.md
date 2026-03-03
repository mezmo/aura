# Using Aura within LibreChat and OpenWebUI

You can use Aura within LibreChat and OpenWebUI because Aura supports an OpenAI-compatible HTTP API.


## Pre-requisites

- You have Aura web-server running on docker's host on port 8080.  See [running the web server](../README.md#web-api-server) for help on this.

## Starting LibreChat and OpenWebUI

- All of these commands must be run from the `development` directory within the aura project root
- Run `docker compose up` to start an instance of each front-end.
  - Add `-d` to the end of the command to run them in the background!
    - `docker compose up -d`
- Log in to LibreChat by navigating to http://localhost:3001/
  - There is one additional step for creating the initial user before you can log in
    - `docker-compose exec librechat npm run create-user`
    - Enter in the details for your user
- Log in to OpenWebUI by navigating to http://localhost:3000/
  - The web UI will walk you through creating an admin user

## Using MCP services that require authorization tokens

If you have MCP's that require authorization tokens you will need to configure LibreChat and
OpenWebUI to send those tokens as headers and then map those headers to the MCP's using
Aura MCP header forwarding configuration.

### Configuring Aura MCP's to forward specific headers from requests

1. Add headers_from_request configuration to your MCP's listed in Aura's config.toml

- This example adds the GitHub MCP server

```toml
[mcp.servers.github]
transport = "http_streamable"
url = "https://api.githubcopilot.com/mcp"
description = "GitHub MCP for searching and retrieving code and github information"

[mcp.servers.github.headers_from_request]
"Authorization" = "x-auth-github-token"
```

If you're using Mezmo Log Analysis MCP and haven't configured Aura to use a static token for
the Mezmo Log Analysis MCP, you will want to forward the "Authorization" header being
sent from the chat clients.

```toml
# Mezmo Log Analysis Server - direct streamable HTTP
[mcp.servers.mezmo_log_analysis]
transport = "http_streamable"
url = "https://mcp.use.dev.mezmo.it/mcp"
description = "Mezmo MCP server providing log analysis, export, and monitoring tools"

# This will take the Auth token from chat clients and forward it to the MCP
[mcp.servers.mezmo_log_analysis.headers_from_request]
"Authorization" = "Authorization"
```

### LibreChat header configuration

Configure LibreChat to send "x-auth-github-token" with chat requests by adding "headers" to the
librechat.yaml file.

```yaml
# LibreChat Configuration for Aura Server
# See: https://docs.librechat.ai/install/configuration/custom_config.html

version: 1.1.5

endpoints:
  custom:
    - name: "Aura"
      apiKey: "${MEZMO_API_KEY}" # Use environment variable
      baseURL: "http://host.docker.internal:8080/v1"
      # Add whatever headers you want and they'll be sent with chat requests!
      headers:
        "x-auth-github-token": "${GITHUB_TOKEN}" # Do not hard-code token here, use an ENV variable
    # ADDITIONAL CONFIGURATION BELOW
```

### OpenWebUI Header Configuration

1. Click on your avatar icon in the bottom left of the OpenWebUI page.
2. Click on "Admin Panel".
3. Click on "Settings" located at the top of the page, to the right.
4. Click on "Connections" in the left-hand navigation section.
5. Locate OpenAPI API section, where you've added Aura as an OpenAPI LLM
6. Click on the configuration cog next to the Aura OpenAPI API connection entry
7. Add a JSON blob to the "Headers" section of the connection configuration

```json
{ "x-auth-github-token": "YOUR GITHUB TOKEN" }
```
