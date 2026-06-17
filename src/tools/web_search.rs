use serde::Deserialize;

use crate::tools::{ToolRequest, ToolResult, truncate_output};

#[derive(Debug, Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_count")]
    count: usize,
}

#[derive(Debug, Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}

#[derive(Debug, Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Debug, Deserialize)]
struct BraveResult {
    title: String,
    url: String,
    description: Option<String>,
}

pub fn execute(request: &ToolRequest, max_bytes: usize) -> ToolResult {
    let args = match parse_args(request) {
        Ok(args) => args,
        Err(error) => return ToolResult::failed(request, error, None),
    };

    let api_key = match std::env::var("BRAVE_SEARCH_API_KEY") {
        Ok(key) if !key.trim().is_empty() => key,
        _ => {
            return ToolResult::failed(
                request,
                "BRAVE_SEARCH_API_KEY is required for web_search",
                None,
            );
        }
    };

    let count = args.count.clamp(1, 10);
    let response = match reqwest::blocking::Client::new()
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", api_key)
        .query(&[("q", args.query.as_str()), ("count", &count.to_string())])
        .send()
    {
        Ok(response) => response,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("web search request failed: {error}"),
                None,
            );
        }
    };

    let status = response.status();
    if !status.is_success() {
        return ToolResult::failed(
            request,
            format!("web search request failed with {status}"),
            None,
        );
    }

    let response = match response.json::<BraveResponse>() {
        Ok(response) => response,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("invalid web search response: {error}"),
                None,
            );
        }
    };

    let results = response
        .web
        .map(|web| web.results)
        .unwrap_or_default()
        .into_iter()
        .take(count)
        .enumerate()
        .map(|(index, result)| {
            format!(
                "{}. {}\n{}\n{}",
                index + 1,
                result.title,
                result.description.unwrap_or_default(),
                result.url
            )
        })
        .collect::<Vec<_>>();

    let output = if results.is_empty() {
        "(no web search results)".to_string()
    } else {
        results.join("\n\n")
    };
    let (output, truncated) = truncate_output(output, max_bytes);
    ToolResult::completed(request, output, truncated)
}

fn parse_args(request: &ToolRequest) -> Result<SearchArgs, String> {
    let Some(raw) = request.raw_arguments.as_deref() else {
        return request
            .target
            .as_deref()
            .filter(|query| !query.trim().is_empty())
            .map(|query| SearchArgs {
                query: query.to_string(),
                count: default_count(),
            })
            .ok_or_else(|| "web_search query is required".to_string());
    };
    let args: SearchArgs =
        serde_json::from_str(raw).map_err(|error| format!("invalid arguments: {error}"))?;
    if args.query.trim().is_empty() {
        return Err("web_search query is required".to_string());
    }
    Ok(args)
}

fn default_count() -> usize {
    5
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::policy::ActionKind;
    use crate::tools::ToolName;

    fn request(raw_arguments: Option<String>, target: Option<String>) -> ToolRequest {
        ToolRequest {
            id: "search-1".to_string(),
            name: ToolName::WebSearch,
            action: ActionKind::Read,
            target,
            raw_arguments,
        }
    }

    #[test]
    fn parses_json_args() {
        let args = parse_args(&request(
            Some(r#"{"query":"rust","count":3}"#.to_string()),
            None,
        ))
        .unwrap();
        assert_eq!(args.query, "rust");
        assert_eq!(args.count, 3);
    }

    #[test]
    fn parses_target_fallback() {
        let args = parse_args(&request(None, Some("rust async".to_string()))).unwrap();
        assert_eq!(args.query, "rust async");
        assert_eq!(args.count, 5);
    }
}
