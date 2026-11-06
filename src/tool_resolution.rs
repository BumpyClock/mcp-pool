use crate::tool_filter::ToolSelectionError;
use anyhow::Result;
use serde_json::Value;

/// Callers normalize catalogs before filtering or resolving names.
pub(crate) fn resolve_tool<F>(
    server_name: &str,
    tools: &[Value],
    requested: Option<&str>,
    mut allowed: F,
) -> Result<String>
where
    F: FnMut(&str) -> bool,
{
    let names: Vec<_> = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    let mut visible = Vec::new();
    for &name in &names {
        if allowed(name) {
            visible.push(name);
        }
    }

    let Some(requested) = requested else {
        return match visible.as_slice() {
            [name] => Ok((*name).to_owned()),
            [] => Err(ToolSelectionError(format!(
                "Server '{server_name}' exposes no available tools"
            ))
            .into()),
            _ => Err(ToolSelectionError(format!(
                "Missing tool: '{server_name}' exposes multiple tools; use call {server_name}.TOOL"
            ))
            .into()),
        };
    };

    if names.contains(&requested) {
        if visible.contains(&requested) {
            return Ok(requested.to_owned());
        }
        return Err(ToolSelectionError(format!(
            "Tool '{requested}' is excluded by the configuration for '{server_name}'"
        ))
        .into());
    }

    let requested_key = normalize(requested);
    let aliases = visible
        .iter()
        .filter(|name| !requested_key.is_empty() && normalize(name) == requested_key)
        .copied()
        .collect::<Vec<_>>();
    match aliases.as_slice() {
        [name] => return Ok((*name).to_owned()),
        [] => {}
        _ => {
            return Err(ToolSelectionError(format!(
                "Tool '{requested}' is ambiguous on '{server_name}'; matches: {}",
                aliases.join(", ")
            ))
            .into());
        }
    }
    if names.iter().any(|name| normalize(name) == requested_key) {
        return Err(ToolSelectionError(format!(
            "Tool '{requested}' is excluded by the configuration for '{server_name}'"
        ))
        .into());
    }

    if let Some(suggestion) = suggestion(requested, &visible) {
        return Err(ToolSelectionError(format!(
            "Unknown tool '{requested}' on '{server_name}'. Did you mean '{suggestion}'?"
        ))
        .into());
    }
    Err(ToolSelectionError(format!("Unknown tool '{requested}' on '{server_name}'")).into())
}

pub(crate) fn has_real_tool(tools: &[Value], requested: &str) -> bool {
    let requested_key = normalize(requested);
    tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .filter(|name| !name.is_empty())
        .any(|name| {
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

fn suggestion<'a>(requested: &str, visible: &[&'a str]) -> Option<&'a str> {
    let requested = normalize(requested);
    if requested.is_empty() {
        return None;
    }
    let mut candidates = visible
        .iter()
        .map(|name| (distance(&requested, &normalize(name)), *name))
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
    Some(*name)
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
    use anyhow::Context;
    use serde_json::json;

    fn tools(names: &[&str]) -> Vec<Value> {
        let mut catalog = names.iter().map(|name| json!({"name":name})).collect();
        crate::tool_filter::normalize_catalog(&mut catalog);
        catalog
    }

    fn allow_all(_: &str) -> bool {
        true
    }

    #[test]
    fn exact_names_win_over_normalized_aliases() -> Result<()> {
        assert_eq!(
            resolve_tool(
                "fixture",
                &tools(&["echo", "ECHO", "echo", "ECHO"]),
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
    fn ambiguous_aliases_preserve_first_seen_order_without_duplicates() -> Result<()> {
        let error = resolve_tool(
            "fixture",
            &tools(&["echo_v2", "echo-v2", "echo_v2", "echo.v2", "echo-v2"]),
            Some("echov2"),
            allow_all,
        )
        .err()
        .context("ambiguous alias should fail")?;
        assert_eq!(
            error.to_string(),
            "Tool 'echov2' is ambiguous on 'fixture'; matches: echo_v2, echo-v2, echo.v2"
        );
        Ok(())
    }

    #[test]
    fn typos_only_return_a_suggestion_despite_duplicate_names() -> Result<()> {
        let error = resolve_tool("fixture", &tools(&["echo", "echo"]), Some("ech"), allow_all)
            .err()
            .context("typo should not execute")?;
        assert_eq!(
            error.to_string(),
            "Unknown tool 'ech' on 'fixture'. Did you mean 'echo'?"
        );
        Ok(())
    }

    #[test]
    fn excluded_names_are_not_suggested_or_resolved() -> Result<()> {
        let error = resolve_tool(
            "fixture",
            &tools(&["echo", "secret", "secret"]),
            Some("secre"),
            |name| name != "secret",
        )
        .err()
        .context("excluded typo should fail")?;
        assert!(!error.to_string().contains("secret"));
        assert!(error.to_string().contains("Unknown tool"));
        Ok(())
    }

    #[test]
    fn malformed_and_empty_names_are_not_candidates() -> Result<()> {
        let mut catalog = vec![
            Value::Null,
            json!(false),
            json!("echo"),
            json!([]),
            json!({}),
            json!({"name":null}),
            json!({"name":42}),
            json!({"name":["echo"]}),
            json!({"name":""}),
        ];
        crate::tool_filter::normalize_catalog(&mut catalog);
        let error = resolve_tool("fixture", &catalog, None, allow_all)
            .err()
            .context("invalid names must not produce a tool")?;
        assert_eq!(
            error.to_string(),
            "Server 'fixture' exposes no available tools"
        );
        assert!(!has_real_tool(&catalog, ""));
        assert!(!has_real_tool(&catalog, "echo"));

        catalog.extend(tools(&["echo", "echo"]));
        assert_eq!(resolve_tool("fixture", &catalog, None, allow_all)?, "echo");
        assert!(has_real_tool(&catalog, "ECHO"));
        Ok(())
    }

    #[test]
    fn policy_filters_exact_names_and_aliases_consistently() -> Result<()> {
        let catalog = tools(&[
            "first",
            "second_alias",
            "first",
            "",
            "third",
            "second_alias",
        ]);
        for requested in [
            None,
            Some("first"),
            Some("second_alias"),
            Some("second-alias"),
            Some("unknown"),
        ] {
            let resolved = resolve_tool("fixture", &catalog, requested, |name| {
                name == "second_alias"
            });
            match requested {
                None | Some("second_alias") | Some("second-alias") => {
                    assert_eq!(resolved?, "second_alias")
                }
                Some("first") => assert_eq!(
                    resolved
                        .err()
                        .context("excluded tool must fail")?
                        .to_string(),
                    "Tool 'first' is excluded by the configuration for 'fixture'"
                ),
                _ => assert_eq!(
                    resolved
                        .err()
                        .context("unknown tool must fail")?
                        .to_string(),
                    "Unknown tool 'unknown' on 'fixture'"
                ),
            }
        }
        Ok(())
    }

    #[test]
    fn missing_tool_requires_exactly_one_distinct_visible_name() -> Result<()> {
        let catalog = tools(&["hidden", "echo", "echo", "hidden"]);
        assert_eq!(
            resolve_tool("fixture", &catalog, None, |name| name == "echo")?,
            "echo"
        );
        for (allowed, expected) in [
            (false, "Server 'fixture' exposes no available tools"),
            (
                true,
                "Missing tool: 'fixture' exposes multiple tools; use call fixture.TOOL",
            ),
        ] {
            let error = resolve_tool("fixture", &catalog, None, |_| allowed)
                .err()
                .context("zero or multiple visible names must fail")?;
            assert_eq!(error.to_string(), expected);
        }
        Ok(())
    }

    #[test]
    fn duplicate_real_pseudo_tools_remain_excluded_instead_of_falling_back() -> Result<()> {
        let catalog = tools(&["help", "list_tools", "help", "echo", "list_tools"]);
        for requested in ["help", "HELP", "list_tools", "list-tools"] {
            assert!(is_discovery_pseudo(requested));
            assert!(has_real_tool(&catalog, requested));
            let error = resolve_tool("fixture", &catalog, Some(requested), |name| name == "echo")
                .err()
                .context("real pseudo-tool must remain excluded")?;
            assert_eq!(
                error.to_string(),
                format!("Tool '{requested}' is excluded by the configuration for 'fixture'")
            );
        }
        Ok(())
    }

    #[test]
    fn excluded_exact_name_wins_over_a_visible_normalized_alias() -> Result<()> {
        let catalog = tools(&["ECHO", "echo", "ECHO", "echo"]);
        let error = resolve_tool("fixture", &catalog, Some("echo"), |name| name == "ECHO")
            .err()
            .context("excluded exact name must not select a visible alias")?;
        assert_eq!(
            error.to_string(),
            "Tool 'echo' is excluded by the configuration for 'fixture'"
        );
        assert_eq!(
            resolve_tool("fixture", &catalog, Some("Echo"), |name| name == "ECHO")?,
            "ECHO"
        );
        Ok(())
    }

    #[test]
    fn unicode_aliases_and_punctuation_exact_names_keep_their_meaning() -> Result<()> {
        let catalog = tools(&["\u{00c5}_Name", "---", "\u{00c5}_Name", "---"]);
        assert_eq!(
            resolve_tool("fixture", &catalog, Some("\u{00e5}-name"), allow_all)?,
            "\u{00c5}_Name"
        );
        assert!(has_real_tool(&catalog, "\u{00e5}-name"));
        assert_eq!(
            resolve_tool(
                "fixture",
                &tools(&["\u{0130}_Name", "\u{0130}_Name"]),
                Some("\u{0130}-name"),
                allow_all,
            )?,
            "\u{0130}_Name"
        );
        assert_eq!(
            resolve_tool("fixture", &catalog, Some("---"), allow_all)?,
            "---"
        );
        assert!(has_real_tool(&catalog, "---"));
        assert!(resolve_tool("fixture", &catalog, Some("___"), allow_all).is_err());
        assert!(!has_real_tool(&catalog, "___"));
        Ok(())
    }

    #[test]
    fn typo_suggestions_depend_only_on_distinct_visible_names() -> Result<()> {
        for names in [&["cat", "bat", "cat"][..], &["bat", "cat", "bat"][..]] {
            let catalog = tools(names);
            let error = resolve_tool("fixture", &catalog, Some("hat"), allow_all)
                .err()
                .context("a typo must never execute")?;
            assert_eq!(error.to_string(), "Unknown tool 'hat' on 'fixture'");
            let error = resolve_tool("fixture", &catalog, Some("hat"), |name| name != "bat")
                .err()
                .context("a unique suggestion must still not execute")?;
            assert_eq!(
                error.to_string(),
                "Unknown tool 'hat' on 'fixture'. Did you mean 'cat'?"
            );
        }
        Ok(())
    }
}
