#[cfg(feature = "rpc-v3-transport")]
use std::process::Command;

#[cfg(feature = "rpc-v3-transport")]
#[test]
fn status_reports_blocking_transport_without_async_or_keep_alive_claims() {
    let output = Command::new(env!("CARGO_BIN_EXE_fastlink"))
        .arg("status")
        .output()
        .expect("run fastlink status");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 status output");
    assert!(stdout.contains("Synchronous blocking"));
    assert!(stdout.contains("Not implemented:"));
    assert!(stdout.contains("async I/O, keep-alive"));
}

#[cfg(feature = "rpc-v3-transport")]
#[test]
fn status_rejects_unexpected_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_fastlink"))
        .args(["status", "--unexpected"])
        .output()
        .expect("run fastlink status with an extra argument");

    assert_eq!(output.status.code(), Some(2));
}
