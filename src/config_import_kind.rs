use std::fmt;

use anyhow::{Result, bail};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ImportKind {
    Claude,
    ClaudeDesktop,
    Codex,
    Mcporter,
    Project,
    Native,
    Cursor,
    Windsurf,
    OpenCode,
    VsCode,
}

impl ImportKind {
    pub(super) fn parse(source: &str) -> Result<Self> {
        Ok(match source {
            "claude" | "claude-code" => Self::Claude,
            "claude-desktop" => Self::ClaudeDesktop,
            "codex" => Self::Codex,
            "mcporter" => Self::Mcporter,
            "project" => Self::Project,
            "mcp-pool" => Self::Native,
            "cursor" => Self::Cursor,
            "windsurf" => Self::Windsurf,
            "opencode" => Self::OpenCode,
            "vscode" | "vs-code" => Self::VsCode,
            _ => bail!(
                "Import source must be claude, claude-desktop, codex, mcporter, project, mcp-pool, cursor, windsurf, opencode, or vscode"
            ),
        })
    }

    pub(super) fn is_editor(self) -> bool {
        matches!(
            self,
            Self::Cursor | Self::Windsurf | Self::OpenCode | Self::VsCode
        )
    }

    pub(super) fn is_toml(self) -> bool {
        matches!(self, Self::Codex | Self::Native)
    }

    pub(super) fn containers(self) -> &'static [&'static str] {
        match self {
            Self::Codex => &["mcp_servers"],
            Self::Native => &["server"],
            Self::OpenCode => &["mcp"],
            Self::Cursor | Self::Windsurf | Self::VsCode => &["mcpServers", "servers"],
            _ => &["mcpServers"],
        }
    }
}

impl fmt::Display for ImportKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Claude => "claude",
            Self::ClaudeDesktop => "claude-desktop",
            Self::Codex => "codex",
            Self::Mcporter => "mcporter",
            Self::Project => "project",
            Self::Native => "mcp-pool",
            Self::Cursor => "cursor",
            Self::Windsurf => "windsurf",
            Self::OpenCode => "opencode",
            Self::VsCode => "vscode",
        })
    }
}
