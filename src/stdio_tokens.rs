use anyhow::{Result, bail};

pub(crate) fn windows_absolute(value: &str) -> bool {
    let mut characters = value.chars();
    matches!(
        (characters.next(), characters.next(), characters.next()),
        (Some(drive), Some(':'), Some('\\' | '/')) if drive.is_ascii_alphabetic()
    ) || value.starts_with(r"\\")
}

pub(crate) fn command_tokens(command: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    let mut characters = command.trim().chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\'' | '"' if quote == Some(character) => quote = None,
            '\'' | '"' if quote.is_none() => {
                quote = Some(character);
                started = true;
            }
            '\\' if !windows_absolute(&current) => {
                match characters.peek().copied() {
                    Some(next) if matches!(next, '\'' | '"') || next.is_whitespace() => {
                        current.push(next);
                        characters.next();
                    }
                    _ => current.push(character),
                }
                started = true;
            }
            character if character.is_whitespace() && quote.is_none() => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            character => {
                current.push(character);
                started = true;
            }
        }
    }
    if quote.is_some() {
        bail!("unclosed quote in command string");
    }
    if started {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_is_tokenized_before_extra_arguments() -> Result<()> {
        assert_eq!(
            command_tokens("node \"server script.js\"")?,
            vec!["node", "server script.js"]
        );
        assert!(command_tokens("node 'broken").is_err());
        assert_eq!(
            command_tokens(r#""C:\Program Files\node.exe" server.js"#)?,
            vec![r"C:\Program Files\node.exe", "server.js"]
        );
        assert_eq!(
            command_tokens(r#"node '' "" escaped\ argument"#)?,
            ["node", "", "", "escaped argument"]
        );
        assert_eq!(
            command_tokens(r"\\host\share\server.exe")?,
            [r"\\host\share\server.exe"]
        );
        assert_eq!(
            command_tokens(r#""C:/Program Files/node.exe" ''"#)?,
            ["C:/Program Files/node.exe", ""]
        );
        assert!(command_tokens("")?.is_empty());
        Ok(())
    }
}
