use std::collections::BTreeSet;
use std::env;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use swarm_discovery::{Discoverer, IpClass};
use tokio::time::sleep;

/// Convert an interface name to its OS interface index.
fn if_nametoindex(name: &str) -> u32 {
    let c_name = CString::new(name).expect("invalid interface name");
    let idx = unsafe { libc::if_nametoindex(c_name.as_ptr()) };
    if idx == 0 {
        panic!(
            "interface '{}' not found: {}",
            name,
            std::io::Error::last_os_error()
        );
    }
    idx
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "Usage: {} <peer-id> <port> [<interface-name>...]",
            args[0]
        );
        std::process::exit(1);
    }

    let peer_id = args[1].clone();
    let port: u16 = args[2].parse().expect("valid port");

    // Accept interface names and resolve to indices
    let iface_names: Vec<String> = args[3..].to_vec();
    let iface_indices: Vec<u32> = iface_names.iter().map(|n| if_nametoindex(n)).collect();

    // Get IP addresses for each interface to advertise
    let addrs: Vec<IpAddr> = if_addrs_for_names(&iface_names);

    let events_path = PathBuf::from(format!("/tmp/discovery-events-{}.jsonl", peer_id));
    let cmd_path = PathBuf::from(format!("/tmp/discovery-cmd-{}", peer_id));
    let ready_path = PathBuf::from(format!("/tmp/discovery-ready-{}", peer_id));

    // Truncate/create events file
    fs::write(&events_path, "").unwrap();

    let events_file = Arc::new(Mutex::new(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)
            .unwrap(),
    ));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let events = events_file.clone();
    let mut discoverer = Discoverer::new("nixtest".to_owned(), peer_id.clone())
        .with_ip_class(IpClass::V4Only)
        .with_multicast_interfaces_v4(iface_indices)
        .with_cadence(Duration::from_millis(500))
        .with_response_rate(5.0);
    if !addrs.is_empty() {
        discoverer = discoverer.with_addrs(port, addrs);
    }
    let guard = discoverer
        .with_callback(move |pid, peer| {
            let line = if peer.is_expiry() {
                format!(r#"{{"event":"lost","peer_id":"{}"}}"#, pid)
            } else {
                let addrs_json: Vec<String> = peer
                    .addrs()
                    .iter()
                    .map(|(ip, p)| format!(r#"["{}",{}]"#, ip, p))
                    .collect();
                format!(
                    r#"{{"event":"discovered","peer_id":"{}","addrs":[{}]}}"#,
                    pid,
                    addrs_json.join(",")
                )
            };
            let mut f = events.lock().unwrap();
            writeln!(f, "{}", line).unwrap();
            f.flush().unwrap();
        })
        .spawn(rt.handle())
        .expect("spawn discoverer");

    // Mirror the actively advertising interface set (as confirmed by the
    // discoverer's watch channel) into a file the test script can poll.
    let interfaces_path = PathBuf::from(format!("/tmp/discovery-interfaces-{}", peer_id));
    let mut interfaces_rx = guard.subscribe_multicast_interfaces_v4();
    write_interfaces(&interfaces_path, &interfaces_rx.borrow());
    {
        let interfaces_path = interfaces_path.clone();
        rt.spawn(async move {
            while interfaces_rx.changed().await.is_ok() {
                let set = interfaces_rx.borrow_and_update().clone();
                write_interfaces(&interfaces_path, &set);
            }
        });
    }

    // Signal readiness
    fs::write(&ready_path, "ready").unwrap();
    eprintln!("Test node {} ready", peer_id);

    // Command polling loop
    rt.block_on(async {
        loop {
            sleep(Duration::from_millis(100)).await;

            let contents = match fs::read_to_string(&cmd_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let contents = contents.trim();
            if contents.is_empty() {
                continue;
            }

            let _ = fs::remove_file(&cmd_path);

            let parts: Vec<&str> = contents.split_whitespace().collect();
            match parts.first().copied() {
                Some("add_interface") => {
                    let ifindex = if_nametoindex(parts[1]);
                    guard.add_interface_v4(ifindex);
                    eprintln!("Added interface {} (index {})", parts[1], ifindex);
                }
                Some("add_interface_index") => {
                    // Raw index variant: lets tests request indices that do
                    // not resolve to any interface, to verify failed
                    // registrations never surface in the active set.
                    let ifindex: u32 = parts[1].parse().unwrap();
                    guard.add_interface_v4(ifindex);
                    eprintln!("Requested interface index {}", ifindex);
                }
                Some("remove_interface") => {
                    let ifindex = if_nametoindex(parts[1]);
                    guard.remove_interface_v4(ifindex);
                    eprintln!("Removed interface {} (index {})", parts[1], ifindex);
                }
                Some("add_addr") => {
                    let p: u16 = parts[1].parse().unwrap();
                    let addr: IpAddr = parts[2].parse().unwrap();
                    guard.add(p, vec![addr]);
                    eprintln!("Added addr {}:{}", addr, p);
                }
                Some("remove_addr") => {
                    let addr: IpAddr = parts[1].parse().unwrap();
                    guard.remove_addr(addr);
                    eprintln!("Removed addr {}", addr);
                }
                Some("shutdown") => {
                    eprintln!("Shutting down");
                    break;
                }
                other => {
                    eprintln!("Unknown command: {:?}", other);
                }
            }
        }
    });

    drop(guard);
}

/// Write the active interface set as a compact JSON array (e.g. `[2,3]`),
/// atomically via rename so a concurrently polling reader never sees a
/// partial write.
fn write_interfaces(path: &Path, set: &BTreeSet<u32>) {
    let content = format!(
        "[{}]",
        set.iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, &content).unwrap();
    fs::rename(&tmp, path).unwrap();
}

/// Look up the IPv4 addresses assigned to the given interface names.
fn if_addrs_for_names(names: &[String]) -> Vec<IpAddr> {
    let all_addrs = if_addrs::get_if_addrs().expect("get_if_addrs");
    let mut result = Vec::new();
    for iface in all_addrs {
        if names.contains(&iface.name) && iface.addr.ip().is_ipv4() {
            result.push(iface.addr.ip());
        }
    }
    result
}
