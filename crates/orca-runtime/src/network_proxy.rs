use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use orca_core::config::PermissionProfileNetworkAccess;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeNetworkPolicy {
    domains: HashMap<String, PermissionProfileNetworkAccess>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeNetworkBlockReason {
    Allowlist,
    Denylist,
    Policy,
}

impl RuntimeNetworkBlockReason {
    fn proxy_error(self) -> &'static str {
        match self {
            Self::Allowlist => "blocked-by-allowlist",
            Self::Denylist => "blocked-by-denylist",
            Self::Policy => "blocked-by-policy",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeNetworkDecision {
    Allow,
    Block(RuntimeNetworkBlockReason),
}

impl RuntimeNetworkPolicy {
    pub fn new(domains: HashMap<String, PermissionProfileNetworkAccess>) -> Self {
        let domains = domains
            .into_iter()
            .map(|(domain, access)| (normalize_host(&domain), access))
            .collect();
        Self { domains }
    }

    pub fn is_empty(&self) -> bool {
        self.domains.is_empty()
    }

    fn access_for_host(&self, host: &str) -> Option<PermissionProfileNetworkAccess> {
        let host = normalize_host(host);
        self.domains.get(&host).copied().or_else(|| {
            self.domains.iter().find_map(|(pattern, access)| {
                domain_pattern_matches(pattern, &host).then_some(*access)
            })
        })
    }

    #[cfg(test)]
    fn allows_host(&self, host: &str) -> bool {
        matches!(self.decision_for_host(host), RuntimeNetworkDecision::Allow)
    }

    fn decision_for_host(&self, host: &str) -> RuntimeNetworkDecision {
        match self.access_for_host(host) {
            Some(PermissionProfileNetworkAccess::Deny) => {
                RuntimeNetworkDecision::Block(RuntimeNetworkBlockReason::Denylist)
            }
            Some(PermissionProfileNetworkAccess::Allow) => RuntimeNetworkDecision::Allow,
            None if self
                .domains
                .values()
                .any(|access| matches!(access, PermissionProfileNetworkAccess::Allow)) =>
            {
                RuntimeNetworkDecision::Block(RuntimeNetworkBlockReason::Allowlist)
            }
            None => RuntimeNetworkDecision::Allow,
        }
    }
}

pub struct RuntimeNetworkProxy {
    proxy_url: String,
    stop: Arc<AtomicBool>,
    accept_thread: Option<thread::JoinHandle<()>>,
}

impl RuntimeNetworkProxy {
    pub fn start(policy: RuntimeNetworkPolicy) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let policy = Arc::new(policy);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let accept_thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                };
                let policy = Arc::clone(&policy);
                thread::spawn(move || {
                    let _ = handle_proxy_connection(stream, &policy);
                });
            }
        });
        Ok(Self {
            proxy_url: format!("http://{addr}"),
            stop,
            accept_thread: Some(accept_thread),
        })
    }

    pub fn proxy_url(&self) -> &str {
        &self.proxy_url
    }
}

impl Drop for RuntimeNetworkProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
    }
}

fn handle_proxy_connection(mut client: TcpStream, policy: &RuntimeNetworkPolicy) -> io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(10))).ok();
    client.set_write_timeout(Some(Duration::from_secs(10))).ok();
    let mut reader = BufReader::new(client.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let request_line = request_line.trim_end_matches(['\r', '\n']);
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        headers.push(line);
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or("HTTP/1.1");
    let host = proxy_target_host(target)
        .or_else(|| header_host(&headers))
        .unwrap_or_default();
    if host.is_empty() {
        write_forbidden(&mut client, RuntimeNetworkBlockReason::Policy)?;
        return Ok(());
    }
    if let RuntimeNetworkDecision::Block(reason) = policy.decision_for_host(&host) {
        write_forbidden(&mut client, reason)?;
        return Ok(());
    }

    if method.eq_ignore_ascii_case("CONNECT") {
        return proxy_connect(client, target);
    }
    proxy_http(client, method, target, version, &headers)
}

fn proxy_http(
    mut client: TcpStream,
    method: &str,
    target: &str,
    version: &str,
    headers: &[String],
) -> io::Result<()> {
    let Some((host, port, path)) = parse_http_target(target, headers) else {
        write_forbidden(&mut client, RuntimeNetworkBlockReason::Policy)?;
        return Ok(());
    };
    let mut upstream = TcpStream::connect((host.as_str(), port))?;
    upstream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok();
    upstream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .ok();
    write!(upstream, "{method} {path} {version}\r\n")?;
    for header in headers {
        if header
            .split_once(':')
            .map(|(name, _)| name.eq_ignore_ascii_case("proxy-connection"))
            .unwrap_or(false)
        {
            continue;
        }
        upstream.write_all(header.as_bytes())?;
    }
    upstream.write_all(b"\r\n")?;
    io::copy(&mut upstream, &mut client)?;
    Ok(())
}

fn proxy_connect(mut client: TcpStream, target: &str) -> io::Result<()> {
    let (host, port) = split_host_port(target, 443);
    let mut upstream = TcpStream::connect((host.as_str(), port))?;
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    let mut client_read = client.try_clone()?;
    let mut upstream_write = upstream.try_clone()?;
    let join = thread::spawn(move || {
        let _ = io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut upstream, &mut client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = join.join();
    Ok(())
}

fn write_forbidden(client: &mut TcpStream, reason: RuntimeNetworkBlockReason) -> io::Result<()> {
    write!(
        client,
        "HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nx-proxy-error: {}\r\n\r\n",
        reason.proxy_error()
    )
}

fn parse_http_target(target: &str, headers: &[String]) -> Option<(String, u16, String)> {
    if let Some(rest) = target.strip_prefix("http://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (host, port) = split_host_port(authority, 80);
        return Some((host, port, format!("/{path}")));
    }
    let host = header_host(headers)?;
    let (host, port) = split_host_port(&host, 80);
    Some((host, port, target.to_string()))
}

fn proxy_target_host(target: &str) -> Option<String> {
    if let Some(rest) = target.strip_prefix("http://") {
        return Some(rest.split('/').next().unwrap_or_default().to_string());
    }
    if let Some(rest) = target.strip_prefix("https://") {
        return Some(rest.split('/').next().unwrap_or_default().to_string());
    }
    target.contains(':').then(|| target.to_string())
}

fn header_host(headers: &[String]) -> Option<String> {
    headers.iter().find_map(|header| {
        let (name, value) = header.split_once(':')?;
        name.eq_ignore_ascii_case("host")
            .then(|| value.trim().to_string())
    })
}

fn split_host_port(authority: &str, default_port: u16) -> (String, u16) {
    let authority = authority.trim().trim_matches(['[', ']']);
    if let Some((host, port)) = authority.rsplit_once(':')
        && let Ok(port) = port.parse::<u16>()
    {
        return (host.trim_matches(['[', ']']).to_string(), port);
    }
    (authority.to_string(), default_port)
}

fn normalize_host(host: &str) -> String {
    split_host_port(host, 0)
        .0
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn domain_pattern_matches(pattern: &str, host: &str) -> bool {
    if let Some(domain) = pattern.strip_prefix("**.") {
        return host == domain || host.ends_with(&format!(".{domain}"));
    }
    if let Some(domain) = pattern.strip_prefix("*.") {
        return host.ends_with(&format!(".{domain}")) && host != domain;
    }
    pattern == host
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_network_proxy_stops_accepting_after_drop() {
        let proxy = RuntimeNetworkProxy::start(RuntimeNetworkPolicy::new(HashMap::from([(
            "127.0.0.1".to_string(),
            PermissionProfileNetworkAccess::Allow,
        )])))
        .expect("start proxy");
        let addr = proxy
            .proxy_url()
            .strip_prefix("http://")
            .expect("proxy url prefix")
            .to_string();

        drop(proxy);

        for _ in 0..20 {
            if TcpStream::connect(&addr).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("proxy still accepts connections after drop");
    }

    #[test]
    fn runtime_network_policy_matches_scoped_domain_patterns() {
        let policy = RuntimeNetworkPolicy::new(HashMap::from([
            (
                "*.example.com".to_string(),
                PermissionProfileNetworkAccess::Allow,
            ),
            (
                "**.blocked.test".to_string(),
                PermissionProfileNetworkAccess::Deny,
            ),
        ]));

        assert!(policy.allows_host("api.example.com"));
        assert!(!policy.allows_host("example.com"));
        assert!(!policy.allows_host("blocked.test"));
        assert!(!policy.allows_host("api.blocked.test"));
    }

    #[test]
    fn runtime_network_policy_blocks_allowlist_misses_when_allow_entries_exist() {
        let policy = RuntimeNetworkPolicy::new(HashMap::from([(
            "api.example.com".to_string(),
            PermissionProfileNetworkAccess::Allow,
        )]));

        assert!(policy.allows_host("api.example.com"));
        assert!(!policy.allows_host("other.example.com"));
    }

    #[test]
    fn runtime_network_policy_allows_misses_when_policy_only_has_denies() {
        let policy = RuntimeNetworkPolicy::new(HashMap::from([(
            "blocked.example.com".to_string(),
            PermissionProfileNetworkAccess::Deny,
        )]));

        assert!(!policy.allows_host("blocked.example.com"));
        assert!(policy.allows_host("api.example.com"));
    }

    #[test]
    fn runtime_network_proxy_reports_denylist_blocks() {
        let proxy = RuntimeNetworkProxy::start(RuntimeNetworkPolicy::new(HashMap::from([
            (
                "api.example.com".to_string(),
                PermissionProfileNetworkAccess::Allow,
            ),
            (
                "blocked.example.com".to_string(),
                PermissionProfileNetworkAccess::Deny,
            ),
        ])))
        .expect("start proxy");
        let addr = proxy
            .proxy_url()
            .strip_prefix("http://")
            .expect("proxy url prefix")
            .to_string();
        let mut stream = TcpStream::connect(addr).expect("connect proxy");

        stream
            .write_all(
                b"GET http://blocked.example.com/ HTTP/1.1\r\nHost: blocked.example.com\r\n\r\n",
            )
            .expect("write request");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).expect("read response");

        assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        assert!(
            response.contains("x-proxy-error: blocked-by-denylist"),
            "response should identify denylist block: {response:?}"
        );
    }

    #[test]
    fn runtime_network_proxy_reports_allowlist_misses() {
        let proxy = RuntimeNetworkProxy::start(RuntimeNetworkPolicy::new(HashMap::from([(
            "api.example.com".to_string(),
            PermissionProfileNetworkAccess::Allow,
        )])))
        .expect("start proxy");
        let addr = proxy
            .proxy_url()
            .strip_prefix("http://")
            .expect("proxy url prefix")
            .to_string();
        let mut stream = TcpStream::connect(addr).expect("connect proxy");

        stream
            .write_all(b"GET http://other.example.com/ HTTP/1.1\r\nHost: other.example.com\r\n\r\n")
            .expect("write request");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).expect("read response");

        assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
        assert!(
            response.contains("x-proxy-error: blocked-by-allowlist"),
            "response should identify allowlist block: {response:?}"
        );
    }
}
