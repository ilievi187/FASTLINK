//! RSS bound for warm and new state sessions. CPU only.
//! Linux exposes /proc/self/status for RSS and high-watermark measurements.
//! Other platforms still execute the full 200-operation functional stress test,
//! but do not claim to measure process RSS.

use fastlink::qwen35_4b::ssm_state_bytes;
use fastlink::{end_state_session, spawn_server, sync_ssm, ServerConfig};
use std::fs;
use std::time::Duration;

#[cfg(target_os = "linux")]
fn rss_bytes() -> u64 {
    let text = fs::read_to_string("/proc/self/status").unwrap();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next().unwrap().parse().unwrap();
            return kb.saturating_mul(1024);
        }
    }
    panic!("VmRSS missing");
}

#[cfg(target_os = "linux")]
fn hwm_bytes() -> u64 {
    let text = fs::read_to_string("/proc/self/status").unwrap();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest.split_whitespace().next().unwrap().parse().unwrap();
            return kb.saturating_mul(1024);
        }
    }
    panic!("VmHWM missing");
}

#[test]
fn rss_stays_within_eight_ssm_records_across_100_warm_and_100_new() {
    let dir = std::env::temp_dir().join(format!("fastlink-fl4impl-rss-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let config = ServerConfig {
        chunk_queue_capacity: 2,
        connection_queue_capacity: 4,
        max_concurrent_sessions: 2,
        io_timeout: Duration::from_secs(60),
    };
    let running = spawn_server("127.0.0.1:0", &dir, config).unwrap();
    let addr = running.addr_string();
    let bytes = vec![9u8; ssm_state_bytes()];
    #[cfg(target_os = "linux")]
    let before = rss_bytes();
    #[cfg(target_os = "linux")]
    let hwm_before = hwm_bytes();

    let cold = sync_ssm(&addr, 1, 0, &bytes).unwrap();
    assert_eq!(cold.payload_bytes, bytes.len() as u64);
    let mut warm_payload = 0u64;
    for _ in 0..100 {
        let warm = sync_ssm(&addr, 1, 0, &bytes).unwrap();
        assert!(warm.reused);
        warm_payload += warm.payload_bytes;
    }
    assert_eq!(warm_payload, 0);

    for session in 2..102u64 {
        let created = sync_ssm(&addr, session, 0, &bytes).unwrap();
        assert_eq!(created.payload_bytes, bytes.len() as u64);
        end_state_session(&addr, session).unwrap();
    }
    end_state_session(&addr, 1).unwrap();
    drop(running);

    #[cfg(target_os = "linux")]
    {
        let after = rss_bytes();
        let hwm_after = hwm_bytes();
        let rss_delta = after.saturating_sub(before);
        let hwm_delta = hwm_after.saturating_sub(hwm_before);
        let cap = (ssm_state_bytes() as u64).saturating_mul(8);
        println!(
            "FL4-IMPL rss_before={before} rss_after={after} rss_delta={rss_delta} hwm_before={hwm_before} hwm_after={hwm_after} hwm_delta={hwm_delta} warm_payload={warm_payload} cap={cap}"
        );
        assert!(
            rss_delta <= cap,
            "rss delta {rss_delta} exceeds 8 SSM records ({cap})"
        );
        assert!(
            hwm_delta <= cap,
            "hwm delta {hwm_delta} exceeds 8 SSM records ({cap})"
        );
    }
    #[cfg(not(target_os = "linux"))]
    println!("FL4-IMPL stress PASSED (200 syncs); RSS/HWM unavailable on this platform");
    let _ = fs::remove_dir_all(&dir);
}
