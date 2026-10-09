use anyhow::{Result, bail};

pub(super) fn command_tokens(command: &str) -> Result<Vec<String>> {
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
            '\\' if !current.get(1..3).is_some_and(|part| part == ":\\") => {
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
        bail!("Unclosed quote in --stdio command");
    }
    if started {
        tokens.push(current);
    }
    if tokens.first().is_none_or(String::is_empty) {
        bail!("--stdio requires a nonempty command");
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
        Ok(())
    }
}
