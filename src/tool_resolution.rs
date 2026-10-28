use anyhow::{Result, bail};
use serde_json::Value;

pub(crate) fn resolve_tool<F>(
    server_name: &str,
    tools: &[Value],
    requested: Option<&str>,
    mut allowed: F,
) -> Result<String>
where
    F: FnMut(&str) -> bool,
{
    let names = tool_names(tools);
    let mut visible = Vec::new();
    for name in &names {
        if allowed(name) {
            visible.push(name);
        }
    }

    let Some(requested) = requested else {
        return match visible.as_slice() {
            [name] => Ok((*name).clone()),
            [] => bail!("Server '{server_name}' exposes no available tools"),
            _ => bail!(
                "Missing tool: '{server_name}' exposes multiple tools; use call {server_name}.TOOL"
            ),
        };
    };

    if names.iter().any(|name| name == requested) {
        if visible.iter().any(|name| *name == requested) {
            return Ok(requested.to_owned());
        }
        bail!("Tool '{requested}' is excluded by the configuration for '{server_name}'");
    }

    let requested_key = normalize(requested);
    let aliases = visible
        .iter()
        .filter(|name| !requested_key.is_empty() && normalize(name) == requested_key)
        .map(|name| (*name).clone())
        .collect::<Vec<_>>();
    match aliases.as_slice() {
        [name] => return Ok(name.clone()),
        [] => {}
        _ => {
            bail!(
                "Tool '{requested}' is ambiguous on '{server_name}'; matches: {}",
                aliases.join(", ")
            );
        }
    }
    if names.iter().any(|name| normalize(name) == requested_key) {
        bail!("Tool '{requested}' is excluded by the configuration for '{server_name}'");
    }

    if let Some(suggestion) = suggestion(requested, &visible) {
        bail!("Unknown tool '{requested}' on '{server_name}'. Did you mean '{suggestion}'?");
    }
    bail!("Unknown tool '{requested}' on '{server_name}'")
}

pub(crate) fn has_real_tool(tools: &[Value], requested: &str) -> bool {
    let requested_key = normalize(requested);
    tool_names(tools).iter().any(|name| {
        name == requested || (!requested_key.is_empty() && normalize(name) == requested_key)
    })
}

pub(crate) fn is_discovery_pseudo(name: &str) -> bool {
    matches!(normalize(name).as_str(), "listtools" | "help")
}

pub(crate) fn normalize(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn tool_names(tools: &[Value]) -> Vec<String> {
    let mut names = Vec::new();
    for name in tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .filter(|name| !name.is_empty())
    {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_owned());
        }
    }
    names
}

fn suggestion(requested: &str, visible: &[&String]) -> Option<String> {
    let requested = normalize(requested);
    if requested.is_empty() {
        return None;
    }
    let mut candidates = visible
        .iter()
        .map(|name| (distance(&requested, &normalize(name)), (*name).clone()))
        .collect::<Vec<_>>();
    candidates.sort();
    let (distance, name) = candidates.first()?;
    let tied = candidates
        .iter()
        .skip(1)
        .any(|candidate| candidate.0 == *distance);
    if tied || *distance > requested.len().max(3) / 3 {
        return None;
    }
    Some(name.clone())
}

fn distance(left: &str, right: &str) -> usize {
    let right = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut final_cost = right.len();
    for (left_index, left_character) in left.chars().enumerate() {
        let mut current = vec![left_index + 1];
        let mut left_cost = left_index + 1;
        for ((right_character, upper_cost), diagonal_cost) in right
            .iter()
            .zip(previous.iter().skip(1))
            .zip(previous.iter())
        {
            let insertion = left_cost + 1;
            let deletion = upper_cost + 1;
            let substitution = diagonal_cost + usize::from(left_character != *right_character);
            left_cost = insertion.min(deletion).min(substitution);
            current.push(left_cost);
        }
        final_cost = left_cost;
        previous = current;
    }
    final_cost
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools(names: &[&str]) -> Vec<Value> {
        names.iter().map(|name| json!({"name":name})).collect()
    }

    fn allow_all(_: &str) -> bool {
        true
    }

    #[test]
    fn exact_names_win_over_normalized_aliases() -> Result<()> {
        assert_eq!(
            resolve_tool(
                "fixture",
                &tools(&["echo", "ECHO"]),
                Some("echo"),
                allow_all
            )?,
            "echo"
        );
        Ok(())
    }

    #[test]
    fn normalized_aliases_accept_case_and_separators() -> Result<()> {
        assert_eq!(
            resolve_tool("fixture", &tools(&["echo.v2"]), Some("Echo-V2"), allow_all)?,
            "echo.v2"
        );
        Ok(())
    }

    #[test]
    fn unrelated_unicode_or_empty_identifiers_never_become_aliases() -> Result<()> {
        let unicode = tools(&["\u{5220}\u{9664}"]);
        assert!(resolve_tool("fixture", &unicode, Some("\u{6dfb}\u{52a0}"), allow_all).is_err());
        assert_eq!(
            resolve_tool("fixture", &unicode, Some("\u{5220}\u{9664}"), allow_all)?,
            "\u{5220}\u{9664}"
        );
        assert!(resolve_tool("fixture", &tools(&["---"]), Some("___"), allow_all).is_err());
        assert!(!has_real_tool(&tools(&["---"]), "___"));
        Ok(())
    }

    #[test]
    fn ambiguous_aliases_never_choose_a_first_match() {
        let error = resolve_tool(
            "fixture",
            &tools(&["echo_v2", "echo-v2"]),
            Some("echov2"),
            allow_all,
        )
        .expect_err("ambiguous alias should fail");
        assert!(error.to_string().contains("ambiguous"));
        assert!(error.to_string().contains("echo_v2"));
        assert!(error.to_string().contains("echo-v2"));
    }

    #[test]
    fn typos_only_return_a_suggestion() {
        let error = resolve_tool("fixture", &tools(&["echo"]), Some("ech"), allow_all)
            .expect_err("typo should not execute");
        assert!(error.to_string().contains("Did you mean 'echo'?"));
    }

    #[test]
    fn excluded_names_are_not_suggested_or_resolved() {
        let error = resolve_tool(
            "fixture",
            &tools(&["echo", "secret"]),
            Some("secre"),
            |name| name != "secret",
        )
        .expect_err("excluded typo should fail");
        assert!(!error.to_string().contains("secret"));
        assert!(error.to_string().contains("Unknown tool"));
    }

    #[test]
    fn exact_excluded_names_do_not_bypass_the_filter() {
        let error = resolve_tool("fixture", &tools(&["help"]), Some("help"), |_| false)
            .expect_err("excluded exact name should fail");
        assert!(error.to_string().contains("excluded"));
    }

    #[test]
    fn real_names_shadow_pseudo_tools_even_when_excluded() {
        assert!(has_real_tool(&tools(&["help"]), "help"));
        assert!(has_real_tool(&tools(&["list_tools"]), "list-tools"));
    }

    #[test]
    fn normalized_excluded_names_do_not_shadow_with_a_fallback() {
        let error = resolve_tool(
            "fixture",
            &tools(&["list_tools"]),
            Some("list-tools"),
            |_| false,
        )
        .expect_err("excluded normalized name should fail");
        assert!(error.to_string().contains("excluded"));
    }
}
