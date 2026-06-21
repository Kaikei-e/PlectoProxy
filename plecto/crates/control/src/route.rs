//! Host-native routing (ADR 000013): match a request to a route by host + path prefix, then
//! the fast-path server runs that route's chain and forwards to its upstream. Pure config logic
//! — no I/O, no wasmtime — so it is unit-tested directly and runs on the async thread (the
//! blocking work is the chain dispatch, not the match).

/// A route compiled from a manifest [`crate::Route`] into the live config: the upstream name is
/// resolved to its address and the host is pre-normalised, so matching is allocation-free.
#[derive(Debug, Clone)]
pub(crate) struct CompiledRoute {
    /// Lower-cased authority to match, or `None` for any host.
    pub(crate) host: Option<String>,
    pub(crate) path_prefix: String,
    /// This route's inline chain (filter ids, in order).
    pub(crate) filters: Vec<String>,
    /// Resolved `host:port` of the upstream this route forwards to.
    pub(crate) upstream_address: String,
    pub(crate) strip_prefix: Option<String>,
}

/// What [`crate::ConfigSnapshot::find_route`] hands the fast-path server: which route matched
/// (`index`, used to dispatch its chain) plus the data needed to forward — the upstream address
/// and the optional prefix strip. Owned so it survives a move into `spawn_blocking`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteInfo {
    pub index: usize,
    pub upstream_address: String,
    pub strip_prefix: Option<String>,
}

impl RouteInfo {
    /// Apply this route's host-native prefix strip to the path the fast-path server forwards to
    /// the upstream. The chain already ran against the original path; this only affects what the
    /// upstream sees. No rule (or a non-matching path) leaves the path unchanged.
    pub fn rewrite_path(&self, path: &str) -> String {
        rewrite_path(path, self.strip_prefix.as_deref())
    }
}

/// Normalise an authority for host matching: drop any `:port`, lower-case. (`example.COM:8443`
/// → `example.com`.) IPv6 literals in brackets keep their colons inside `[...]`.
fn normalize_host(authority: &str) -> String {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        // `[::1]:8080` → `[::1]`
        match rest.split_once(']') {
            Some((inner, _)) => &authority[..inner.len() + 2],
            None => authority,
        }
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    host.to_ascii_lowercase()
}

/// Does `path` fall under `prefix` on a `/` boundary? `/api` matches `/api` and `/api/x` but not
/// `/apix`; `/` matches everything.
fn path_under_prefix(prefix: &str, path: &str) -> bool {
    if !path.starts_with(prefix) {
        return false;
    }
    if path.len() == prefix.len() || prefix.ends_with('/') {
        return true;
    }
    path.as_bytes().get(prefix.len()) == Some(&b'/')
}

/// Select the best route for `(authority, path)`: among all that match host + path prefix, the
/// longest prefix wins; ties prefer a host-constrained route over a wildcard one, then the
/// earliest in manifest order. Returns the winner's index, or `None` (no route → the server
/// responds 404).
pub(crate) fn select(routes: &[CompiledRoute], authority: &str, path: &str) -> Option<usize> {
    let host = normalize_host(authority);
    routes
        .iter()
        .enumerate()
        .filter(|(_, r)| match &r.host {
            Some(h) => *h == host,
            None => true,
        })
        .filter(|(_, r)| path_under_prefix(&r.path_prefix, path))
        // best = longest prefix, then host-constrained, then earliest index. `max_by_key` keeps
        // the LAST max on ties, so negate the index to prefer the earliest.
        .max_by_key(|(i, r)| (r.path_prefix.len(), r.host.is_some(), usize::MAX - i))
        .map(|(i, _)| i)
}

/// Apply a route's host-native prefix strip to the forwarded path (the chain already saw the
/// original). Leaves the path unchanged if it does not start with `strip`; always keeps a
/// leading `/`. `/api` stripped from `/api/users` → `/users`; from `/api` → `/`.
pub(crate) fn rewrite_path(path: &str, strip: Option<&str>) -> String {
    let Some(strip) = strip else {
        return path.to_string();
    };
    let Some(rest) = path.strip_prefix(strip) else {
        return path.to_string();
    };
    if rest.is_empty() {
        "/".to_string()
    } else if rest.starts_with('/') {
        rest.to_string()
    } else {
        format!("/{rest}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(host: Option<&str>, prefix: &str, upstream: &str) -> CompiledRoute {
        CompiledRoute {
            host: host.map(|h| h.to_ascii_lowercase()),
            path_prefix: prefix.to_string(),
            filters: vec![],
            upstream_address: upstream.to_string(),
            strip_prefix: None,
        }
    }

    #[test]
    fn longest_prefix_wins() {
        let routes = vec![
            route(None, "/", "root"),
            route(None, "/api", "api"),
            route(None, "/api/v2", "v2"),
        ];
        assert_eq!(select(&routes, "h", "/api/v2/x"), Some(2));
        assert_eq!(select(&routes, "h", "/api/users"), Some(1));
        assert_eq!(select(&routes, "h", "/other"), Some(0));
    }

    #[test]
    fn prefix_matches_on_boundary_only() {
        let routes = vec![route(None, "/api", "api")];
        assert_eq!(select(&routes, "h", "/api"), Some(0));
        assert_eq!(select(&routes, "h", "/api/x"), Some(0));
        assert_eq!(select(&routes, "h", "/apix"), None, "no boundary match");
    }

    #[test]
    fn host_constraint_filters_and_breaks_ties() {
        let routes = vec![
            route(None, "/api", "wild"),
            route(Some("example.com"), "/api", "vhost"),
        ];
        // request to example.com (with a port) prefers the host-constrained route
        assert_eq!(select(&routes, "example.com:8443", "/api/x"), Some(1));
        // a different host falls back to the wildcard
        assert_eq!(select(&routes, "other.test", "/api/x"), Some(0));
    }

    #[test]
    fn no_match_returns_none() {
        let routes = vec![route(Some("a.test"), "/api", "u")];
        assert_eq!(select(&routes, "b.test", "/api"), None);
        let empty: Vec<CompiledRoute> = vec![];
        assert_eq!(select(&empty, "a", "/"), None);
    }

    #[test]
    fn strip_prefix_keeps_leading_slash() {
        assert_eq!(rewrite_path("/api/users", Some("/api")), "/users");
        assert_eq!(rewrite_path("/api", Some("/api")), "/");
        assert_eq!(rewrite_path("/api/", Some("/api")), "/");
        assert_eq!(rewrite_path("/other", Some("/api")), "/other", "no strip");
        assert_eq!(rewrite_path("/api/x", None), "/api/x", "no rule");
    }
}
