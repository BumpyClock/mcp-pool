pub(crate) fn command_help(command: &str) {
    let usage = match command {
        "call" => {
            "call SERVER.TOOL [key=value|key:value ...] [--args JSON|-] [--server NAME --tool NAME]\n  --output auto|text|markdown|json|raw --timeout MILLISECONDS --no-oauth\n  --raw-strings/--no-coerce --save-images DIRECTORY --tail-log\n  Named @FILE values read UTF-8 text; @@TEXT escapes a literal @."
        }
        "list" | "describe" | "list-tools" => {
            "list [SERVER[.TOOL]] [--schema|--signatures|--brief] [--all-parameters]\n  --status --exit-code --quiet --json --no-color --timeout MILLISECONDS --no-oauth"
        }
        "resource" | "resources" => {
            "resource SERVER [URI] [--output auto|text|markdown|json|raw] [--json] [--no-oauth]"
        }
        "auth" => {
            "auth SERVER [--reset] [--no-browser|--browser none] [--json]\n  --oauth-timeout MILLISECONDS bounds explicit authorization, not cached call/list refresh."
        }
        "vault" => "vault set SERVER (--tokens-file PATH|--stdin)\n  vault clear SERVER",
        "config" => {
            "config list|get|add|remove|import|login|logout|doctor|help\n  config add NAME [URL] [--command COMMAND --arg ARG] [--copy-from SOURCE:NAME]\n    [--from PATH] [--replace] [--protocol-version auto|legacy|2026-07-28]\n    [--scope home|project] [--dry-run] [--persist PATH]\n  config import claude|codex|mcporter|project|mcp-pool|cursor|windsurf|opencode|vscode|vs-code [NAME...]\n    [--from PATH] [--scope home|project] [--dry-run] [--replace] [--json]"
        }
        "import" => {
            "import claude|codex|mcporter|project|mcp-pool|cursor|windsurf|opencode|vscode|vs-code [NAME...]\n  --from PATH --scope home|project --dry-run --replace --json\n  Copies definitions only. Sources are never executed or modified; OAuth login remains explicit."
        }
        "daemon" => {
            "daemon start|status|stop|restart|migrate|help [--json]\n  start/restart: --foreground --log --log-file PATH --log-servers CSV\n  migrate: inspect legacy owners; --stop-legacy requires --confirmed-drained.\n  Uses the existing single mcp-pool broker. Start preserves a live broker; restart applies new logging options."
        }
        _ => "list | call | auth | vault | resource | config | import | daemon | serve",
    };
    println!("Usage: mcp-pool {usage}");
    if matches!(
        command,
        "call" | "list" | "describe" | "list-tools" | "auth"
    ) {
        println!(
            "Ad-hoc: --stdio COMMAND --stdio-arg ARG | --http-url URL [--allow-http]\n  --name NAME --cwd PATH --env KEY=value --header KEY=value --description TEXT --persist PATH"
        );
    }
    println!(
        "Global: --config PATH --root PATH --json --debug --log-level LEVEL\n  --oauth-timeout MILLISECONDS (auth/config login only)\n  Config: ~/.mcp/config.json plus PROJECT/.mcp/config.json (project wins).\n  --config or MCP_POOL_CONFIG selects one file. Credentials follow the same scopes."
    );
}
