# Web Search 免费 Fallback：DuckDuckGo HTML 爬取

## Context
当前 `web_search` 工具强制要求 `BRAVE_SEARCH_API_KEY` 环境变量，没有 Key 就直接报错。
用户希望无需任何 API Key 也能使用搜索功能。方案：无 Brave Key 时自动 fallback 到 DuckDuckGo HTML 爬取（免费、无需注册、纯 HTTP）。

## 方案

### 修改文件：`src/tools/web_search.rs`

**策略**：
1. 有 `BRAVE_SEARCH_API_KEY` → 走 Brave API（现有逻辑不变）
2. 无 Key → 走 DuckDuckGo HTML 爬取

**DuckDuckGo 实现**：
- GET `https://html.duckduckgo.com/html/?q=<query>` 
- 解析 HTML 中的 `.result__a` (标题+URL) 和 `.result__snippet` (摘要)
- 纯字符串解析（不引入 HTML parser 依赖），用简单的标签定位
- User-Agent 设为标准浏览器值

**代码结构**：
```rust
pub fn execute(request: &ToolRequest, max_bytes: usize) -> ToolResult {
    let args = parse_args(request)?;
    
    let result = match std::env::var("BRAVE_SEARCH_API_KEY") {
        Ok(key) if !key.trim().is_empty() => search_brave(&args, &key),
        _ => search_duckduckgo(&args),
    };
    
    // 格式化输出...
}

fn search_brave(args: &SearchArgs, api_key: &str) -> Result<Vec<SearchResult>, String> { ... }
fn search_duckduckgo(args: &SearchArgs) -> Result<Vec<SearchResult>, String> { ... }
```

**统一结果类型**：
```rust
struct SearchResult {
    title: String,
    url: String,
    description: String,
}
```

### 不引入新依赖
- 已有 `reqwest::blocking` — 复用
- HTML 解析用简单的字符串 find/split，DuckDuckGo HTML 页面结构稳定

## 验证
1. `cargo test` — 现有测试通过
2. 新增测试：解析 DuckDuckGo HTML fixture
3. 手动验证：不设 `BRAVE_SEARCH_API_KEY`，运行 `orca exec --provider mock "web_search rust async"` 观察输出
