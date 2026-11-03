use serde_json::json;

use super::{credential_summary, escape_text};

#[test]
fn human_text_preserves_printable_content_and_escapes_unicode_controls() {
    for text in [
        r"C:\Users\me\.mcp\config.json",
        "Docs' \"server\"",
        "東京",
        "cafe\u{301}",
        "👩\u{200d}💻",
    ] {
        assert_eq!(escape_text(text), text);
    }
    assert_eq!(
        escape_text("line\n\u{1b}[2J\u{202e}\u{061c}\u{2028}"),
        "line\\n\\u{1b}[2J\\u{202e}\\u{61c}\\u{2028}"
    );
}

#[test]
fn doctor_does_not_embed_unsafe_aliases_in_runnable_hints() {
    for name in [
        "alpha$(printf injected)",
        "docs search",
        "docs'local",
        "--reset",
        "cafe\u{301}",
    ] {
        let report =
            credential_summary(name, true, &json!({"bindingValid":true,"hasTokens":false}));
        assert!(
            report.contains("Run mcp-pool auth --help for explicit authorization options."),
            "{report}"
        );
        assert!(
            !report.contains(&format!("Run mcp-pool auth {name} to authorize")),
            "{report}"
        );
    }
}

#[test]
fn doctor_refresh_guidance_requires_cached_client_registration() {
    for registered in [false, true] {
        let report = credential_summary(
            "docs",
            true,
            &json!({
                "bindingValid":true, "hasTokens":true, "expired":true,
                "refreshable":true, "hasClientRegistration":registered
            }),
        );
        assert_eq!(
            report.contains("refresh available on use"),
            registered,
            "{report}"
        );
        assert_eq!(
            report.contains("Run mcp-pool auth docs"),
            !registered,
            "{report}"
        );
    }
}

#[test]
fn doctor_escapes_names_and_keeps_unsafe_aliases_out_of_hints() {
    let report = credential_summary(
        "docs\nforged\u{1b}[2J",
        true,
        &json!({"bindingValid":true,"hasTokens":false}),
    );
    assert!(!report.contains('\u{1b}'), "{report:?}");
    assert_eq!(
        report
            .chars()
            .filter(|character| *character == '\n')
            .count(),
        1,
        "{report:?}"
    );
    assert_eq!(
        report.matches("docs\\nforged\\u{1b}[2J").count(),
        1,
        "{report:?}"
    );
}

#[test]
fn doctor_reports_cached_credential_health_and_actionable_recovery() {
    for (configured, authenticated, tokens, expired, refreshable, binding, expected, needs_auth) in [
        (
            false,
            false,
            false,
            false,
            false,
            true,
            "OAuth not configured",
            false,
        ),
        (
            true,
            false,
            false,
            false,
            false,
            true,
            "no usable cached OAuth credentials",
            true,
        ),
        (
            true,
            true,
            true,
            false,
            false,
            true,
            "cached OAuth credentials valid",
            false,
        ),
        (
            true,
            false,
            true,
            true,
            true,
            true,
            "expired (refresh available on use)",
            false,
        ),
        (
            true,
            false,
            true,
            true,
            false,
            true,
            "cached OAuth credentials expired",
            true,
        ),
        (
            true,
            false,
            true,
            false,
            false,
            false,
            "credential binding mismatch",
            true,
        ),
    ] {
        let report = credential_summary(
            "docs",
            configured,
            &json!({
                "authenticated":authenticated, "hasTokens":tokens, "expired":expired,
                "refreshable":refreshable, "bindingValid":binding,
                "hasClientRegistration":true
            }),
        );
        assert!(report.starts_with("  docs:"), "{report}");
        assert!(report.contains(expected), "missing {expected} in {report}");
        assert_eq!(
            report.contains("Run mcp-pool auth docs"),
            needs_auth,
            "{report}"
        );
    }
}
