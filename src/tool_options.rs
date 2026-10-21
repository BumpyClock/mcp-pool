#[derive(Clone, Copy)]
pub(crate) enum CallOption {
    Server,
    Tool,
    Timeout,
    Output,
    Raw,
    RawStrings,
    NoCoerce,
    NoOauth,
    Yes,
    TailLog,
    SaveImages,
    Json,
}

impl CallOption {
    pub(crate) fn parse(flag: &str) -> Option<Self> {
        Some(match flag {
            "--server" | "--mcp" => Self::Server,
            "--tool" => Self::Tool,
            "--timeout" => Self::Timeout,
            "--output" => Self::Output,
            "--raw" => Self::Raw,
            "--raw-strings" => Self::RawStrings,
            "--no-coerce" => Self::NoCoerce,
            "--no-oauth" => Self::NoOauth,
            "--yes" => Self::Yes,
            "--tail-log" => Self::TailLog,
            "--save-images" => Self::SaveImages,
            "--args" | "--params" | "--json" => Self::Json,
            _ => return None,
        })
    }

    fn takes_value(self) -> bool {
        matches!(
            self,
            Self::Server
                | Self::Tool
                | Self::Timeout
                | Self::Output
                | Self::SaveImages
                | Self::Json
        )
    }
}

#[derive(Clone, Copy)]
pub(crate) enum AdHocOption {
    HttpUrl,
    Sse,
    Stdio,
    StdioArg,
    Cwd,
    Name,
    AllowHttp,
    Env,
    Header,
    Persist,
    Description,
}

impl AdHocOption {
    pub(crate) fn parse(flag: &str) -> Option<Self> {
        Some(match flag {
            "--http-url" => Self::HttpUrl,
            "--sse" => Self::Sse,
            "--stdio" => Self::Stdio,
            "--stdio-arg" => Self::StdioArg,
            "--cwd" => Self::Cwd,
            "--name" => Self::Name,
            "--allow-http" => Self::AllowHttp,
            "--env" => Self::Env,
            "--header" => Self::Header,
            "--persist" => Self::Persist,
            "--description" => Self::Description,
            _ => return None,
        })
    }
}

pub(crate) fn takes_value(flag: &str) -> bool {
    CallOption::parse(flag).is_some_and(CallOption::takes_value)
        || AdHocOption::parse(flag).is_some_and(|option| !matches!(option, AdHocOption::AllowHttp))
}
