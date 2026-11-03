pub(crate) fn command_help(command: &str, arguments: &[String]) {
    let subcommand = arguments
        .iter()
        .find(|argument| !argument.starts_with('-') && argument.as_str() != "help")
        .map(String::as_str);
    let usage = match (command, subcommand) {
        ("config", Some("list")) => {
            "config list [FILTER] [--json] [--source local]\n\nShow merged server definitions without connecting. FILTER matches server names.\n  --json          Emit definitions as JSON; environment/header values are redacted.\nCommand arguments are resolved and unredacted, including environment references.\n  --source local  Imports are local copies, not a separate runtime source.\n\nExamples:\n  mcp-pool config list\n  mcp-pool config list docs --json"
        }
        ("config", Some("get")) => {
            "config get NAME [--json]\n\nInspect one server's transport, source, and settings. No connection.\nEnvironment/header values are redacted. Command arguments are resolved and\nunredacted, including values expanded from environment references.\n  --json  Emit the definition as JSON with environment/header values redacted.\n\nExamples:\n  mcp-pool config get docs\n  mcp-pool config get docs --json"
        }
        ("config", Some("add")) => {
            "config add NAME [URL] [OPTIONS]\n\nSave an HTTP/SSE or stdio server definition.\n  --url URL --transport http|sse|stdio\n  --command COMMAND (alias --stdio) --arg ARG (repeatable)\n  --env KEY=value --header KEY=value (repeatable)\n  --description TEXT --protocol-version auto|legacy|2026-07-28\n  --auth oauth --token-cache-dir PATH --client-name NAME\n  --oauth-client-id ID --oauth-client-secret-env NAME\n  --oauth-token-endpoint-auth-method METHOD --oauth-redirect-url URL\n  --oauth-requested-scope SCOPES\n  --copy-from SOURCE:NAME --from PATH --replace\n  --scope home|project --persist PATH --dry-run\n  --  Forward remaining tokens as stdio arguments.\n\nExamples:\n  mcp-pool config add docs https://example.com/mcp\n  mcp-pool config add local --command python3 --arg server.py\n  mcp-pool config add local --command python3 --arg server.py --dry-run"
        }
        ("config", Some("remove")) => {
            "config remove NAME\n\nRemove a local definition. Retires matching pooled views before writing.\nDoes not modify the external source of an imported definition.\n\nExample:\n  mcp-pool config remove docs"
        }
        ("config", Some("login")) => {
            "config login SERVER [--reset] [--no-browser|--browser none] [--json]\n\nAuthorize a server, or reuse valid cached credentials. Alias for auth.\n  --reset                 Replace cached credentials through authorization.\n  --no-browser            Do not open the browser automatically.\n  --oauth-timeout MILLISECONDS  Bound explicit authorization.\n\nExample:\n  mcp-pool config login docs --no-browser"
        }
        ("config", Some("logout")) => {
            "config logout NAME\n\nClear credentials in the active scope. Retires matching pooled views.\nClearing a project override restores home credential fallback.\n\nExample:\n  mcp-pool config logout docs"
        }
        ("config", Some("doctor")) => {
            "config doctor [--json]\n\nValidate merged configuration and inspect cached credential health.\nDoes not connect, refresh tokens, or start browser consent.\n  --json  Emit configuration, warnings, and credential status as JSON.\n\nExample:\n  mcp-pool config doctor"
        }
        ("config", Some("import")) | ("import", _) => {
            "import SOURCE [NAME...] [--from PATH] [--scope home|project]\n  --dry-run --replace --json --include-auth\n\nCopy external definitions. Alias: config import.\nSources: claude, claude-code, claude-desktop, codex, mcporter, project,\n  mcp-pool, cursor, windsurf, opencode, vscode, vs-code.\n  --dry-run       Preview without writing or retiring pools.\n  --replace       Permit conflicting replacements.\n  --include-auth  Copy cached HTTP/SSE OAuth credentials (mcporter only).\nSources are never executed or modified. No login or token refresh.\n\nExamples:\n  mcp-pool import mcporter --dry-run\n  mcp-pool config import codex --scope project"
        }
        ("daemon", Some("status")) => {
            "daemon status [--json]\n\nShow broker and active server status without starting a daemon.\n  --json  Emit machine-readable status.\n\nExample:\n  mcp-pool daemon status --json"
        }
        ("daemon", Some("stop")) => {
            "daemon stop [--json]\n\nStop the broker and await retirement of its owned upstreams.\nAn absent broker stays stopped. Does not auto-launch.\n\nExample:\n  mcp-pool daemon stop"
        }
        ("daemon", Some("start" | "restart")) => {
            let action = subcommand.unwrap_or("start");
            println!("Usage: mcp-pool daemon {action} [--json] [OPTIONS]");
            println!(
                "\nStart the broker without warming configured servers.\nRestart first drains the existing broker; start preserves a live broker.\n  --foreground       Run in the current process.\n  --log              Enable bounded operation logging.\n  --log-file PATH    Select the log file.\n  --log-servers CSV  Log exact logical server names (implies --log).\nLogging options apply on launch. Restart applies changed options.\n\nExample:\n  mcp-pool daemon {action} --log"
            );
            global_help();
            return;
        }
        ("daemon", Some("migrate")) => {
            "daemon migrate [--json] [--stop-legacy --confirmed-drained]\n\nInspect legacy owners without retiring them by default.\nRetirement requires both flags, a same-user handshake, and verified ownership.\nFailed retirement blocks cutover. Never stops arbitrary PIDs.\n\nExample:\n  mcp-pool daemon migrate"
        }
        ("vault", Some("set")) => {
            "vault set SERVER (--tokens-file PATH|--stdin)\n\nSeed OAuth credentials without browser consent. Use exactly one input source.\nPayload: {\"tokens\":{\"access_token\":\"...\",\"token_type\":\"Bearer\"},\n  \"clientInfo\":{\"client_id\":\"...\"}}\nCredential values are never printed. Retires matching pooled views.\n\nExample:\n  mcp-pool vault set docs --tokens-file tokens.json"
        }
        ("vault", Some("clear")) => {
            "vault clear SERVER\n\nRemove the active OAuth vault entry. Retires matching pooled views.\n\nExample:\n  mcp-pool vault clear docs"
        }
        ("call", _) => {
            "call SERVER.TOOL [key=value|key:value ...] [OPTIONS]\n\nInvoke a tool. Also accepts SERVER TOOL or --server NAME --tool NAME.\n\nArguments:\n  --args, --params JSON|-  JSON object, or - to read stdin.\n  key=@FILE               Read UTF-8 text; @@TEXT sends a literal @.\n  'SERVER.TOOL(key: \"value\")'  Function-call syntax.\n  Positional values follow schema property order. -- ends option parsing.\n\nRuntime flags:\n  --output auto|text|markdown|json|raw  Select result format.\n  --timeout MILLISECONDS --no-oauth\n  --raw-strings --no-coerce --save-images DIRECTORY --tail-log\n  raw emits the JSON result envelope; tail-log prints a bounded log tail.\n\nExamples:\n  mcp-pool call docs.search query=\"MCP\" limit:5\n  mcp-pool call docs.search --args '{\"query\":\"MCP\"}' --output json"
        }
        ("list" | "describe" | "list-tools", _) => {
            "list [SERVER[.TOOL]] [OPTIONS]\n\nDiscover tools. Aliases: describe, list-tools. Can start stdio servers.\nUse config list to inspect definitions without connecting.\n  --schema          Show complete input schemas alongside tool docs.\n  --brief, --signatures  Show required-only signatures.\n  --all-parameters  Include hidden optional parameters.\n  --json --status --sources --no-color\n  --timeout MILLISECONDS --no-oauth\n  --exit-code       Fail when any server fails discovery.\n  --quiet           Suppress all output and enable failure exit codes.\nBrief cannot combine with schema, JSON, or status.\n\nExamples:\n  mcp-pool list\n  mcp-pool list docs --schema\n  mcp-pool list docs.search --json"
        }
        ("resource" | "resources", _) => {
            "resource SERVER [URI] [--output auto|text|markdown|json|raw]\n  --json --no-oauth --timeout MILLISECONDS\n\nWithout URI, list resources. With URI, read its content. Alias: resources.\n\nExamples:\n  mcp-pool resource docs\n  mcp-pool resource docs docs://guide --output text"
        }
        ("auth", _) => {
            "auth SERVER [--reset] [--no-browser|--browser none] [--json]\n\nAuthorize a server, or reuse valid cached credentials.\n  --oauth-timeout MILLISECONDS  Bound explicit authorization, not cached refresh.\n  --reset         Replace credentials, not a read-only validation step.\n  --no-browser    Do not open the browser automatically.\n\nExample:\n  mcp-pool auth docs --no-browser"
        }
        ("vault", _) => {
            "vault set SERVER (--tokens-file PATH|--stdin)\n  mcp-pool vault clear SERVER\n\nSeed or clear scoped OAuth credentials without printing credential values.\nRun mcp-pool vault set --help for the JSON payload format."
        }
        ("config", _) => {
            "config list|get|add|remove|import|login|logout|doctor|help\n\nInspect or edit merged configuration without connecting to MCP servers.\n  list [FILTER]    Show servers and transport details.\n  get NAME         Inspect one definition.\n  add NAME [URL]   Save a server definition.\n  remove NAME      Remove a local definition.\n  import SOURCE    Copy external definitions.\n  login NAME       Authorize a server explicitly.\n  logout NAME      Clear active credentials.\n  doctor           Validate config and inspect credential health.\n\nRun mcp-pool config COMMAND --help for flags and examples."
        }
        ("daemon", _) => {
            "daemon start|status|stop|restart|migrate|help [--json]\n\nManage the single mcp-pool broker.\nStart preserves a live broker. Restart applies changed logging options.\nStatus and stop never auto-launch. Start does not warm configured servers.\n\nRun mcp-pool daemon COMMAND --help for flags and examples."
        }
        ("serve", _) => {
            "serve [--stdio | --http PORT [--host HOST]] [--servers NAME,...]\n\nExpose tools from keep-alive entries over stdio (default) or HTTP.\n  --concurrency 1..64  Bound operations per server (default 1).\nAll connections share one upstream and its state; concurrent calls may finish out of order.\nHTTP defaults to loopback with no downstream authentication.\nThis tool-only bridge does not expose resources, prompts, or elicitation.\n\nExamples:\n  mcp-pool serve --stdio --servers docs\n  mcp-pool serve --http 8080 --servers docs"
        }
        _ => "list | call | auth | vault | resource | config | import | daemon | serve",
    };
    let prefix = if command == "config" && subcommand == Some("import") {
        "config "
    } else {
        ""
    };
    println!("Usage: mcp-pool {prefix}{usage}");
    if matches!(
        command,
        "call" | "list" | "describe" | "list-tools" | "auth"
    ) {
        println!(
            "\nAd-hoc servers:\n  --stdio COMMAND --stdio-arg ARG | --http-url URL [--allow-http]\n  --name NAME --cwd PATH --env KEY=value --header KEY=value\n  --description TEXT --persist PATH"
        );
    }
    global_help();
}

fn global_help() {
    println!(
        "\nGlobal: --config PATH --root PATH --json --plain --debug --log-level LEVEL\n  --oauth-timeout MILLISECONDS (auth/config login only)\n  Config: ~/.mcp/config.json plus PROJECT/.mcp/config.json (project wins).\n  --config or MCP_POOL_CONFIG selects one file. Credentials follow the same scopes."
    );
}
