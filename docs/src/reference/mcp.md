# MCP Servers

Boitata connects to [MCP](https://modelcontextprotocol.io) servers using the
official [`rmcp`](https://crates.io/crates/rmcp) client.

Each server's tools are discovered at startup and exposed to the agent,
namespaced as `<server>_<tool>`, and called through the same agent loop as
built-in tools. So MCP tool calls show up in the [audit log](./audit-log.md)
just like built-in ones. A server that fails to start is logged and skipped, so
one broken server can't abort a run.

## Transports

The transport is inferred from which field you set on a `[[mcp_servers]]` block:

| Field | Transport |
|-------|-----------|
| `command` | stdio: the server is spawned as a subprocess |
| `url` | Streamable HTTP: connect to a remote server |

Set exactly one of the two per server.

## Configuring a server

### stdio (subprocess)

```toml
[[mcp_servers]]
name    = "filesystem"
command = "npx"
args    = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[[mcp_servers]]
name    = "git"
command = "uvx"
args    = ["mcp-server-git"]
env     = { GIT_AUTHOR_NAME = "boitata" }
```

### remote (Streamable HTTP)

```toml
[[mcp_servers]]
name       = "remote"
url        = "https://mcp.example.com/mcp"
auth_token = "your-token"          # sent as `Authorization: Bearer <token>`
headers    = { X-Workspace = "acme" }   # optional extra headers
```

On startup you'll see a line like `MCP server \`filesystem\` connected: 12 tool(s)`.
Credentials (`auth_token`) live in the git-ignored `boitata.toml` and are never
logged; `McpServerConfig` redacts them in its `Debug` output.

## Resources

When a server advertises the MCP resources capability, Boitata also registers two
tools per server so the agent can gather context on demand:

| Tool | Purpose |
|------|---------|
| `<server>_list_resources` | Returns the available resource URIs, names, and descriptions |
| `<server>_read_resource` | Reads one resource by URI |

These count toward the tool total reported at startup and, like all tool calls,
appear in the audit log.

See the [configuration reference](./configuration.md#mcp-servers) for the full
field list.
