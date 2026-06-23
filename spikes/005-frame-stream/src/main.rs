//! Spike 005: Stream screen frames over Iroh
//!
//! Host mode:  capture screen → JPEG encode → stream over Iroh
//! Client mode: connect to host → receive frames → save to disk
//!
//! Usage:
//!   cargo run -- host          # starts host, prints addr_json
//!   cargo run -- client <addr_json>  # connects, saves 10 frames

use anyhow::{Context, Result, bail};
use iroh::{Endpoint, EndpointAddr, SecretKey, endpoint::presets};
use iroh::endpoint::Connection;
use iroh::protocol::{ProtocolHandler, Router};
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ALPN: &[u8] = b"keyhome/frame-stream/0";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("host") => host().await,
        Some("client") => {
            let addr = args.get(1).context("missing addr arg")?;
            client(addr).await
        }
        _ => {
            bail!("usage: cargo run -- host | client <addr_json>")
        }
    }
}

async fn host() -> Result<()> {
    println!("[host] starting...");

    let secret = SecretKey::generate();
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .bind()
        .await?;

    match tokio::time::timeout(Duration::from_secs(5), endpoint.online()).await {
        Ok(()) => println!("[host] endpoint online"),
        Err(_) => println!("[host] endpoint online timeout (continuing)"),
    }

    let node_id = endpoint.id();
    let addr = endpoint.addr();
    println!("[host] node_id={}", node_id);
    let addr_json = serde_json::to_string(&addr)?;
    println!("[host] addr_json={}", addr_json);
    println!("[host] waiting for client on ALPN {:?}", ALPN);

    let handler = Arc::new(FrameStreamHandler);
    let _router = Router::builder(endpoint.clone())
        .accept(ALPN, handler)
        .spawn();

    // Keep alive until killed
    tokio::signal::ctrl_c().await?;
    endpoint.close().await;
    Ok(())
}

#[derive(Debug)]
struct FrameStreamHandler;

impl ProtocolHandler for FrameStreamHandler {
    async fn accept(&self, conn: Connection) -> Result<(), iroh::protocol::AcceptError> {
        if let Err(e) = stream_frames(conn).await {
            eprintln!("[host] stream error: {}", e);
        }
        Ok(())
    }
}

async fn stream_frames(conn: Connection) -> Result<()> {
    let (mut send, mut recv) = conn.accept_bi().await?;
    println!("[host] bi-stream accepted, connection from {}", conn.remote_id());

    // Read start signal
    let mut start_buf = [0u8; 1];
    recv.read_exact(&mut start_buf).await?;
    println!("[host] received start signal");

    // Capture screen
    let monitors = xcap::Monitor::all().context("failed to enumerate monitors")?;
    let monitor = monitors.into_iter().next().context("no monitors found")?;
    println!(
        "[host] capturing: {} ({}x{})",
        monitor.name()?,
        monitor.width()?,
        monitor.height()?
    );

    let mut frame_count = 0u32;
    let start = Instant::now();

    loop {
        // Capture frame
        let image = monitor.capture_image()?;

        // JPEG encode (convert RGBA8 → RGB8 first, JPEG doesn't support alpha)
        let rgb_image = image::DynamicImage::ImageRgba8(image).to_rgb8();
        let (w, h) = (rgb_image.width(), rgb_image.height());
        let mut jpeg_buf = Vec::with_capacity(50_000);
        rgb_image.write_to(&mut Cursor::new(&mut jpeg_buf), image::ImageFormat::Jpeg)?;
        let jpeg_size = jpeg_buf.len();

        // Frame header: [width:u32][height:u32][jpeg_len:u32][jpeg_data]
        let header = [
            (w as u32).to_be_bytes(),
            (h as u32).to_be_bytes(),
            (jpeg_size as u32).to_be_bytes(),
        ]
        .concat();

        send.write_all(&header).await?;
        send.write_all(&jpeg_buf).await?;

        frame_count += 1;
        let elapsed = start.elapsed();
        let fps = frame_count as f64 / elapsed.as_secs_f64().max(0.001);
        print!(
            "\r[host] frame={} {}x{} jpeg={}B fps={:.1} elapsed={:.1}s",
            frame_count, w, h, jpeg_size, fps, elapsed.as_secs_f64()
        );

        // Check for client disconnect
        match tokio::time::timeout(Duration::from_millis(1), recv.read(&mut [0u8; 1])).await {
            Ok(Ok(Some(_))) => {
                println!("\n[host] client disconnected");
                break;
            }
            _ => {}
        }

        // Throttle to ~30fps
        tokio::time::sleep(Duration::from_millis(33)).await;
    }

    println!(
        "[host] sent {} frames in {:.1}s",
        frame_count,
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

async fn client(addr_str: &str) -> Result<()> {
    println!("[client] connecting...");

    let secret = SecretKey::generate();
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .bind()
        .await?;

    let addr: EndpointAddr = serde_json::from_str(addr_str)
        .context("invalid EndpointAddr JSON")?;

    println!("[client] dialing...");
    let conn = endpoint
        .connect(addr, ALPN)
        .await
        .context("failed to connect")?;

    println!("[client] connected!");

    let (mut send, mut recv) = conn.open_bi().await?;
    println!("[client] bi-stream opened");

    // Send start signal
    send.write_all(&[1u8]).await?;
    println!("[client] sent start signal");

    // Receive frames
    let out_dir = std::path::Path::new("/tmp/keyhome-frames");
    std::fs::create_dir_all(out_dir)?;
    println!("[client] saving frames to {}", out_dir.display());

    let max_frames = 10;
    let start = Instant::now();

    for i in 0..max_frames {
        // Read frame header
        let mut header = [0u8; 12];
        recv.read_exact(&mut header).await?;
        let w = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let h = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        let jpeg_len = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;

        // Read JPEG data
        let mut jpeg_buf = vec![0u8; jpeg_len];
        recv.read_exact(&mut jpeg_buf).await?;

        // Save to disk
        let path = out_dir.join(format!("frame_{:03}.jpg", i));
        std::fs::write(&path, &jpeg_buf)?;

        let elapsed = start.elapsed();
        let fps = (i + 1) as f64 / elapsed.as_secs_f64().max(0.001);
        println!(
            "[client] frame={} {}x{} jpeg={}B saved={} fps={:.1}",
            i, w, h, jpeg_len, path.display(), fps
        );
    }

    // Send stop signal
    send.write_all(&[0u8]).await?;
    println!("[client] sent stop signal");

    println!(
        "\n[client] received {} frames in {:.1}s ({:.1} fps avg)",
        max_frames,
        start.elapsed().as_secs_f64(),
        max_frames as f64 / start.elapsed().as_secs_f64().max(0.001)
    );

    endpoint.close().await;
    Ok(())
}
