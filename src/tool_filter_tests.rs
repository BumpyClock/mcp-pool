use super::*;
use serde_json::json;

#[test]
fn catalog_retains_first_metadata_and_exact_unicode_names_in_order() {
    let mut catalog = vec![
        Value::Null,
        json!({"name":"","description":"empty"}),
        json!({"name":42}),
        json!({"name":"Å_Name","description":"first","inputSchema":{"type":"object"}}),
        json!({"name":"å-name"}),
        json!({"name":"Å_Name","description":"duplicate"}),
        json!({"name":"---"}),
        json!({"name":" "}),
    ];
    let expected = vec![
        json!({"name":"Å_Name","description":"first","inputSchema":{"type":"object"}}),
        json!({"name":"å-name"}),
        json!({"name":"---"}),
        json!({"name":" "}),
    ];
    normalize_catalog(&mut catalog);
    assert_eq!(catalog, expected);
    normalize_catalog(&mut catalog);
    assert_eq!(catalog, expected);
}
