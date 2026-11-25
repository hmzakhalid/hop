//! The Mac's network, as Hop needs it: where to broadcast for PCs, this Mac's name, the voice
//! queue for input packets, and whether AirDrop's radio (AWDL) is busy.

use std::ffi::{c_char, c_void, CStr};

/// Whether AWDL (AirDrop, Handoff) is active: it causes periodic Wi-Fi latency spikes.
pub fn awdl_active() -> Option<bool> {
    let out = std::process::Command::new("/sbin/ifconfig").arg("awdl0").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    Some(s.contains("status: active"))
}

/// IPv4 broadcast addresses of the interfaces that are up.
pub fn broadcast_addrs() -> Vec<std::net::IpAddr> {
    let mut out = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut p = ifap;
        while !p.is_null() {
            let i = &*p;
            let up = i.ifa_flags & (libc::IFF_UP as u32) != 0;
            let bcast = i.ifa_flags & (libc::IFF_BROADCAST as u32) != 0;
            if up && bcast && !i.ifa_addr.is_null() && (*i.ifa_addr).sa_family as i32 == libc::AF_INET && !i.ifa_dstaddr.is_null() {
                let sin = &*(i.ifa_dstaddr as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !ip.is_unspecified() && !out.contains(&std::net::IpAddr::V4(ip)) {
                    out.push(std::net::IpAddr::V4(ip));
                }
            }
            p = i.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
}

/// Mark the socket as interactive voice: the Wi-Fi driver puts it in the WMM voice queue.
pub fn set_voice_priority(sock: &std::net::UdpSocket) {
    use std::os::fd::AsRawFd;
    let fd = sock.as_raw_fd();
    unsafe {
        let vo: libc::c_int = 4; // NET_SERVICE_TYPE_VO
        libc::setsockopt(fd, libc::SOL_SOCKET, 0x1116, &vo as *const _ as *const c_void, 4);
        let tos: libc::c_int = 0xB8; // DSCP EF
        libc::setsockopt(fd, libc::IPPROTO_IP, libc::IP_TOS, &tos as *const _ as *const c_void, 4);
    }
}

pub fn host_name() -> String {
    let mut buf = [0 as c_char; 256];
    unsafe {
        if libc::gethostname(buf.as_mut_ptr(), buf.len()) == 0 {
            let s = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
            return s.trim_end_matches(".local").to_string();
        }
    }
    "Mac".into()
}
