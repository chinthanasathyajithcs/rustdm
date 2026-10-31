use rustdm::testing::{pattern_bytes, Mode, TestServer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let size = 64 * 1024 * 1024; // 64 MiB
    let body = pattern_bytes(size);
    let srv = TestServer::start_at(
        "127.0.0.1:8080",
        body,
        Mode::Throttle {
            bytes_per_sec: 4 * 1024 * 1024,
        },
    )
    .await?;

    eprintln!(
        "Demo server listening at http://127.0.0.1:8080/demo.bin (64 MiB, throttled to 4 MB/s)"
    );
    eprintln!("Run in another terminal:");
    eprintln!("  cargo run -- download http://127.0.0.1:8080/demo.bin");
    eprintln!("\nPress Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;
    eprintln!("\nShutting down demo server.");
    drop(srv);
    Ok(())
}
