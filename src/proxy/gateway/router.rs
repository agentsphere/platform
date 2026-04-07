//! In-memory routing table and `HTTPRoute` request matching.
//!
//! Implements the full Gateway API `HTTPRoute` match specification:
//! hostname matching, path matching (prefix/exact/regex), header matching,
//! query parameter matching, method matching, and weighted backend selection.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

/// The in-memory routing table, rebuilt on every `HTTPRoute` change.
#[derive(Debug)]
pub struct RoutingTable {
    /// Routes indexed by exact hostname. Empty string key ("") = wildcard/fallback routes.
    by_hostname: HashMap<String, Vec<RouteEntry>>,
    /// When the table was last rebuilt.
    last_updated: Instant,
    /// Whether the initial route list has been loaded.
    ready: bool,
}

/// A single `HTTPRoute` resource parsed into match-ready form.
#[derive(Debug)]
pub struct RouteEntry {
    /// Where this route came from (for debugging).
    pub source: RouteSource,
    /// Ordered rules — first match wins.
    pub rules: Vec<RouteRule>,
}

/// Identifies the source `HTTPRoute` resource.
#[derive(Debug, Clone)]
pub struct RouteSource {
    pub name: String,
    pub namespace: String,
}

/// A single rule within a route, with match criteria and backends.
#[derive(Debug)]
pub struct RouteRule {
    /// Path match (None = match all paths).
    pub path_match: Option<PathMatch>,
    /// Header matches (all must match).
    pub header_matches: Vec<HeaderMatch>,
    /// Query parameter matches (all must match).
    pub query_param_matches: Vec<QueryMatch>,
    /// Method match (None = match all methods).
    pub method_match: Option<String>,
    /// Weighted backends to forward to.
    pub backends: Vec<WeightedBackend>,
}

/// Path matching types from the Gateway API spec.
#[derive(Debug)]
pub enum PathMatch {
    /// Match if the path starts with this prefix.
    Prefix(String),
    /// Match if the path equals this value exactly.
    Exact(String),
    /// Match if the path matches this regex.
    Regex(regex::Regex),
}

/// Header matching criteria.
#[derive(Debug)]
pub struct HeaderMatch {
    /// Header name (case-insensitive comparison).
    pub name: String,
    /// Match type.
    pub match_type: StringMatch,
}

/// Query parameter matching criteria.
#[derive(Debug)]
pub struct QueryMatch {
    /// Query parameter name.
    pub name: String,
    /// Match type.
    pub match_type: StringMatch,
}

/// String matching types for headers and query params.
#[derive(Debug)]
pub enum StringMatch {
    /// Exact string match.
    Exact(String),
    /// Regex match.
    Regex(regex::Regex),
}

/// A backend with weight for traffic splitting and resolved endpoints.
#[derive(Debug, Clone)]
pub struct WeightedBackend {
    /// Kubernetes Service name.
    pub service: String,
    /// Kubernetes namespace.
    pub namespace: String,
    /// Service port.
    pub port: u16,
    /// Relative weight (for traffic splitting).
    pub weight: u32,
    /// Resolved pod endpoints (from `EndpointSlice`).
    pub endpoints: Vec<SocketAddr>,
}

/// Result of a successful route match, including the selected endpoint and metadata.
#[derive(Debug, Clone)]
pub struct MatchResult {
    /// Selected backend endpoint address.
    pub endpoint: SocketAddr,
    /// Route name (from `HTTPRoute` `metadata.name`).
    pub route_name: String,
    /// Route namespace (from `HTTPRoute` `metadata.namespace`).
    pub route_namespace: String,
    /// Backend service name.
    pub backend_service: String,
    /// Backend service namespace.
    pub backend_namespace: String,
}

impl RoutingTable {
    /// Create an empty, not-ready routing table.
    pub fn empty() -> Self {
        Self {
            by_hostname: HashMap::new(),
            last_updated: Instant::now(),
            ready: false,
        }
    }

    /// Create a new ready routing table from parsed routes.
    pub fn new(by_hostname: HashMap<String, Vec<RouteEntry>>) -> Self {
        Self {
            by_hostname,
            last_updated: Instant::now(),
            ready: true,
        }
    }

    /// Whether the initial route sync has completed.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// When the table was last updated.
    pub fn last_updated(&self) -> Instant {
        self.last_updated
    }

    /// Match an incoming request against the routing table.
    ///
    /// Returns the selected backend endpoint, or `None` if no route matches.
    /// Implements first-match-wins semantics within each hostname's route list.
    pub fn match_request(
        &self,
        hostname: &str,
        path: &str,
        method: &str,
        headers: &[(String, String)],
        query_params: &[(String, String)],
    ) -> Option<SocketAddr> {
        self.match_request_full(hostname, path, method, headers, query_params)
            .map(|m| m.endpoint)
    }

    /// Match an incoming request and return full match metadata.
    ///
    /// Returns the selected backend endpoint plus route/backend metadata for
    /// observability (span attributes, rate limiting, etc.).
    pub fn match_request_full(
        &self,
        hostname: &str,
        path: &str,
        method: &str,
        headers: &[(String, String)],
        query_params: &[(String, String)],
    ) -> Option<MatchResult> {
        // Try exact hostname match first
        if let Some(result) =
            self.try_match_hostname_full(hostname, path, method, headers, query_params)
        {
            return Some(result);
        }

        // Try wildcard hostname matches (e.g., *.example.com)
        if let Some(domain_suffix) = hostname.split_once('.').map(|(_, rest)| rest) {
            let wildcard = format!("*.{domain_suffix}");
            if let Some(result) =
                self.try_match_hostname_full(&wildcard, path, method, headers, query_params)
            {
                return Some(result);
            }
        }

        // Try default/wildcard routes (empty hostname key)
        self.try_match_hostname_full("", path, method, headers, query_params)
    }

    /// Try to match against routes for a specific hostname key (full metadata).
    fn try_match_hostname_full(
        &self,
        hostname_key: &str,
        path: &str,
        method: &str,
        headers: &[(String, String)],
        query_params: &[(String, String)],
    ) -> Option<MatchResult> {
        let routes = self.by_hostname.get(hostname_key)?;
        for route in routes {
            for rule in &route.rules {
                if rule.matches(path, method, headers, query_params)
                    && let Some((endpoint, backend)) = rule.select_backend_full()
                {
                    return Some(MatchResult {
                        endpoint,
                        route_name: route.source.name.clone(),
                        route_namespace: route.source.namespace.clone(),
                        backend_service: backend.service.clone(),
                        backend_namespace: backend.namespace.clone(),
                    });
                }
            }
        }
        None
    }
}

impl RouteRule {
    /// Check if this rule matches the given request.
    fn matches(
        &self,
        path: &str,
        method: &str,
        headers: &[(String, String)],
        query_params: &[(String, String)],
    ) -> bool {
        // Path match
        if let Some(ref pm) = self.path_match
            && !pm.matches(path)
        {
            return false;
        }

        // Method match
        if let Some(ref m) = self.method_match
            && !m.eq_ignore_ascii_case(method)
        {
            return false;
        }

        // All header matches must pass
        for hm in &self.header_matches {
            let found = headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&hm.name));
            match found {
                Some((_, value)) => {
                    if !hm.match_type.matches(value) {
                        return false;
                    }
                }
                None => return false,
            }
        }

        // All query param matches must pass
        for qm in &self.query_param_matches {
            let found = query_params.iter().find(|(name, _)| name == &qm.name);
            match found {
                Some((_, value)) => {
                    if !qm.match_type.matches(value) {
                        return false;
                    }
                }
                None => return false,
            }
        }

        true
    }

    /// Select a backend endpoint using weighted random selection.
    fn select_backend(&self) -> Option<SocketAddr> {
        self.select_backend_full().map(|(addr, _)| addr)
    }

    /// Select a backend endpoint and return the chosen backend metadata.
    fn select_backend_full(&self) -> Option<(SocketAddr, &WeightedBackend)> {
        if self.backends.is_empty() {
            return None;
        }

        // Build cumulative weight distribution
        let total_weight: u32 = self.backends.iter().map(|b| b.weight).sum();
        if total_weight == 0 {
            // All weights zero — pick first backend with endpoints
            return self
                .backends
                .iter()
                .find(|b| !b.endpoints.is_empty())
                .and_then(|b| b.pick_endpoint().map(|ep| (ep, b)));
        }

        let roll = rand::random_range(0..total_weight);
        let mut cumulative = 0;
        for backend in &self.backends {
            cumulative += backend.weight;
            if roll < cumulative && !backend.endpoints.is_empty() {
                return backend.pick_endpoint().map(|ep| (ep, backend));
            }
        }

        // Fallback: first backend with endpoints
        self.backends
            .iter()
            .find(|b| !b.endpoints.is_empty())
            .and_then(|b| b.pick_endpoint().map(|ep| (ep, b)))
    }
}

impl PathMatch {
    /// Check if a request path matches this path match.
    fn matches(&self, path: &str) -> bool {
        match self {
            Self::Prefix(prefix) => {
                path == prefix || path.starts_with(&format!("{prefix}/"))
                    // Handle exact prefix match at path boundary
                    || (prefix == "/" && path.starts_with('/'))
            }
            Self::Exact(exact) => path == exact,
            Self::Regex(re) => re.is_match(path),
        }
    }
}

impl StringMatch {
    /// Check if a value matches this string match.
    fn matches(&self, value: &str) -> bool {
        match self {
            Self::Exact(exact) => value == exact,
            Self::Regex(re) => re.is_match(value),
        }
    }
}

impl WeightedBackend {
    /// Pick a random endpoint from this backend's resolved endpoints.
    fn pick_endpoint(&self) -> Option<SocketAddr> {
        if self.endpoints.is_empty() {
            return None;
        }
        if self.endpoints.len() == 1 {
            return Some(self.endpoints[0]);
        }
        let idx = rand::random_range(0..self.endpoints.len());
        Some(self.endpoints[idx])
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_backend(
        service: &str,
        port: u16,
        weight: u32,
        endpoints: Vec<SocketAddr>,
    ) -> WeightedBackend {
        WeightedBackend {
            service: service.into(),
            namespace: "default".into(),
            port,
            weight,
            endpoints,
        }
    }

    fn make_rule(
        path_match: Option<PathMatch>,
        header_matches: Vec<HeaderMatch>,
        query_param_matches: Vec<QueryMatch>,
        method_match: Option<String>,
        backends: Vec<WeightedBackend>,
    ) -> RouteRule {
        RouteRule {
            path_match,
            header_matches,
            query_param_matches,
            method_match,
            backends,
        }
    }

    fn endpoint(addr: &str) -> SocketAddr {
        addr.parse().unwrap()
    }

    fn simple_backend() -> Vec<WeightedBackend> {
        vec![make_backend(
            "svc",
            8080,
            100,
            vec![endpoint("10.0.0.1:8080")],
        )]
    }

    fn make_route(hostname: &str, rules: Vec<RouteRule>) -> (String, RouteEntry) {
        let key = hostname.to_string();
        let entry = RouteEntry {
            source: RouteSource {
                name: "test-route".into(),
                namespace: "default".into(),
            },
            rules,
        };
        (key, entry)
    }

    fn build_table(routes: Vec<(String, RouteEntry)>) -> RoutingTable {
        let mut by_hostname: HashMap<String, Vec<RouteEntry>> = HashMap::new();
        for (hostname, entry) in routes {
            by_hostname.entry(hostname).or_default().push(entry);
        }
        RoutingTable::new(by_hostname)
    }

    // -- Hostname matching --

    #[test]
    fn hostname_exact_match() {
        let (key, entry) = make_route(
            "api.example.com",
            vec![make_rule(None, vec![], vec![], None, simple_backend())],
        );
        let table = build_table(vec![(key, entry)]);

        let result = table.match_request("api.example.com", "/", "GET", &[], &[]);
        assert!(result.is_some());

        let result = table.match_request("other.example.com", "/", "GET", &[], &[]);
        assert!(result.is_none());
    }

    #[test]
    fn hostname_wildcard_match() {
        let (key, entry) = make_route(
            "*.example.com",
            vec![make_rule(None, vec![], vec![], None, simple_backend())],
        );
        let table = build_table(vec![(key, entry)]);

        let result = table.match_request("foo.example.com", "/", "GET", &[], &[]);
        assert!(result.is_some());

        let result = table.match_request("bar.example.com", "/", "GET", &[], &[]);
        assert!(result.is_some());

        // Exact hostname doesn't match wildcard pattern lookup
        let result = table.match_request("example.com", "/", "GET", &[], &[]);
        assert!(result.is_none());
    }

    #[test]
    fn wildcard_hostname_fallback() {
        // Routes with empty hostname key are wildcard fallbacks
        let (key, entry) = make_route(
            "",
            vec![make_rule(None, vec![], vec![], None, simple_backend())],
        );
        let table = build_table(vec![(key, entry)]);

        let result = table.match_request("anything.com", "/", "GET", &[], &[]);
        assert!(result.is_some());
    }

    // -- Path matching --

    #[test]
    fn path_prefix_match() {
        let (key, entry) = make_route(
            "",
            vec![make_rule(
                Some(PathMatch::Prefix("/api".into())),
                vec![],
                vec![],
                None,
                simple_backend(),
            )],
        );
        let table = build_table(vec![(key, entry)]);

        assert!(table.match_request("", "/api", "GET", &[], &[]).is_some());
        assert!(
            table
                .match_request("", "/api/users", "GET", &[], &[])
                .is_some()
        );
        assert!(table.match_request("", "/other", "GET", &[], &[]).is_none());
    }

    #[test]
    fn path_exact_match() {
        let (key, entry) = make_route(
            "",
            vec![make_rule(
                Some(PathMatch::Exact("/healthz".into())),
                vec![],
                vec![],
                None,
                simple_backend(),
            )],
        );
        let table = build_table(vec![(key, entry)]);

        assert!(
            table
                .match_request("", "/healthz", "GET", &[], &[])
                .is_some()
        );
        assert!(
            table
                .match_request("", "/healthz/", "GET", &[], &[])
                .is_none()
        );
        assert!(
            table
                .match_request("", "/health", "GET", &[], &[])
                .is_none()
        );
    }

    #[test]
    fn path_regex_match() {
        let re = regex::Regex::new(r"^/users/\d+$").unwrap();
        let (key, entry) = make_route(
            "",
            vec![make_rule(
                Some(PathMatch::Regex(re)),
                vec![],
                vec![],
                None,
                simple_backend(),
            )],
        );
        let table = build_table(vec![(key, entry)]);

        assert!(
            table
                .match_request("", "/users/123", "GET", &[], &[])
                .is_some()
        );
        assert!(
            table
                .match_request("", "/users/abc", "GET", &[], &[])
                .is_none()
        );
    }

    // -- Header matching --

    #[test]
    fn header_exact_match() {
        let (key, entry) = make_route(
            "",
            vec![make_rule(
                None,
                vec![HeaderMatch {
                    name: "x-version".into(),
                    match_type: StringMatch::Exact("v2".into()),
                }],
                vec![],
                None,
                simple_backend(),
            )],
        );
        let table = build_table(vec![(key, entry)]);

        let headers = vec![("x-version".into(), "v2".into())];
        assert!(table.match_request("", "/", "GET", &headers, &[]).is_some());

        let headers = vec![("x-version".into(), "v1".into())];
        assert!(table.match_request("", "/", "GET", &headers, &[]).is_none());

        // Missing header should not match
        assert!(table.match_request("", "/", "GET", &[], &[]).is_none());
    }

    #[test]
    fn header_regex_match() {
        let re = regex::Regex::new(r"^v\d+$").unwrap();
        let (key, entry) = make_route(
            "",
            vec![make_rule(
                None,
                vec![HeaderMatch {
                    name: "x-api-version".into(),
                    match_type: StringMatch::Regex(re),
                }],
                vec![],
                None,
                simple_backend(),
            )],
        );
        let table = build_table(vec![(key, entry)]);

        let headers = vec![("x-api-version".into(), "v2".into())];
        assert!(table.match_request("", "/", "GET", &headers, &[]).is_some());

        let headers = vec![("x-api-version".into(), "latest".into())];
        assert!(table.match_request("", "/", "GET", &headers, &[]).is_none());
    }

    #[test]
    fn header_match_case_insensitive_name() {
        let (key, entry) = make_route(
            "",
            vec![make_rule(
                None,
                vec![HeaderMatch {
                    name: "X-Version".into(),
                    match_type: StringMatch::Exact("v2".into()),
                }],
                vec![],
                None,
                simple_backend(),
            )],
        );
        let table = build_table(vec![(key, entry)]);

        let headers = vec![("x-version".into(), "v2".into())];
        assert!(table.match_request("", "/", "GET", &headers, &[]).is_some());
    }

    // -- Query param matching --

    #[test]
    fn query_param_exact_match() {
        let (key, entry) = make_route(
            "",
            vec![make_rule(
                None,
                vec![],
                vec![QueryMatch {
                    name: "version".into(),
                    match_type: StringMatch::Exact("2".into()),
                }],
                None,
                simple_backend(),
            )],
        );
        let table = build_table(vec![(key, entry)]);

        let params = vec![("version".into(), "2".into())];
        assert!(table.match_request("", "/", "GET", &[], &params).is_some());

        let params = vec![("version".into(), "1".into())];
        assert!(table.match_request("", "/", "GET", &[], &params).is_none());

        assert!(table.match_request("", "/", "GET", &[], &[]).is_none());
    }

    #[test]
    fn query_param_regex_match() {
        let re = regex::Regex::new(r"^\d+$").unwrap();
        let (key, entry) = make_route(
            "",
            vec![make_rule(
                None,
                vec![],
                vec![QueryMatch {
                    name: "page".into(),
                    match_type: StringMatch::Regex(re),
                }],
                None,
                simple_backend(),
            )],
        );
        let table = build_table(vec![(key, entry)]);

        let params = vec![("page".into(), "42".into())];
        assert!(table.match_request("", "/", "GET", &[], &params).is_some());

        let params = vec![("page".into(), "abc".into())];
        assert!(table.match_request("", "/", "GET", &[], &params).is_none());
    }

    // -- Method matching --

    #[test]
    fn method_match() {
        let (key, entry) = make_route(
            "",
            vec![make_rule(
                None,
                vec![],
                vec![],
                Some("POST".into()),
                simple_backend(),
            )],
        );
        let table = build_table(vec![(key, entry)]);

        assert!(table.match_request("", "/", "POST", &[], &[]).is_some());
        assert!(table.match_request("", "/", "GET", &[], &[]).is_none());
        // Case-insensitive
        assert!(table.match_request("", "/", "post", &[], &[]).is_some());
    }

    // -- Combined matches --

    #[test]
    fn combined_path_header_method_match() {
        let (key, entry) = make_route(
            "api.example.com",
            vec![make_rule(
                Some(PathMatch::Prefix("/api".into())),
                vec![HeaderMatch {
                    name: "x-auth".into(),
                    match_type: StringMatch::Exact("token-123".into()),
                }],
                vec![],
                Some("POST".into()),
                simple_backend(),
            )],
        );
        let table = build_table(vec![(key, entry)]);

        let headers = vec![("x-auth".into(), "token-123".into())];

        // All criteria match
        assert!(
            table
                .match_request("api.example.com", "/api/data", "POST", &headers, &[])
                .is_some()
        );

        // Wrong method
        assert!(
            table
                .match_request("api.example.com", "/api/data", "GET", &headers, &[])
                .is_none()
        );

        // Wrong path
        assert!(
            table
                .match_request("api.example.com", "/other", "POST", &headers, &[])
                .is_none()
        );

        // Missing header
        assert!(
            table
                .match_request("api.example.com", "/api/data", "POST", &[], &[])
                .is_none()
        );
    }

    // -- First-match-wins --

    #[test]
    fn first_match_wins_rule_ordering() {
        let backend_a = vec![make_backend(
            "svc-a",
            8080,
            100,
            vec![endpoint("10.0.0.1:8080")],
        )];
        let backend_b = vec![make_backend(
            "svc-b",
            8080,
            100,
            vec![endpoint("10.0.0.2:8080")],
        )];

        let (key, entry) = make_route(
            "",
            vec![
                make_rule(
                    Some(PathMatch::Prefix("/api".into())),
                    vec![],
                    vec![],
                    None,
                    backend_a,
                ),
                make_rule(
                    Some(PathMatch::Prefix("/".into())),
                    vec![],
                    vec![],
                    None,
                    backend_b,
                ),
            ],
        );
        let table = build_table(vec![(key, entry)]);

        // /api should match the first rule
        let result = table.match_request("", "/api/foo", "GET", &[], &[]);
        assert_eq!(result, Some(endpoint("10.0.0.1:8080")));

        // /other should match the second (fallback) rule
        let result = table.match_request("", "/other", "GET", &[], &[]);
        assert_eq!(result, Some(endpoint("10.0.0.2:8080")));
    }

    // -- Weighted backend selection --

    #[test]
    fn weighted_backend_selection_statistical() {
        let backends = vec![
            make_backend("svc-stable", 8080, 90, vec![endpoint("10.0.0.1:8080")]),
            make_backend("svc-canary", 8080, 10, vec![endpoint("10.0.0.2:8080")]),
        ];
        let rule = make_rule(None, vec![], vec![], None, backends);

        let mut stable_count = 0u32;
        let mut canary_count = 0u32;
        let iterations = 10_000;

        for _ in 0..iterations {
            if let Some(addr) = rule.select_backend() {
                if addr == endpoint("10.0.0.1:8080") {
                    stable_count += 1;
                } else {
                    canary_count += 1;
                }
            }
        }

        // 90/10 split should give roughly 9000/1000 with some variance
        // Allow generous tolerance (80-97% stable)
        let stable_pct = f64::from(stable_count) / f64::from(iterations) * 100.0;
        assert!(
            (80.0..=97.0).contains(&stable_pct),
            "expected ~90% stable, got {stable_pct:.1}% ({stable_count} stable, {canary_count} canary)"
        );
    }

    // -- Empty table --

    #[test]
    fn empty_table_returns_no_match() {
        let table = RoutingTable::empty();
        assert!(!table.is_ready());
        assert!(
            table
                .match_request("example.com", "/", "GET", &[], &[])
                .is_none()
        );
    }

    // -- No endpoints --

    #[test]
    fn backend_with_no_endpoints_skipped() {
        let backends = vec![
            make_backend("svc-no-eps", 8080, 100, vec![]),
            make_backend("svc-has-eps", 8080, 0, vec![endpoint("10.0.0.1:8080")]),
        ];
        let rule = make_rule(None, vec![], vec![], None, backends);

        // Should fall through to the backend that has endpoints
        let result = rule.select_backend();
        assert_eq!(result, Some(endpoint("10.0.0.1:8080")));
    }

    #[test]
    fn all_backends_no_endpoints_returns_none() {
        let backends = vec![
            make_backend("svc-a", 8080, 50, vec![]),
            make_backend("svc-b", 8080, 50, vec![]),
        ];
        let rule = make_rule(None, vec![], vec![], None, backends);
        assert!(rule.select_backend().is_none());
    }

    // -- Path prefix edge cases --

    #[test]
    fn path_prefix_root() {
        let pm = PathMatch::Prefix("/".into());
        assert!(pm.matches("/"));
        assert!(pm.matches("/anything"));
        assert!(pm.matches("/a/b/c"));
    }

    #[test]
    fn path_prefix_no_partial_segment() {
        let pm = PathMatch::Prefix("/api".into());
        // /api should match
        assert!(pm.matches("/api"));
        // /api/foo should match (starts with /api/)
        assert!(pm.matches("/api/foo"));
        // /apiary should NOT match (/api is not a segment boundary)
        assert!(!pm.matches("/apiary"));
    }

    // -- Ready state --

    #[test]
    fn new_table_is_ready() {
        let table = RoutingTable::new(HashMap::new());
        assert!(table.is_ready());
    }

    #[test]
    fn empty_table_is_not_ready() {
        let table = RoutingTable::empty();
        assert!(!table.is_ready());
    }

    // -- Hostname priority --

    #[test]
    fn exact_hostname_has_priority_over_wildcard() {
        let exact_backend = vec![make_backend(
            "exact-svc",
            8080,
            100,
            vec![endpoint("10.0.0.1:8080")],
        )];
        let wildcard_backend = vec![make_backend(
            "wildcard-svc",
            8080,
            100,
            vec![endpoint("10.0.0.2:8080")],
        )];
        let fallback_backend = vec![make_backend(
            "fallback-svc",
            8080,
            100,
            vec![endpoint("10.0.0.3:8080")],
        )];

        let routes = vec![
            make_route(
                "api.example.com",
                vec![make_rule(None, vec![], vec![], None, exact_backend)],
            ),
            make_route(
                "*.example.com",
                vec![make_rule(None, vec![], vec![], None, wildcard_backend)],
            ),
            make_route(
                "",
                vec![make_rule(None, vec![], vec![], None, fallback_backend)],
            ),
        ];
        let table = build_table(routes);

        // Exact hostname should match exact route
        let result = table.match_request("api.example.com", "/", "GET", &[], &[]);
        assert_eq!(result, Some(endpoint("10.0.0.1:8080")));

        // Other subdomain should match wildcard
        let result = table.match_request("other.example.com", "/", "GET", &[], &[]);
        assert_eq!(result, Some(endpoint("10.0.0.2:8080")));

        // Unrelated hostname should match fallback
        let result = table.match_request("other.net", "/", "GET", &[], &[]);
        assert_eq!(result, Some(endpoint("10.0.0.3:8080")));
    }

    // -- Zero weight backends --

    #[test]
    fn zero_weight_backends_uses_first_with_endpoints() {
        let backends = vec![
            make_backend("svc-a", 8080, 0, vec![endpoint("10.0.0.1:8080")]),
            make_backend("svc-b", 8080, 0, vec![endpoint("10.0.0.2:8080")]),
        ];
        let rule = make_rule(None, vec![], vec![], None, backends);

        // Should pick first backend with endpoints when all weights are 0
        let result = rule.select_backend();
        assert_eq!(result, Some(endpoint("10.0.0.1:8080")));
    }
}
