use conerror::conerror;
use log::info;

use crate::util::select::{select, Either};

#[cfg(unix)]
#[conerror]
pub async fn catch_signal() -> conerror::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut term = signal(SignalKind::terminate())?;
    match select(interrupt.recv(), term.recv()).await {
        Either::Left(_) => info!("receive SIGINT"),
        Either::Right(_) => info!("receive SIGTERM"),
    };
    Ok(())
}

#[cfg(windows)]
#[conerror]
async fn catch_signal() -> conerror::Result<()> {
    tokio::signal::ctrl_c().await?;
    info!("receive SIGINT");
    Ok(())
}
