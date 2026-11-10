# Authentication and credentials

[README](../README.md) | [Configuration](configuration.md) | [Imports](imports.md)

Authentication is explicit. `list`, `call`, resources, and `proxy` never open
consent; they use existing credentials and can normally refresh an expired token.
Missing credentials require `mcp-cli auth SERVER`. Provider availability, account
permission, and consent are not guaranteed.

Discovery, calls, and resources accept `--no-oauth` to use valid cached OAuth
tokens without refresh; it does not remove explicitly configured headers.

```sh
mcp-cli auth docs
mcp-cli auth docs --no-browser
mcp-cli vault set docs --tokens-file tokens.json
mcp-cli vault clear docs
```

Replace `docs` and `tokens.json` with your server and protected token file. Pass
token material through a file or stdin, not command-line arguments. Never commit
credential files. Do not use `auth --reset`, `vault set`, `vault clear`, or config
login/logout against credentials that must remain unchanged. Explicit auth and
mutations first retire matching pooled views and can disconnect clients; reconnect
afterward.

## Credential scope and isolation

Credentials belong to the config defining the selected server:

| Config source | Credential store |
| --- | --- |
| Home definition | `~/.mcp/credentials.json`, across projects |
| Project definition | `PROJECT/.mcp/credentials.json` |
| `--config PATH` | Credentials beside that config |

A same-named project definition replaces the home definition and never falls back
to home credentials when missing, expired, or cleared. Removing it restores the
home definition and credentials. Login, refresh, inspection, vault, and logout use
the same owning store. `--root` does not relocate inherited home credentials.

Home definitions share pools across projects; project and explicit definitions
have separate identities. Old project credentials are not moved automatically.
Explicit `tokenCacheDir` can intentionally share a cache; legacy mcporter paths
are not auto-discovered.

## Read-only credentials

Set `MCP_CLI_CREDENTIALS_READ_ONLY=1` in the calling process to use valid cached
credentials without refresh or credential writes. Either this flag or
`MCP_POOL_CREDENTIALS_READ_ONLY=1` enforces the restriction; setting the other to
`0` cannot weaken it. Expired credentials cannot refresh, and auth and vault
mutations fail. The policy travels with the request even to an already-running
daemon. It does not prevent explicit config writes.

For example, PowerShell can select the home config while isolating daemon state
and enforcing read-only credentials:

```powershell
$env:MCP_CLI_HOME = Join-Path $PWD "target\mcp-validation"
$env:MCP_CLI_CONFIG = Join-Path $HOME ".mcp\config.json"
$env:MCP_CLI_CREDENTIALS_READ_ONLY = "1"
mcp-cli list --json --exit-code
mcp-cli daemon stop
```

## Vault format and reconciliation

`vault set` accepts a token object or credential object containing `tokens`.
Tokens require a nonempty `access_token` and Bearer `token_type`. Use `--stdin`
instead of `--tokens-file` for JSON on stdin.

Expiration uses absolute Unix seconds in `expires_at` or `expiresAt`. `expires_in`
is converted at import time, so use it only for newly issued responses. Existing
entries with `expires_in` but no absolute expiration are treated as expired. A
payload with a refresh token must also include its legitimate bound `clientInfo`
registration; refresh needs issuer metadata.

Stores reconcile partial writes using committed generation ordering, not
expiration. Registration metadata is not combined across token generations.
Divergent legacy rotating refresh tokens without verified ordering fail closed.
Use `auth SERVER --reset` only when authorized to replace credentials; the pool
does not guess or replay an older refresh token.

## OAuth requirements

HTTP authorization uses OAuth metadata discovery, PKCE S256, and a loopback
callback. `--no-browser` prints the URL and still waits for the callback. The
issuer must advertise PKCE S256. Existing registered client information can be
reused.

Dynamic registration chooses an advertised client authentication method,
preferring `none`, then `client_secret_basic`, then `client_secret_post`; absent
a method list it uses `client_secret_basic`. `oauthTokenEndpointAuthMethod`
overrides selection. Issued client secrets are retained for code exchange and
refresh. Without dynamic registration, configure a legitimate `oauthClientId` or
supported HTTPS client metadata document. Unsupported signed client-authentication
methods fail explicitly.

Leading `--oauth-timeout MILLISECONDS` bounds explicit authorization for `auth`
and `config login`, including stdio helpers; other commands reject it. Cached
refresh retains its fixed OAuth deadline. Use operation `--timeout` for call or
discovery waits.
