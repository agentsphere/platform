//! Kubernetes CRD watcher for `HTTPRoute` and `EndpointSlice` resources.
//!
//! Watches `HTTPRoute` resources via `kube::runtime::watcher()` using `DynamicObject`.
//! On every event, rebuilds the full `RoutingTable` and atomically swaps it via `ArcSwap`.
//! Also watches `EndpointSlice` resources to resolve Service names to pod IPs.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;

use futures_util::StreamExt;
use kube::api::{Api, DynamicObject};
use kube::discovery::ApiResource;
use kube::runtime::watcher::{self, Event, watcher as kube_watcher};
use tokio::sync::watch;

type WatcherStream =
    Pin<Box<dyn futures_util::Stream<Item = Result<Event<DynamicObject>, watcher::Error>> + Send>>;

use super::router::{
    HeaderMatch, PathMatch, QueryMatch, RouteEntry, RouteRule, RouteSource, RoutingTable,
    StringMatch, WeightedBackend,
};
use super::{GatewayConfig, SharedRateLimitConfigs, SharedRoutingTable};

/// Run the watcher loop.
///
/// Watches `HTTPRoute` CRDs and `EndpointSlice` resources. On every change,
/// rebuilds the full routing table and swaps it atomically. Also extracts
/// rate limit annotations and updates the shared rate limit config map.
#[tracing::instrument(skip_all)]
pub async fn run(
    kube_client: kube::Client,
    config: GatewayConfig,
    table: SharedRoutingTable,
    rate_limit_configs: SharedRateLimitConfigs,
    mut shutdown: watch::Receiver<()>,
) {
    loop {
        let httproute_ar = ApiResource {
            group: "gateway.networking.k8s.io".into(),
            version: "v1".into(),
            api_version: "gateway.networking.k8s.io/v1".into(),
            kind: "HTTPRoute".into(),
            plural: "httproutes".into(),
        };

        // Accumulate all known HTTPRoutes
        let mut all_routes: HashMap<String, DynamicObject> = HashMap::new();
        // Track EndpointSlice data: service "namespace/name" -> Vec<SocketAddr>
        let endpoint_cache = load_endpoint_slices(&kube_client, &config.watch_namespaces).await;

        let mut stream: WatcherStream = if config.watch_namespaces.is_empty() {
            // No namespace filter — watch cluster-wide (requires ClusterRoleBinding)
            let api = Api::all_with(kube_client.clone(), &httproute_ar);
            Box::pin(kube_watcher(api, watcher::Config::default()))
        } else {
            // Per-namespace watches (works with namespace-scoped RoleBindings)
            let streams: Vec<WatcherStream> = config
                .watch_namespaces
                .iter()
                .map(|ns| {
                    let api = Api::namespaced_with(kube_client.clone(), ns, &httproute_ar);
                    Box::pin(kube_watcher(api, watcher::Config::default())) as WatcherStream
                })
                .collect();
            Box::pin(futures_util::stream::select_all(streams))
        };

        if run_watcher_loop(
            &mut stream,
            &mut all_routes,
            &endpoint_cache,
            &config,
            &table,
            &rate_limit_configs,
            &mut shutdown,
        )
        .await
        {
            return; // shutdown
        }
    }
}

/// Process watcher events in a loop. Returns `true` if shutdown was requested.
async fn run_watcher_loop(
    stream: &mut WatcherStream,
    all_routes: &mut HashMap<String, DynamicObject>,
    endpoint_cache: &HashMap<String, Vec<SocketAddr>>,
    config: &GatewayConfig,
    table: &SharedRoutingTable,
    rate_limit_configs: &SharedRateLimitConfigs,
    shutdown: &mut watch::Receiver<()>,
) -> bool {
    let mut init_done = false;
    loop {
        tokio::select! {
            _ = shutdown.changed() => return true,
            event = stream.next() => {
                match event {
                    Some(Ok(Event::Init)) => {
                        // Beginning of initial list — clear accumulated routes.
                        // With merged per-namespace streams, Init from one namespace
                        // clears everything; the subsequent InitApply events from all
                        // namespaces will repopulate. This is safe because the merged
                        // stream replays all objects during its initial sync.
                        all_routes.clear();
                    }
                    Some(Ok(Event::InitApply(obj))) => {
                        let key = route_key(&obj);
                        all_routes.insert(key, obj);
                    }
                    Some(Ok(Event::InitDone)) => {
                        if !init_done {
                            init_done = true;
                            tracing::info!(
                                route_count = all_routes.len(),
                                "initial HTTPRoute list complete"
                            );
                        }
                        let new_table = build_routing_table(all_routes, endpoint_cache, config);
                        table.store(std::sync::Arc::new(new_table));
                        update_rate_limit_configs(all_routes, rate_limit_configs, config);
                    }
                    Some(Ok(Event::Apply(obj))) => {
                        let key = route_key(&obj);
                        tracing::debug!(route = %key, "HTTPRoute applied");
                        all_routes.insert(key, obj);
                        let new_table = build_routing_table(all_routes, endpoint_cache, config);
                        table.store(std::sync::Arc::new(new_table));
                        update_rate_limit_configs(all_routes, rate_limit_configs, config);
                    }
                    Some(Ok(Event::Delete(obj))) => {
                        let key = route_key(&obj);
                        tracing::debug!(route = %key, "HTTPRoute deleted");
                        all_routes.remove(&key);
                        let new_table = build_routing_table(all_routes, endpoint_cache, config);
                        table.store(std::sync::Arc::new(new_table));
                        update_rate_limit_configs(all_routes, rate_limit_configs, config);
                    }
                    None => {
                        tracing::debug!("HTTPRoute watcher stream ended, restarting");
                        return false;
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "HTTPRoute watcher error, restarting");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        return false;
                    }
                }
            }
        }
    }
}

/// Wait until the routing table is marked as ready (initial sync done).
pub async fn wait_for_ready(table: &SharedRoutingTable) {
    loop {
        if table.load().is_ready() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Build a unique key for an `HTTPRoute` object.
fn route_key(obj: &DynamicObject) -> String {
    let ns = obj.metadata.namespace.as_deref().unwrap_or("default");
    let name = obj.metadata.name.as_deref().unwrap_or("unknown");
    format!("{ns}/{name}")
}

/// Load `EndpointSlice` resources and build a service -> endpoints map.
///
/// When `watch_namespaces` is non-empty, loads per-namespace (works with
/// namespace-scoped `RoleBindings`). Otherwise falls back to cluster-wide list.
async fn load_endpoint_slices(
    kube_client: &kube::Client,
    watch_namespaces: &[String],
) -> HashMap<String, Vec<SocketAddr>> {
    let es_ar = ApiResource {
        group: "discovery.k8s.io".into(),
        version: "v1".into(),
        api_version: "discovery.k8s.io/v1".into(),
        kind: "EndpointSlice".into(),
        plural: "endpointslices".into(),
    };

    let mut cache: HashMap<String, Vec<SocketAddr>> = HashMap::new();

    if watch_namespaces.is_empty() {
        // Cluster-wide (requires ClusterRoleBinding)
        let api: Api<DynamicObject> = Api::all_with(kube_client.clone(), &es_ar);
        list_endpoint_slices_into(&api, &mut cache).await;
    } else {
        // Per-namespace (works with namespace-scoped RoleBindings)
        for ns in watch_namespaces {
            let api: Api<DynamicObject> = Api::namespaced_with(kube_client.clone(), ns, &es_ar);
            list_endpoint_slices_into(&api, &mut cache).await;
        }
    }

    tracing::info!(services = cache.len(), "loaded EndpointSlice cache");
    cache
}

/// List endpoint slices from a single API and merge into cache.
async fn list_endpoint_slices_into(
    api: &Api<DynamicObject>,
    cache: &mut HashMap<String, Vec<SocketAddr>>,
) {
    match api.list(&kube::api::ListParams::default()).await {
        Ok(list) => {
            for es in list.items {
                if let Some((svc_key, addrs)) = parse_endpoint_slice(&es) {
                    cache.entry(svc_key).or_default().extend(addrs);
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to list EndpointSlices");
        }
    }
}

/// Parse an `EndpointSlice` `DynamicObject` into (`service_key`, addresses).
fn parse_endpoint_slice(es: &DynamicObject) -> Option<(String, Vec<SocketAddr>)> {
    let ns = es.metadata.namespace.as_deref().unwrap_or("default");

    // Get the service name from the kubernetes.io/service-name label
    let svc_name = es
        .metadata
        .labels
        .as_ref()?
        .get("kubernetes.io/service-name")?;
    let svc_key = format!("{ns}/{svc_name}");

    // Parse endpoints from the data
    let endpoints = es.data.get("endpoints")?.as_array()?;
    let ports = es.data.get("ports").and_then(|p| p.as_array());

    let mut addrs = Vec::new();
    for ep in endpoints {
        // Check conditions.ready
        let ready = ep
            .get("conditions")
            .and_then(|c| c.get("ready"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        if !ready {
            continue;
        }

        let addresses = ep.get("addresses").and_then(|a| a.as_array())?;
        for addr in addresses {
            if let Some(ip) = addr.as_str() {
                // Get port from ports array
                if let Some(ports) = ports {
                    for port_obj in ports {
                        if let Some(port) = port_obj.get("port").and_then(serde_json::Value::as_u64)
                        {
                            #[allow(clippy::cast_possible_truncation)]
                            let port = port as u16;
                            if let Ok(sa) = format!("{ip}:{port}").parse::<SocketAddr>() {
                                addrs.push(sa);
                            }
                        }
                    }
                }
            }
        }
    }

    Some((svc_key, addrs))
}

/// Build a `RoutingTable` from all known `HTTPRoute` objects.
fn build_routing_table(
    routes: &HashMap<String, DynamicObject>,
    endpoint_cache: &HashMap<String, Vec<SocketAddr>>,
    config: &GatewayConfig,
) -> RoutingTable {
    let mut by_hostname: HashMap<String, Vec<RouteEntry>> = HashMap::new();

    for obj in routes.values() {
        // Filter by namespace if configured (dev/test isolation)
        if !config.watch_namespaces.is_empty() {
            let obj_ns = obj.metadata.namespace.as_deref().unwrap_or("");
            if !config.watch_namespaces.iter().any(|ns| ns == obj_ns) {
                continue;
            }
        }

        // Filter by parentRef matching our gateway
        if !matches_parent_ref(obj, &config.gateway_name, &config.gateway_namespace) {
            continue;
        }

        let source = RouteSource {
            name: obj.metadata.name.clone().unwrap_or_default(),
            namespace: obj.metadata.namespace.clone().unwrap_or_default(),
        };

        let hostnames = extract_hostnames(obj);
        let rules = extract_rules(obj, endpoint_cache);

        let entry = RouteEntry { source, rules };

        if hostnames.is_empty() {
            // No hostnames specified — wildcard/fallback
            by_hostname.entry(String::new()).or_default().push(entry);
        } else {
            for hostname in hostnames {
                by_hostname
                    .entry(hostname)
                    .or_default()
                    .push(entry.clone_entry());
            }
        }
    }

    RoutingTable::new(by_hostname)
}

impl RouteEntry {
    /// Clone a `RouteEntry` (needed because we store per-hostname copies).
    fn clone_entry(&self) -> Self {
        Self {
            source: self.source.clone(),
            rules: self
                .rules
                .iter()
                .map(|r| RouteRule {
                    path_match: r.path_match.as_ref().map(|pm| match pm {
                        PathMatch::Prefix(s) => PathMatch::Prefix(s.clone()),
                        PathMatch::Exact(s) => PathMatch::Exact(s.clone()),
                        PathMatch::Regex(re) => {
                            PathMatch::Regex(regex::Regex::new(re.as_str()).unwrap())
                        }
                    }),
                    header_matches: r
                        .header_matches
                        .iter()
                        .map(|hm| HeaderMatch {
                            name: hm.name.clone(),
                            match_type: hm.match_type.clone_match(),
                        })
                        .collect(),
                    query_param_matches: r
                        .query_param_matches
                        .iter()
                        .map(|qm| QueryMatch {
                            name: qm.name.clone(),
                            match_type: qm.match_type.clone_match(),
                        })
                        .collect(),
                    method_match: r.method_match.clone(),
                    backends: r.backends.clone(),
                })
                .collect(),
        }
    }
}

impl StringMatch {
    fn clone_match(&self) -> Self {
        match self {
            Self::Exact(s) => Self::Exact(s.clone()),
            Self::Regex(re) => Self::Regex(regex::Regex::new(re.as_str()).unwrap()),
        }
    }
}

/// Check if an `HTTPRoute` references our gateway in `parentRefs`.
fn matches_parent_ref(obj: &DynamicObject, gateway_name: &str, gateway_namespace: &str) -> bool {
    let parent_refs = obj
        .data
        .get("spec")
        .and_then(|s| s.get("parentRefs"))
        .and_then(|p| p.as_array());

    let Some(refs) = parent_refs else {
        return false;
    };

    refs.iter().any(|pr| {
        let name = pr.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let ns = pr.get("namespace").and_then(|n| n.as_str()).unwrap_or("");
        name == gateway_name && ns == gateway_namespace
    })
}

/// Extract hostnames from an `HTTPRoute` spec.
fn extract_hostnames(obj: &DynamicObject) -> Vec<String> {
    obj.data
        .get("spec")
        .and_then(|s| s.get("hostnames"))
        .and_then(|h| h.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Extract rules from an `HTTPRoute` spec.
fn extract_rules(
    obj: &DynamicObject,
    endpoint_cache: &HashMap<String, Vec<SocketAddr>>,
) -> Vec<RouteRule> {
    let route_ns = obj.metadata.namespace.as_deref().unwrap_or("default");

    let rules = obj
        .data
        .get("spec")
        .and_then(|s| s.get("rules"))
        .and_then(|r| r.as_array());

    let Some(rules) = rules else {
        return Vec::new();
    };

    rules
        .iter()
        .map(|rule| parse_rule(rule, route_ns, endpoint_cache))
        .collect()
}

/// Parse a single `HTTPRoute` rule from JSON.
fn parse_rule(
    rule: &serde_json::Value,
    route_ns: &str,
    endpoint_cache: &HashMap<String, Vec<SocketAddr>>,
) -> RouteRule {
    // Parse matches (take first match block — simplified)
    let matches = rule.get("matches").and_then(|m| m.as_array());
    let first_match = matches.and_then(|arr| arr.first());

    let path_match = first_match
        .and_then(|m| m.get("path"))
        .and_then(parse_path_match);

    let header_matches = first_match
        .and_then(|m| m.get("headers"))
        .and_then(|h| h.as_array())
        .map(|arr| arr.iter().filter_map(parse_header_match).collect())
        .unwrap_or_default();

    let query_param_matches = first_match
        .and_then(|m| m.get("queryParams"))
        .and_then(|q| q.as_array())
        .map(|arr| arr.iter().filter_map(parse_query_match).collect())
        .unwrap_or_default();

    let method_match = first_match
        .and_then(|m| m.get("method"))
        .and_then(|m| m.as_str())
        .map(ToString::to_string);

    // Parse backends
    let backends = rule
        .get("backendRefs")
        .and_then(|b| b.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|b| parse_backend(b, route_ns, endpoint_cache))
                .collect()
        })
        .unwrap_or_default();

    RouteRule {
        path_match,
        header_matches,
        query_param_matches,
        method_match,
        backends,
    }
}

/// Parse a path match from the `HTTPRoute` JSON.
pub fn parse_path_match(value: &serde_json::Value) -> Option<PathMatch> {
    let match_type = value
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("PathPrefix");
    let path_value = value.get("value").and_then(|v| v.as_str()).unwrap_or("/");

    match match_type {
        "PathPrefix" => Some(PathMatch::Prefix(path_value.to_string())),
        "Exact" => Some(PathMatch::Exact(path_value.to_string())),
        "RegularExpression" => regex::Regex::new(path_value).ok().map(PathMatch::Regex),
        _ => {
            tracing::warn!(match_type, "unknown path match type");
            None
        }
    }
}

/// Parse a header match from the `HTTPRoute` JSON.
fn parse_header_match(value: &serde_json::Value) -> Option<HeaderMatch> {
    let name = value.get("name").and_then(|n| n.as_str())?.to_string();
    let match_type = value
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("Exact");
    let match_value = value.get("value").and_then(|v| v.as_str())?;

    let string_match = match match_type {
        "Exact" => StringMatch::Exact(match_value.to_string()),
        "RegularExpression" => regex::Regex::new(match_value)
            .ok()
            .map(StringMatch::Regex)?,
        _ => {
            tracing::warn!(match_type, "unknown header match type");
            return None;
        }
    };

    Some(HeaderMatch {
        name,
        match_type: string_match,
    })
}

/// Parse a query parameter match from the `HTTPRoute` JSON.
fn parse_query_match(value: &serde_json::Value) -> Option<QueryMatch> {
    let name = value.get("name").and_then(|n| n.as_str())?.to_string();
    let match_type = value
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("Exact");
    let match_value = value.get("value").and_then(|v| v.as_str())?;

    let string_match = match match_type {
        "Exact" => StringMatch::Exact(match_value.to_string()),
        "RegularExpression" => regex::Regex::new(match_value)
            .ok()
            .map(StringMatch::Regex)?,
        _ => {
            tracing::warn!(match_type, "unknown query param match type");
            return None;
        }
    };

    Some(QueryMatch {
        name,
        match_type: string_match,
    })
}

/// Parse a backend ref from the `HTTPRoute` JSON.
fn parse_backend(
    value: &serde_json::Value,
    route_ns: &str,
    endpoint_cache: &HashMap<String, Vec<SocketAddr>>,
) -> Option<WeightedBackend> {
    let service = value.get("name").and_then(|n| n.as_str())?.to_string();
    let namespace = value
        .get("namespace")
        .and_then(|n| n.as_str())
        .unwrap_or(route_ns)
        .to_string();
    #[allow(clippy::cast_possible_truncation)]
    let port = value
        .get("port")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(80) as u16;
    #[allow(clippy::cast_possible_truncation)]
    let weight = value
        .get("weight")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1) as u32;

    // Resolve endpoints from cache
    let svc_key = format!("{namespace}/{service}");
    let endpoints = endpoint_cache.get(&svc_key).cloned().unwrap_or_default();

    Some(WeightedBackend {
        service,
        namespace,
        port,
        weight,
        endpoints,
    })
}

/// Update the shared rate limit config map from all known `HTTPRoute` objects.
///
/// For each route that has `platform.io/rate-limit` annotations, stores
/// the parsed config keyed by route name (`metadata.name`).
fn update_rate_limit_configs(
    routes: &HashMap<String, DynamicObject>,
    configs: &SharedRateLimitConfigs,
    gateway_config: &GatewayConfig,
) {
    configs.clear();
    for obj in routes.values() {
        if !matches_parent_ref(
            obj,
            &gateway_config.gateway_name,
            &gateway_config.gateway_namespace,
        ) {
            continue;
        }
        let name = obj.metadata.name.as_deref().unwrap_or("unknown");
        if let Some(annotations) = &obj.metadata.annotations
            && let Some(rl_config) = super::rate_limit::parse_annotations(annotations)
        {
            configs.insert(name.to_string(), rl_config);
        }
    }
    tracing::debug!(
        rate_limited_routes = configs.len(),
        "updated rate limit configurations"
    );
}

/// Extract annotations from an `HTTPRoute` for external use.
/// Returns the annotations map if present.
pub fn extract_route_annotations(
    obj: &DynamicObject,
) -> Option<&std::collections::BTreeMap<String, String>> {
    obj.metadata.annotations.as_ref()
}

/// Parse a full `HTTPRoute` JSON into route entries for the routing table.
/// Exposed for unit testing.
pub fn parse_httproute(
    json: &serde_json::Value,
    gateway_name: &str,
    gateway_namespace: &str,
) -> Option<(Vec<String>, RouteEntry)> {
    // Build a minimal DynamicObject from the JSON
    let obj = serde_json::from_value::<DynamicObject>(json.clone()).ok()?;

    if !matches_parent_ref(&obj, gateway_name, gateway_namespace) {
        return None;
    }

    let source = RouteSource {
        name: obj.metadata.name.clone().unwrap_or_default(),
        namespace: obj.metadata.namespace.clone().unwrap_or_default(),
    };

    let hostnames = extract_hostnames(&obj);
    let empty_cache = HashMap::new();
    let rules = extract_rules(&obj, &empty_cache);

    Some((hostnames, RouteEntry { source, rules }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_httproute() -> serde_json::Value {
        json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "metadata": {
                "name": "my-route",
                "namespace": "myapp-prod"
            },
            "spec": {
                "parentRefs": [{
                    "name": "platform-gateway",
                    "namespace": "platform"
                }],
                "hostnames": ["api.example.com"],
                "rules": [{
                    "matches": [{
                        "path": {
                            "type": "PathPrefix",
                            "value": "/api"
                        }
                    }],
                    "backendRefs": [{
                        "name": "api-svc",
                        "port": 8080,
                        "weight": 100
                    }]
                }]
            }
        })
    }

    #[test]
    fn parse_httproute_basic() {
        let route_json = sample_httproute();
        let result = parse_httproute(&route_json, "platform-gateway", "platform");
        assert!(result.is_some());

        let (hostnames, entry) = result.unwrap();
        assert_eq!(hostnames, vec!["api.example.com"]);
        assert_eq!(entry.source.name, "my-route");
        assert_eq!(entry.source.namespace, "myapp-prod");
        assert_eq!(entry.rules.len(), 1);

        let rule = &entry.rules[0];
        assert!(rule.path_match.is_some());
        assert_eq!(rule.backends.len(), 1);
        assert_eq!(rule.backends[0].service, "api-svc");
        assert_eq!(rule.backends[0].port, 8080);
        assert_eq!(rule.backends[0].weight, 100);
    }

    #[test]
    fn parse_httproute_multiple_rules() {
        let route_json = json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "metadata": {
                "name": "multi-rule",
                "namespace": "ns"
            },
            "spec": {
                "parentRefs": [{"name": "platform-gateway", "namespace": "platform"}],
                "hostnames": ["app.example.com"],
                "rules": [
                    {
                        "matches": [{"path": {"type": "Exact", "value": "/healthz"}}],
                        "backendRefs": [{"name": "health-svc", "port": 8080}]
                    },
                    {
                        "matches": [{"path": {"type": "PathPrefix", "value": "/"}}],
                        "backendRefs": [{"name": "app-svc", "port": 8080}]
                    }
                ]
            }
        });

        let (hostnames, entry) =
            parse_httproute(&route_json, "platform-gateway", "platform").unwrap();
        assert_eq!(hostnames, vec!["app.example.com"]);
        assert_eq!(entry.rules.len(), 2);
    }

    #[test]
    fn parse_httproute_weighted_backends() {
        let route_json = json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "metadata": {
                "name": "canary-route",
                "namespace": "ns"
            },
            "spec": {
                "parentRefs": [{"name": "platform-gateway", "namespace": "platform"}],
                "rules": [{
                    "backendRefs": [
                        {"name": "stable-svc", "port": 8080, "weight": 90},
                        {"name": "canary-svc", "port": 8080, "weight": 10}
                    ]
                }]
            }
        });

        let (hostnames, entry) =
            parse_httproute(&route_json, "platform-gateway", "platform").unwrap();
        assert!(hostnames.is_empty()); // no hostnames = wildcard
        assert_eq!(entry.rules[0].backends.len(), 2);
        assert_eq!(entry.rules[0].backends[0].service, "stable-svc");
        assert_eq!(entry.rules[0].backends[0].weight, 90);
        assert_eq!(entry.rules[0].backends[1].service, "canary-svc");
        assert_eq!(entry.rules[0].backends[1].weight, 10);
    }

    #[test]
    fn filter_by_parent_ref_matching() {
        let route_json = sample_httproute();
        let result = parse_httproute(&route_json, "platform-gateway", "platform");
        assert!(result.is_some());
    }

    #[test]
    fn filter_by_parent_ref_wrong_name() {
        let route_json = sample_httproute();
        let result = parse_httproute(&route_json, "other-gateway", "platform");
        assert!(result.is_none());
    }

    #[test]
    fn filter_by_parent_ref_wrong_namespace() {
        let route_json = sample_httproute();
        let result = parse_httproute(&route_json, "platform-gateway", "other-ns");
        assert!(result.is_none());
    }

    #[test]
    fn parse_httproute_with_header_match() {
        let route_json = json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "metadata": { "name": "header-route", "namespace": "ns" },
            "spec": {
                "parentRefs": [{"name": "platform-gateway", "namespace": "platform"}],
                "rules": [{
                    "matches": [{
                        "headers": [{
                            "type": "Exact",
                            "name": "x-version",
                            "value": "v2"
                        }]
                    }],
                    "backendRefs": [{"name": "v2-svc", "port": 8080}]
                }]
            }
        });

        let (_, entry) = parse_httproute(&route_json, "platform-gateway", "platform").unwrap();
        assert_eq!(entry.rules[0].header_matches.len(), 1);
        assert_eq!(entry.rules[0].header_matches[0].name, "x-version");
    }

    #[test]
    fn parse_httproute_with_query_param_match() {
        let route_json = json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "metadata": { "name": "query-route", "namespace": "ns" },
            "spec": {
                "parentRefs": [{"name": "platform-gateway", "namespace": "platform"}],
                "rules": [{
                    "matches": [{
                        "queryParams": [{
                            "type": "Exact",
                            "name": "version",
                            "value": "2"
                        }]
                    }],
                    "backendRefs": [{"name": "v2-svc", "port": 8080}]
                }]
            }
        });

        let (_, entry) = parse_httproute(&route_json, "platform-gateway", "platform").unwrap();
        assert_eq!(entry.rules[0].query_param_matches.len(), 1);
        assert_eq!(entry.rules[0].query_param_matches[0].name, "version");
    }

    #[test]
    fn parse_httproute_with_method_match() {
        let route_json = json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "metadata": { "name": "method-route", "namespace": "ns" },
            "spec": {
                "parentRefs": [{"name": "platform-gateway", "namespace": "platform"}],
                "rules": [{
                    "matches": [{
                        "method": "POST",
                        "path": {"type": "PathPrefix", "value": "/submit"}
                    }],
                    "backendRefs": [{"name": "submit-svc", "port": 8080}]
                }]
            }
        });

        let (_, entry) = parse_httproute(&route_json, "platform-gateway", "platform").unwrap();
        assert_eq!(entry.rules[0].method_match, Some("POST".into()));
    }

    #[test]
    fn parse_httproute_no_parent_refs() {
        let route_json = json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "metadata": { "name": "orphan", "namespace": "ns" },
            "spec": {
                "rules": [{
                    "backendRefs": [{"name": "svc", "port": 8080}]
                }]
            }
        });

        let result = parse_httproute(&route_json, "platform-gateway", "platform");
        assert!(result.is_none());
    }

    #[test]
    fn parse_endpoint_slice_basic() {
        let es_json = json!({
            "apiVersion": "discovery.k8s.io/v1",
            "kind": "EndpointSlice",
            "metadata": {
                "name": "my-svc-abc12",
                "namespace": "default",
                "labels": {
                    "kubernetes.io/service-name": "my-svc"
                }
            },
            "endpoints": [
                {
                    "addresses": ["10.0.0.1"],
                    "conditions": { "ready": true }
                },
                {
                    "addresses": ["10.0.0.2"],
                    "conditions": { "ready": false }
                }
            ],
            "ports": [
                { "port": 8080, "protocol": "TCP" }
            ]
        });

        let obj: DynamicObject = serde_json::from_value(es_json).unwrap();
        let result = parse_endpoint_slice(&obj);
        assert!(result.is_some());

        let (svc_key, addrs) = result.unwrap();
        assert_eq!(svc_key, "default/my-svc");
        // Only the ready endpoint should be included
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0], "10.0.0.1:8080".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_endpoint_slice_no_service_label() {
        let es_json = json!({
            "apiVersion": "discovery.k8s.io/v1",
            "kind": "EndpointSlice",
            "metadata": {
                "name": "orphan-es",
                "namespace": "default",
                "labels": {}
            },
            "endpoints": [
                { "addresses": ["10.0.0.1"], "conditions": { "ready": true } }
            ],
            "ports": [{ "port": 8080 }]
        });

        let obj: DynamicObject = serde_json::from_value(es_json).unwrap();
        assert!(parse_endpoint_slice(&obj).is_none());
    }

    #[test]
    fn parse_httproute_regex_path() {
        let route_json = json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "metadata": { "name": "regex-route", "namespace": "ns" },
            "spec": {
                "parentRefs": [{"name": "platform-gateway", "namespace": "platform"}],
                "rules": [{
                    "matches": [{
                        "path": {
                            "type": "RegularExpression",
                            "value": "^/users/\\d+$"
                        }
                    }],
                    "backendRefs": [{"name": "users-svc", "port": 8080}]
                }]
            }
        });

        let (_, entry) = parse_httproute(&route_json, "platform-gateway", "platform").unwrap();
        assert!(entry.rules[0].path_match.is_some());
    }

    #[test]
    fn matches_parent_ref_checks_both_name_and_namespace() {
        let obj_json = json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "metadata": { "name": "test", "namespace": "ns" },
            "spec": {
                "parentRefs": [
                    {"name": "gateway-a", "namespace": "ns-a"},
                    {"name": "gateway-b", "namespace": "ns-b"}
                ]
            }
        });
        let obj: DynamicObject = serde_json::from_value(obj_json).unwrap();

        assert!(matches_parent_ref(&obj, "gateway-a", "ns-a"));
        assert!(matches_parent_ref(&obj, "gateway-b", "ns-b"));
        assert!(!matches_parent_ref(&obj, "gateway-a", "ns-b"));
        assert!(!matches_parent_ref(&obj, "gateway-c", "ns-a"));
    }
}
