use serde::Deserialize;
use serde_json::Value;

use orca_core::tool_types::{ToolRequest, ToolResult, truncate_output};

#[derive(Debug, Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_count")]
    count: usize,
    #[serde(default, deserialize_with = "deserialize_freshness")]
    freshness: Option<String>,
}

struct SearchResult {
    title: String,
    url: String,
    description: String,
}

pub fn execute(request: &ToolRequest, max_bytes: usize) -> ToolResult {
    let args = match parse_args(request) {
        Ok(args) => args,
        Err(error) => return ToolResult::failed(request, error, None),
    };

    let results = match std::env::var("BRAVE_SEARCH_API_KEY") {
        Ok(key) if !key.trim().is_empty() => search_brave(&args, &key),
        _ => search_exa(&args),
    };

    let results = match results {
        Ok(results) => results,
        Err(error) => return ToolResult::failed(request, error, None),
    };

    let output = if results.is_empty() {
        "(no web search results)".to_string()
    } else {
        results
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                format!(
                    "{}. {}\n{}\n{}",
                    index + 1,
                    result.title,
                    result.description,
                    result.url
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let (output, truncated) = truncate_output(output, max_bytes);
    ToolResult::completed(request, output, truncated)
}

// --- Brave Search API ---

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

fn search_brave(args: &SearchArgs, api_key: &str) -> Result<Vec<SearchResult>, String> {
    let count = args.count.clamp(1, 10);
    let query_params = brave_query_params(args, count);
    let response = reqwest::blocking::Client::new()
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", api_key)
        .query(&query_params)
        .send()
        .map_err(|e| format!("web search request failed: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("web search request failed with {status}"));
    }

    let body: BraveResponse = response
        .json()
        .map_err(|e| format!("invalid web search response: {e}"))?;

    Ok(body
        .web
        .map(|web| web.results)
        .unwrap_or_default()
        .into_iter()
        .take(count)
        .map(|r| SearchResult {
            title: r.title,
            url: r.url,
            description: r.description.unwrap_or_default(),
        })
        .collect())
}

fn brave_query_params(args: &SearchArgs, count: usize) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("q", args.query.clone()),
        ("count", count.clamp(1, 10).to_string()),
    ];
    if let Some(freshness) = args.freshness.as_ref() {
        params.push(("freshness", freshness.clone()));
    }
    params
}

// --- Exa MCP fallback (no API key required) ---

fn search_exa(args: &SearchArgs) -> Result<Vec<SearchResult>, String> {
    let count = args.count.clamp(1, 10);
    let query = exa_query(args);
    let request_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "web_search_exa",
            "arguments": {
                "query": query,
                "type": "auto",
                "numResults": count
            }
        }
    });

    let response = reqwest::blocking::Client::new()
        .post("https://mcp.exa.ai/mcp")
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(25))
        .json(&request_body)
        .send()
        .map_err(|e| format!("Exa search failed: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("Exa search failed with {status}"));
    }

    let text = response
        .text()
        .map_err(|e| format!("failed to read Exa response: {e}"))?;

    parse_exa_response(&text)
}

fn parse_exa_response(text: &str) -> Result<Vec<SearchResult>, String> {
    // Response may be SSE (data: {...}) or direct JSON
    let json_str = if let Some(data_line) = text.lines().find(|l| l.starts_with("data: ")) {
        &data_line[6..]
    } else {
        text.trim()
    };

    let response: Value =
        serde_json::from_str(json_str).map_err(|e| format!("invalid Exa response: {e}"))?;

    let content_text = response
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");

    if content_text.is_empty() {
        return Ok(Vec::new());
    }

    Ok(parse_exa_text_results(content_text))
}

fn parse_exa_text_results(text: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut title = String::new();
    let mut url = String::new();
    let mut snippet = String::new();

    for line in text.lines() {
        let line = line.trim();
        if let Some(t) = line.strip_prefix("Title: ") {
            // Save previous result
            if !title.is_empty() && !url.is_empty() {
                results.push(SearchResult {
                    title: std::mem::take(&mut title),
                    url: std::mem::take(&mut url),
                    description: std::mem::take(&mut snippet),
                });
            }
            title = t.to_string();
            snippet.clear();
        } else if let Some(u) = line.strip_prefix("URL: ") {
            url = u.to_string();
        } else if let Some(s) = line.strip_prefix("Text: ") {
            snippet = s.chars().take(300).collect();
        }
    }

    // Save last result
    if !title.is_empty() && !url.is_empty() {
        results.push(SearchResult {
            title,
            url,
            description: snippet,
        });
    }

    results
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
                freshness: infer_freshness(query),
            })
            .ok_or_else(|| "web_search query is required".to_string());
    };
    let args: SearchArgs =
        serde_json::from_str(raw).map_err(|error| format!("invalid arguments: {error}"))?;
    if args.query.trim().is_empty() {
        return Err("web_search query is required".to_string());
    }
    Ok(SearchArgs {
        freshness: args.freshness.or_else(|| infer_freshness(&args.query)),
        ..args
    })
}

fn default_count() -> usize {
    5
}

fn deserialize_freshness<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw.and_then(|value| {
        let value = value.trim();
        matches!(value, "pd" | "pw" | "pm" | "py" | _ if is_custom_freshness(value))
            .then(|| value.to_string())
    }))
}

fn infer_freshness(query: &str) -> Option<String> {
    let query = query.to_ascii_lowercase();
    if query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| {
            matches!(
                token,
                "latest" | "current" | "recent" | "today" | "news" | "updates" | "update"
            )
        })
    {
        Some("pm".to_string())
    } else {
        None
    }
}

fn is_custom_freshness(value: &str) -> bool {
    let Some((start, end)) = value.split_once("to") else {
        return false;
    };
    is_iso_date(start) && is_iso_date(end)
}

fn is_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(idx, byte)| matches!(idx, 4 | 7) || byte.is_ascii_digit())
}

fn exa_query(args: &SearchArgs) -> String {
    match args.freshness.as_deref() {
        Some("pd") => format!("{} from the last 24 hours", args.query),
        Some("pw") => format!("{} from the last 7 days", args.query),
        Some("pm") => format!("{} from the last 31 days", args.query),
        Some("py") => format!("{} from the last year", args.query),
        Some(custom) => format!(
            "{} published between {}",
            args.query,
            custom.replace("to", " and ")
        ),
        None => args.query.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::ToolName;

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

    #[test]
    fn parses_recency_intent_query_with_month_freshness() {
        let args = parse_args(&request(
            Some(r#"{"query":"deepseek latest news","count":3}"#.to_string()),
            None,
        ))
        .unwrap();

        assert_eq!(args.freshness.as_deref(), Some("pm"));
    }

    #[test]
    fn brave_query_params_include_inferred_freshness() {
        let args = parse_args(&request(
            Some(r#"{"query":"deepseek latest news","count":3}"#.to_string()),
            None,
        ))
        .unwrap();

        let params = brave_query_params(&args, args.count);

        assert_eq!(
            params,
            vec![
                ("q", "deepseek latest news".to_string()),
                ("count", "3".to_string()),
                ("freshness", "pm".to_string())
            ]
        );
    }

    #[test]
    fn exa_query_includes_recency_window_when_backend_has_no_freshness_arg() {
        let args = parse_args(&request(
            Some(r#"{"query":"deepseek latest news","count":3}"#.to_string()),
            None,
        ))
        .unwrap();

        assert_eq!(
            exa_query(&args),
            "deepseek latest news from the last 31 days"
        );
    }

    #[test]
    fn parse_exa_text_results_extracts_entries() {
        let text = "\
Title: Rust Programming Language
URL: https://www.rust-lang.org/
Text: A language empowering everyone to build reliable software.

Title: Rust Documentation
URL: https://doc.rust-lang.org/
Text: Official Rust documentation and guides.";
        let results = parse_exa_text_results(text);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert_eq!(
            results[0].description,
            "A language empowering everyone to build reliable software."
        );
        assert_eq!(results[1].url, "https://doc.rust-lang.org/");
        assert_eq!(results[1].title, "Rust Documentation");
    }

    #[test]
    fn parse_exa_response_handles_sse_format() {
        let sse = r#"data: {"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"Title: Example\nURL: https://example.com/\nText: An example site."}]}}"#;
        let results = parse_exa_response(sse).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example");
        assert_eq!(results[0].url, "https://example.com/");
    }
}
