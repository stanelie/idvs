use std::net::Ipv4Addr;

#[derive(Clone, Debug)]
pub struct NetworkInterface {
    pub name: String,
    pub ip: Option<Ipv4Addr>,
}

impl std::fmt::Display for NetworkInterface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.ip {
            Some(ip) => write!(f, "{} ({})", self.name, ip),
            None => write!(f, "{} (no IPv4)", self.name),
        }
    }
}

/// List all non-loopback network interfaces with their IPv4 addresses
pub fn list_interfaces() -> Vec<NetworkInterface> {
    let mut by_name: std::collections::BTreeMap<String, Option<Ipv4Addr>> =
        std::collections::BTreeMap::new();

    if let Ok(addrs) = nix::ifaddrs::getifaddrs() {
        for ifaddr in addrs {
            let name = ifaddr.interface_name.clone();
            if name == "lo" {
                continue;
            }

            let ip = ifaddr.address.and_then(|addr| {
                addr.as_sockaddr_in()
                    .map(|sin| Ipv4Addr::from(sin.ip()))
            });

            let entry = by_name.entry(name).or_insert(None);
            if ip.is_some() {
                *entry = ip;
            }
        }
    }

    by_name
        .into_iter()
        .map(|(name, ip)| NetworkInterface { name, ip })
        .collect()
}

/// Get the IPv4 address of a named interface
pub fn interface_ip(name: &str) -> Option<Ipv4Addr> {
    if let Ok(addrs) = nix::ifaddrs::getifaddrs() {
        for ifaddr in addrs {
            if ifaddr.interface_name == name {
                if let Some(addr) = ifaddr.address {
                    if let Some(sin) = addr.as_sockaddr_in() {
                        return Some(Ipv4Addr::from(sin.ip()));
                    }
                }
            }
        }
    }
    None
}
