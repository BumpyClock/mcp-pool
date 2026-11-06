use std::sync::OnceLock;

use anyhow::{Context, Result};
use clap::{Arg, CommandFactory};

fn globals() -> &'static [Arg] {
    static GLOBALS: OnceLock<Vec<Arg>> = OnceLock::new();
    GLOBALS.get_or_init(|| {
        let mut globals = crate::cli::Cli::command()
            .get_arguments()
            .filter(|argument| argument.is_global_set())
            .cloned()
            .collect::<Vec<_>>();
        // These options belong to MCP commands, which retain their custom payload grammar.
        globals.extend([
            Arg::new("log_level").long("log-level").num_args(1),
            Arg::new("oauth_timeout").long("oauth-timeout").num_args(1),
        ]);
        globals
    })
}

pub(crate) struct GlobalArgument<'a> {
    pub id: &'static str,
    inline: Option<&'a str>,
    pub width: usize,
}

impl<'a> GlobalArgument<'a> {
    pub(crate) fn value(&self, next: Option<&'a str>) -> Result<&'a str> {
        self.inline.or(next).with_context(|| {
            let name = globals()
                .iter()
                .find(|argument| argument.get_id().as_str() == self.id)
                .and_then(Arg::get_long)
                .unwrap_or(self.id);
            format!("Flag '--{name}' requires a value")
        })
    }
}

pub(crate) fn global(argument: &str) -> Option<GlobalArgument<'_>> {
    let name = argument.strip_prefix("--")?;
    let (name, inline) = name
        .split_once('=')
        .map_or((name, None), |(name, value)| (name, Some(value)));
    let option = globals()
        .iter()
        .find(|option| option.get_long() == Some(name))?;
    let takes_value = option.get_action().takes_values();
    if inline.is_some() && !takes_value {
        return None;
    }
    Some(GlobalArgument {
        id: option.get_id().as_str(),
        inline,
        width: if takes_value && inline.is_none() {
            2
        } else {
            1
        },
    })
}

pub(crate) fn command_index(arguments: &[String]) -> Option<usize> {
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        match global(argument) {
            Some(option) => index += option.width,
            None => return Some(index),
        }
    }
    None
}

pub(crate) struct Part<'a> {
    pub position: usize,
    pub argument: &'a str,
    pub global: Option<GlobalArgument<'a>>,
    pub next: Option<&'a str>,
}

pub(crate) struct CommandSyntax<'a> {
    pub command_position: Option<usize>,
    pub literal_position: Option<usize>,
    pub parts: Vec<Part<'a>>,
    pub help: bool,
    pub quiet: bool,
}

impl<'a> CommandSyntax<'a> {
    pub(crate) fn parse(arguments: &'a [String]) -> Self {
        let mut parsed = Self {
            command_position: None,
            literal_position: None,
            parts: Vec::new(),
            help: false,
            quiet: false,
        };
        let mut literal = false;
        let mut index = 0;
        while let Some(argument) = arguments.get(index) {
            let global = (!literal)
                .then(|| global(argument))
                .flatten()
                .filter(|option| {
                    // After the command, --json introduces a call payload or discovery output.
                    option.id != "json" || parsed.command_position.is_none()
                });
            if parsed.command_position.is_none() && global.is_none() {
                parsed.command_position = Some(index);
            } else if !literal && global.is_none() {
                parsed.help |= matches!(argument.as_str(), "--help" | "-h");
                parsed.quiet |= argument == "--quiet";
            }
            if !literal && argument == "--" {
                parsed.literal_position = Some(index);
                literal = true;
            }
            let width = global.as_ref().map_or(1, |option| option.width);
            let call_value = !literal
                && global.is_none()
                && parsed
                    .command_position
                    .and_then(|position| arguments.get(position))
                    .is_some_and(|command| command == "call")
                && crate::tool_arguments::options::takes_value(argument);
            parsed.parts.push(Part {
                position: index,
                argument,
                global,
                next: arguments.get(index + 1).map(String::as_str),
            });
            index += width;
            if call_value && let Some(value) = arguments.get(index) {
                parsed.parts.push(Part {
                    position: index,
                    argument: value,
                    global: None,
                    next: None,
                });
                index += 1;
            }
        }
        parsed
    }
}
