use std::path::PathBuf;
use clap::Parser;
use mozart_core::config::{Config, WebAdapterConfig};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    print_config: bool,
}

// Bind before creating worker threads. File capabilities apply at exec; only the
// listening socket needs them, not HTTP handlers or any subsequently spawned CLI.
fn drop_bind_capability() -> std::io::Result<()> {
    #[repr(C)]
    struct Header { version: u32, pid: i32 }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Data { effective: u32, permitted: u32, inheritable: u32 }
    let header = Header { version: 0x2008_0522, pid: 0 };
    let data = [Data { effective: 0, permitted: 0, inheritable: 0 }; 2];
    // Linux capability ABI v3 uses two data words. Both pointers remain valid.
    if unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Restore normal same-user process inspection after dropping exec privileges.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let path = Config::resolve_path(args.config.as_deref());
    let config = WebAdapterConfig::load_from(path.as_deref())?;
    if args.print_config {
        println!("{}", mozart_web_adapter::config_description(&config)?);
        return Ok(());
    }
    let listener = std::net::TcpListener::bind(config.web.listen)?;
    drop_bind_capability()?;
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run(path, config, listener))
}

async fn run(path: Option<PathBuf>, config: WebAdapterConfig, listener: std::net::TcpListener) -> Result<(), Box<dyn std::error::Error>> {
    let app = mozart_web_adapter::build(config, path).await?;
    let listener = mozart_web_adapter::transport::Listener::from_std(listener)?;
    eprintln!("web adapter listening on {}", listener.local_addr()?);
    let streams = app.streams.clone();
    axum::serve(listener, app.router.into_make_service_with_connect_info::<mozart_web_adapter::transport::Transport>()).with_graceful_shutdown(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
        streams.shutdown();
    }).await?;
    Ok(())
}
