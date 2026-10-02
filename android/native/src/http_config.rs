//! Validate listener policy before opening USB or starting a worker.
use std::net::{Ipv4Addr, SocketAddr};

pub struct HttpConfig {
    pub bind: SocketAddr,
    pub token: Option<String>,
}
impl HttpConfig {
    pub fn new(lan: bool, port: i32, token: &str) -> Result<Self, &'static str> {
        let port = u16::try_from(port)
            .ok()
            .filter(|p| *p != 0)
            .ok_or("port must be 1..65535")?;
        // The app generates 32 random bytes and stores their hex encoding.
        if lan && (token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit())) {
            return Err("LAN access requires a generated 64-character token");
        }
        Ok(Self {
            bind: SocketAddr::from((
                if lan {
                    Ipv4Addr::UNSPECIFIED
                } else {
                    Ipv4Addr::LOCALHOST
                },
                port,
            )),
            token: lan.then(|| token.to_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loopback_stays_local_without_authentication() {
        let config = HttpConfig::new(false, 38473, "").unwrap();
        assert!(config.bind.ip().is_loopback());
        assert_eq!(config.bind.port(), 38473);
        assert!(config.token.is_none());
    }
    #[test]
    fn lan_requires_a_header_safe_token_and_keeps_it() {
        for token in [
            "".to_owned(),
            "a".repeat(63),
            "a".repeat(65),
            "\n".repeat(64),
            "x".repeat(64),
        ] {
            assert!(HttpConfig::new(true, 38473, &token).is_err());
        }
        let token = "ab".repeat(32);
        let config = HttpConfig::new(true, 45678, &token).unwrap();
        assert!(config.bind.ip().is_unspecified());
        assert_eq!(config.bind.port(), 45678);
        assert_eq!(config.token.as_deref(), Some(token.as_str()));
    }
    #[test]
    fn invalid_ports_never_bind_an_ephemeral_or_truncated_port() {
        for port in [-1, 0, 65536, i32::MAX] {
            assert!(HttpConfig::new(false, port, "").is_err());
        }
    }
}
