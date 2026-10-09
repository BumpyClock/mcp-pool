use anyhow::{Context, Result};

pub(crate) fn split_http(selector: &str) -> Result<Option<(String, Option<String>)>> {
    let selector = selector.trim();
    if !selector.starts_with("http://") && !selector.starts_with("https://") {
        return Ok(None);
    }
    let mut url = reqwest::Url::parse(selector).context("Invalid HTTP server selector")?;
    let pathname = url.path().to_owned();
    let segment = pathname.rsplit('/').next().unwrap_or("");
    let selected = segment.rsplit_once('.').filter(|(base, tool)| {
        !base.is_empty()
            && !tool.is_empty()
            && tool.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            })
    });
    let tool = if let Some((base, tool)) = selected {
        let directory_length = pathname.len().saturating_sub(segment.len());
        let directory = pathname
            .get(..directory_length)
            .context("Invalid URL path")?;
        url.set_path(&format!("{directory}{base}"));
        Some(tool.to_owned())
    } else {
        None
    };
    url.set_fragment(None);
    Ok(Some((url.to_string(), tool)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_split_only_the_final_path_segment_and_keep_queries() -> Result<()> {
        assert_eq!(
            split_http("https://example.test/mcp.search?tenant=acme#local")?,
            Some((
                "https://example.test/mcp?tenant=acme".to_owned(),
                Some("search".to_owned())
            ))
        );
        assert_eq!(
            split_http("https://example.test/mcp?tenant=acme.search")?,
            Some((
                "https://example.test/mcp?tenant=acme.search".to_owned(),
                None
            ))
        );
        assert_eq!(
            split_http("https://example.test/.well-known/mcp")?,
            Some(("https://example.test/.well-known/mcp".to_owned(), None))
        );
        assert_eq!(split_http("docs.search")?, None);
        Ok(())
    }
}
