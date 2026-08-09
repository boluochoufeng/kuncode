//! Dial-address vetting that keeps `web_fetch` off the internal network.
//!
//! A fetched page is untrusted text that can end up steering the agent, so the
//! URL it names is treated as attacker-controlled: the interesting target is not
//! the public web but whatever the *host* can reach — cloud instance metadata at
//! `169.254.169.254`, an RFC 1918 admin panel, a sidecar on the pod network.
//! [`is_blocked`] answers which addresses are off limits, and
//! [`GuardedResolver`] applies it to the address reqwest is about to dial rather
//! than to the hostname, so a public name that resolves (or re-resolves) inward
//! is refused too.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Reports whether `ip` names an address `web_fetch` must never dial.
///
/// Loopback is deliberately reachable. A local dev server is a normal fetch
/// target, and refusing it would buy nothing: everything on loopback is already
/// reachable through `bash`, under the same approval this tool requires.
pub(super) fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => is_blocked_v4(address),
        // An IPv6 address that carries an IPv4 one is decided by the address it
        // carries, so `::ffff:169.254.169.254` cannot smuggle a blocked target
        // past the v4 rules. Loopback is settled first because `::1` is also a
        // (deprecated) IPv4-compatible form of `0.0.0.1`.
        IpAddr::V6(address) => {
            !address.is_loopback()
                && match embedded_ipv4(address) {
                    Some(carried) => is_blocked_v4(carried),
                    None => is_blocked_v6(address),
                }
        }
    }
}

fn is_blocked_v4(address: Ipv4Addr) -> bool {
    // The two ranges below have no stable predicate in `std` (`is_shared` is
    // unstable), so they are spelled out against the leading octets.
    let [first, second, ..] = address.octets();
    address.is_private()
        || address.is_link_local() // 169.254.0.0/16, including cloud metadata
        || address.is_multicast()
        || address.is_broadcast()
        // 0.0.0.0/8 is "this network": unroutable, and on Linux 0.0.0.0 reaches
        // the local host, so it is a spelling of loopback rather than a target.
        || first == 0
        // RFC 6598 shared address space. Not private per RFC 1918, yet clouds
        // put instance metadata there (Alibaba Cloud at 100.100.100.200).
        || (first == 100 && (64..128).contains(&second))
}

fn is_blocked_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    address.is_unspecified()
        || address.is_multicast()
        || segments[0] & 0xffc0 == 0xfe80 // fe80::/10 link-local unicast
        || segments[0] & 0xfe00 == 0xfc00 // fc00::/7 unique local
}

/// Extracts the IPv4 address an IPv6 address carries, for the transition schemes
/// whose payload a dual-stack host will actually route to.
fn embedded_ipv4(address: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return Some(mapped);
    }
    let segments = address.segments();
    let trailing = || Ipv4Addr::from((u32::from(segments[6]) << 16) | u32::from(segments[7]));
    match segments {
        // NAT64 well-known prefix (RFC 6052) and the deprecated IPv4-compatible
        // form both keep the address in the low 32 bits.
        [0x0064, 0xff9b, 0, 0, 0, 0, _, _] => Some(trailing()),
        [0, 0, 0, 0, 0, 0, _, _] => Some(trailing()),
        // 6to4 (RFC 3056) keeps it in the 32 bits after the prefix.
        [0x2002, high, low, ..] => Some(Ipv4Addr::from((u32::from(high) << 16) | u32::from(low))),
        _ => None,
    }
}

/// Resolver that refuses to hand reqwest an address [`is_blocked`] rejects.
///
/// Vetting the resolved address rather than the hostname is what makes the guard
/// hold under a redirect chain or a DNS record that answers publicly once and
/// privately next: reqwest dials exactly the addresses returned here, so there
/// is no second lookup to rebind.
pub(super) struct GuardedResolver;

impl GuardedResolver {
    /// Builds the resolver used by the direct-only `web_fetch` client.
    pub(super) fn new() -> Self {
        Self
    }
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_ascii_lowercase();
        Box::pin(async move {
            // Port 0: reqwest substitutes the URL's port, or the scheme default.
            let resolved = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .collect::<Vec<SocketAddr>>();
            if let Some(blocked) = resolved.iter().find(|address| is_blocked(address.ip())) {
                // One blocked answer fails the whole lookup instead of being
                // filtered out, so a record that mixes a public address with an
                // internal one cannot get the internal one dialed on a retry.
                return Err(format!(
                    "refusing to fetch internal address {} (`{host}` resolves to it)",
                    blocked.ip()
                )
                .into());
            }
            Ok(Box::new(resolved.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(text: &str) -> bool {
        is_blocked(text.parse().expect("test address parses"))
    }

    #[test]
    fn internal_ranges_are_refused() {
        for address in [
            "169.254.169.254",          // AWS/GCP/Azure instance metadata
            "10.1.2.3",                 // RFC 1918
            "172.16.5.6",               // RFC 1918
            "192.168.1.1",              // RFC 1918
            "0.0.0.0",                  // this network
            "255.255.255.255",          // broadcast
            "224.0.0.1",                // multicast
            "100.100.100.200",          // Alibaba Cloud metadata (RFC 6598)
            "100.64.0.1",               // RFC 6598 shared space
            "fe80::1",                  // IPv6 link-local
            "fc00::1",                  // IPv6 unique local
            "ff02::1",                  // IPv6 multicast
            "::",                       // unspecified
            "::ffff:10.0.0.1",          // IPv4-mapped private
            "::ffff:100.100.100.1",     // IPv4-mapped shared space
            "64:ff9b::169.254.169.254", // NAT64-embedded metadata
            "2002:a00:1::",             // 6to4-embedded 10.0.0.1
        ] {
            assert!(blocked(address), "{address} should be refused");
        }
    }

    #[test]
    fn public_and_loopback_addresses_are_reachable() {
        // Loopback stays fetchable on purpose: a local dev server is a normal
        // target, and `100.127.x` / `99.x` sit just outside the shared range.
        for address in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "127.0.0.1",
            "::1",
            "2606:4700::1111",
            "99.255.255.255",
            "100.128.0.1",
        ] {
            assert!(!blocked(address), "{address} should be reachable");
        }
    }
}
