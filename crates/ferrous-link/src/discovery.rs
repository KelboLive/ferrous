//! LAN discovery: senders multicast a small JSON beacon every second;
//! receivers keep a list of what they've heard recently.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::protocol::CodecKind;

pub const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 77, 77);
pub const PORT: u16 = 47777;

type Ifaces = Vec<(Ipv4Addr, Option<Ipv4Addr>)>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Announcement {
    pub ember_link: u32,
    pub id: String,
    pub name: String,
    pub port: u16,
    pub codec: CodecKind,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub encrypted: bool,
    /// Filled in by the receiver from the packet's source address.
    #[serde(default)]
    pub host: String,
}

impl Announcement {
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// This PC's IPv4 interfaces worth announcing on: (address, subnet
/// broadcast). Loopback and unconfigured (169.254.x.x) adapters are left
/// out. Re-read at most every 10 s, so adapters that come and go (VPN,
/// Wi-Fi) are picked up.
fn interfaces() -> Ifaces {
    static CACHE: OnceLock<Mutex<(Instant, Ifaces)>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new((Instant::now() - Duration::from_secs(60), Vec::new())));
    let mut c = cache.lock();
    if c.0.elapsed() >= Duration::from_secs(10) {
        c.1 = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|i| match i.addr {
                if_addrs::IfAddr::V4(v4) if !v4.ip.is_loopback() && !v4.ip.is_link_local() => Some((v4.ip, v4.broadcast)),
                _ => None,
            })
            .collect();
        c.0 = Instant::now();
    }
    c.1.clone()
}

/// This PC's IPv4 addresses a receiver could connect to, as (adapter
/// name, address), real network adapters first and virtual ones (VMware,
/// VirtualBox, Hyper-V/WSL, VPN tunnels) last and flagged.
pub fn local_addresses() -> Vec<(String, Ipv4Addr, bool)> {
    let mut v: Vec<(String, Ipv4Addr, bool)> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i.oper_status == if_addrs::IfOperStatus::Up)
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) if !v4.ip.is_loopback() && !v4.ip.is_link_local() => {
                let n = i.name.to_ascii_lowercase();
                let virt = i.is_p2p
                    || ["vmware", "virtualbox", "vethernet", "hyper-v", "vboxnet", "virbr", "docker", "vmnet", "tailscale", "zerotier", "wsl"]
                        .iter()
                        .any(|k| n.contains(k));
                Some((i.name, v4.ip, virt))
            }
            _ => None,
        })
        .collect();
    v.sort_by_key(|(_, _, virt)| *virt);
    v
}

/// Sends one beacon out of every network adapter, both to the multicast
/// group and as a subnet broadcast. Windows otherwise uses only its
/// default adapter, which with a VPN, Hyper-V/WSL or VirtualBox adapter
/// present is often not the LAN. Also sent to localhost, so two instances
/// on one PC find each other even without a network.
pub fn announce(a: &Announcement) {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    static SOCK: OnceLock<Option<Socket>> = OnceLock::new();
    let Some(sock) = SOCK.get_or_init(|| {
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok()?;
        let _ = s.set_multicast_loop_v4(true);
        let _ = s.set_multicast_ttl_v4(1);
        let _ = s.set_broadcast(true);
        s.bind(&SockAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))).ok()?;
        Some(s)
    }) else {
        return;
    };
    let Ok(json) = serde_json::to_vec(a) else { return };
    let send = |to: Ipv4Addr| {
        let _ = sock.send_to(&json, &SockAddr::from(SocketAddrV4::new(to, PORT)));
    };
    let ifaces = interfaces();
    for (ip, broadcast) in &ifaces {
        if sock.set_multicast_if_v4(ip).is_ok() {
            send(GROUP);
        }
        if let Some(b) = broadcast {
            send(*b);
        }
    }
    if ifaces.is_empty() {
        let _ = sock.set_multicast_if_v4(&Ipv4Addr::UNSPECIFIED);
        send(GROUP);
    }
    send(Ipv4Addr::LOCALHOST);
}

type Seen = HashMap<String, (Announcement, Instant)>;

/// The last address each sender name was heard from, kept after the sender
/// goes quiet, so a saved link still reconnects by name if beacons stop
/// arriving (e.g. a firewall change) while the sender itself is reachable.
fn last_known() -> &'static Mutex<HashMap<String, String>> {
    static LAST: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Where a sender with this name was last heard from, if ever.
pub fn last_known_address(name: &str) -> Option<String> {
    last_known().lock().get(name).cloned()
}

fn seen() -> &'static Mutex<Seen> {
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    SEEN.get_or_init(|| {
        // One listener per process, started on first use.
        std::thread::Builder::new()
            .name("link-discovery".into())
            .spawn(|| {
                let Ok(sock) = UdpSocket::bind(("0.0.0.0", PORT)) else {
                    log::warn!("Ferrous Link discovery: port {PORT} busy; connect by address instead");
                    return;
                };
                // Join the group on every adapter, not only the default one;
                // re-checked periodically for adapters that appear later.
                let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
                let mut joined: Vec<Ipv4Addr> = Vec::new();
                let mut last_join = Instant::now() - Duration::from_secs(60);
                let mut buf = [0u8; 2048];
                loop {
                    if last_join.elapsed() >= Duration::from_secs(10) {
                        last_join = Instant::now();
                        if joined.is_empty() {
                            let _ = sock.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED);
                        }
                        for (ip, _) in interfaces() {
                            if !joined.contains(&ip) && sock.join_multicast_v4(&GROUP, &ip).is_ok() {
                                joined.push(ip);
                            }
                        }
                    }
                    let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
                    if let Ok(mut a) = serde_json::from_slice::<Announcement>(&buf[..n]) {
                        a.host = match from {
                            SocketAddr::V4(v4) => v4.ip().to_string(),
                            SocketAddr::V6(v6) => v6.ip().to_string(),
                        };
                        // Prefer a real LAN address over loopback for the same sender.
                        let mut s = seen().lock();
                        let keep_existing = s.get(&a.id).is_some_and(|(old, t)| a.host == "127.0.0.1" && old.host != "127.0.0.1" && t.elapsed() < Duration::from_secs(3));
                        if !keep_existing {
                            last_known().lock().insert(a.name.clone(), a.address());
                            s.insert(a.id.clone(), (a, Instant::now()));
                        }
                    }
                }
            })
            .ok();
        Mutex::new(HashMap::new())
    })
}

/// Senders heard in the last 4 seconds.
pub fn discovered() -> Vec<Announcement> {
    let mut s = seen().lock();
    s.retain(|_, (_, t)| t.elapsed() < Duration::from_secs(4));
    let mut v: Vec<Announcement> = s.values().map(|(a, _)| a.clone()).collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// Starts the listener early so the list is warm when the UI asks.
pub fn start_listening() {
    let _ = seen();
}
