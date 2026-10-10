use anyhow::{Result, bail};

pub(super) fn split_expression(contents: &str) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut delimiters = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    for (offset, character) in contents.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote.is_some() {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '[' => delimiters.push(']'),
            '{' => delimiters.push('}'),
            '(' => delimiters.push(')'),
            ']' | '}' | ')' => {
                if delimiters.pop() != Some(character) {
                    bail!("Unbalanced call expression");
                }
            }
            ',' if delimiters.is_empty() => {
                if let Some(part) = contents.get(start..offset) {
                    parts.push(part.trim().to_owned());
                }
                start = offset + character.len_utf8();
            }
            _ => {}
        }
    }
    if quote.is_some() || !delimiters.is_empty() {
        bail!("Unclosed quote or collection in call expression");
    }
    if let Some(part) = contents.get(start..).filter(|part| !part.trim().is_empty()) {
        parts.push(part.trim().to_owned());
    }
    Ok(parts)
}
