//! The data-plane listener config (`[listen]`).

use serde::{Deserialize, Serialize};

/// The data-plane listener config (`[listen]`). The manifest is the single static source of
/// config, so the bind address lives here rather than only in a positional CLI arg (containers
/// need `0.0.0.0` binds without entrypoint gymnastics); an explicit CLI `listen_addr` still wins
/// as the operator's override.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Listen {
    /// `host:port` the data plane binds (e.g. `0.0.0.0:8443`). `None` = the binary's default
    /// (`127.0.0.1:8080`) unless the CLI arg overrides.
    #[serde(default)]
    pub addr: Option<String>,
    /// The port `Alt-Svc` advertises for HTTP/3 (RFC 7838), when the PUBLISHED port differs from
    /// the bound one (container port mapping: internal 8443 → published 443 would otherwise
    /// advertise a dead h3 port). `None` = advertise the bound port (the default).
    #[serde(default)]
    pub advertised_port: Option<u16>,
}
