use anyhow::{Result, bail};
use iroh::{Endpoint, endpoint::presets, protocol::Router};
use iroh_ping::Ping;
use iroh_tickets::endpoint::EndpointTicket;
use std::{env, str::FromStr, time::Instant};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("receiver") => receiver().await,
        Some("sender") => {
            let ticket = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("missing endpoint ticket"))?;
            sender(&ticket).await
        }
        Some("loopback") | None => loopback().await,
        Some(other) => {
            bail!("unknown command {other:?}; use receiver, sender <ticket>, or loopback")
        }
    }
}

async fn make_endpoint() -> Result<Endpoint> {
    let ep = Endpoint::bind(presets::N0).await?;
    match tokio::time::timeout(std::time::Duration::from_secs(5), ep.online()).await {
        Ok(()) => eprintln!("endpoint_online=true"),
        Err(_) => eprintln!("endpoint_online=false timeout_after=5s continuing_with_local_addr"),
    }
    Ok(ep)
}

async fn receiver() -> Result<()> {
    let endpoint = make_endpoint().await?;
    let ticket = EndpointTicket::new(endpoint.addr());
    println!("ticket={ticket}");
    let _router = Router::builder(endpoint)
        .accept(iroh_ping::ALPN, Ping::new())
        .spawn();
    println!("receiver=ready");
    tokio::signal::ctrl_c().await?;
    Ok(())
}

async fn sender(ticket: &str) -> Result<()> {
    let ticket = EndpointTicket::from_str(ticket)?;
    let endpoint = make_endpoint().await?;
    let pinger = Ping::new();
    let started = Instant::now();
    let rtt = pinger
        .ping(&endpoint, ticket.endpoint_addr().clone())
        .await?;
    println!("ping_rtt_reported_ms={:.3}", rtt.as_secs_f64() * 1000.0);
    println!(
        "wall_time_ms={:.3}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    endpoint.close().await;
    Ok(())
}

async fn loopback() -> Result<()> {
    let recv_ep = make_endpoint().await?;
    let ticket = EndpointTicket::new(recv_ep.addr());
    println!("ticket={ticket}");
    let _router = Router::builder(recv_ep.clone())
        .accept(iroh_ping::ALPN, Ping::new())
        .spawn();

    let send_ep = make_endpoint().await?;
    let pinger = Ping::new();
    let started = Instant::now();
    let rtt = pinger
        .ping(&send_ep, ticket.endpoint_addr().clone())
        .await?;
    println!(
        "loopback_ping_rtt_reported_ms={:.3}",
        rtt.as_secs_f64() * 1000.0
    );
    println!(
        "loopback_wall_time_ms={:.3}",
        started.elapsed().as_secs_f64() * 1000.0
    );

    send_ep.close().await;
    recv_ep.close().await;
    Ok(())
}
