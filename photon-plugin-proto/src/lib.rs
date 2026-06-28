//! Shared gRPC contract between the Photon host and its subprocess plugins
//! (`go-plugin` style). Both the host (client stubs) and the SDK (server stubs)
//! depend on this crate so the wire types have a single source of truth.

/// Generated tonic/prost code for `package photon.plugin.v1`.
pub mod pb {
    tonic::include_proto!("photon.plugin.v1");
}

/// Bumped whenever the wire contract changes incompatibly. The host rejects a
/// plugin whose declared `protocol_version` doesn't match.
pub const PROTOCOL_VERSION: u32 = 1;

/// Magic-cookie handshake (go-plugin convention): the host sets this env var when
/// launching a plugin; the SDK refuses to start without the matching value, so a
/// plugin binary can't be run by accident outside the host.
pub const MAGIC_COOKIE_KEY: &str = "PHOTON_PLUGIN_COOKIE";
pub const MAGIC_COOKIE_VALUE: &str = "photon-plugin-v1-7c3f";

/// Env var carrying the Unix socket path the plugin must bind + advertise.
pub const SOCKET_ENV: &str = "PHOTON_PLUGIN_SOCKET";

/// Env vars the host injects at launch so a plugin can call BACK into the Photon
/// HTTP API as a trusted service account: the API base URL and a bearer token.
/// The SDK's `PhotonClient::from_env` reads these. Absent ⇒ no callbacks (the
/// plugin still runs; API calls just aren't available).
pub const API_URL_ENV: &str = "PHOTON_API_URL";
pub const API_TOKEN_ENV: &str = "PHOTON_API_TOKEN";

/// Format the single handshake line a plugin prints to stdout after binding its
/// socket: `CORE|APP|unix|<addr>|grpc` (go-plugin layout). The host reads exactly
/// one line, parses the address, and dials the Unix socket.
pub fn handshake_line(socket_path: &str) -> String {
    format!("1|{PROTOCOL_VERSION}|unix|{socket_path}|grpc")
}

/// Parse a handshake line into the advertised Unix socket address, validating the
/// app protocol version. Returns `None` on any malformed/incompatible line.
pub fn parse_handshake(line: &str) -> Option<String> {
    let parts: Vec<&str> = line.trim().split('|').collect();
    // CORE | APP | network | address | protocol
    if parts.len() < 5 {
        return None;
    }
    let app: u32 = parts[1].parse().ok()?;
    if app != PROTOCOL_VERSION || parts[2] != "unix" || parts[4] != "grpc" {
        return None;
    }
    Some(parts[3].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_roundtrips() {
        let line = handshake_line("/tmp/x.sock");
        assert_eq!(parse_handshake(&line).as_deref(), Some("/tmp/x.sock"));
    }

    #[test]
    fn handshake_rejects_bad_lines() {
        assert!(parse_handshake("garbage").is_none());
        assert!(parse_handshake("1|999|unix|/x|grpc").is_none()); // wrong protocol
        assert!(parse_handshake("1|1|tcp|/x|grpc").is_none()); // wrong network
    }
}
