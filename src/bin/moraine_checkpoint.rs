use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use morainefs::{
    CheckpointConfig, Checkpointer, Durability, FileMetadataStore, FileNamespaceJournal, Layout,
    MetadataStore,
};
use socket2::SockRef;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DurabilityArg {
    Syncfs,
    File,
}

impl From<DurabilityArg> for Durability {
    fn from(value: DurabilityArg) -> Self {
        match value {
            DurabilityArg::Syncfs => Self::SyncFs,
            DurabilityArg::File => Self::File,
        }
    }
}

#[derive(Debug, Parser)]
#[command(about = "MoraineFS asynchronous durable-tier checkpointer")]
struct Args {
    #[arg(long, default_value = "/var/lib/morainefs/overlay")]
    overlay_root: PathBuf,
    #[arg(long, default_value = "/var/lib/morainefs/metadata/generations")]
    generation_root: PathBuf,
    #[arg(long, default_value = "/var/lib/morainefs/metadata/journal")]
    journal_root: PathBuf,
    #[arg(long, default_value = "/run/morainefs/checkpoint.sock")]
    socket: PathBuf,
    #[arg(long)]
    source_root: PathBuf,
    #[arg(long, default_value_t = 64)]
    batch_max_files: usize,
    #[arg(long, default_value_t = 0.25)]
    batch_delay: f64,
    #[arg(long, default_value_t = 0.0)]
    settle_delay: f64,
    #[arg(long, value_enum, default_value_t = DurabilityArg::Syncfs)]
    durability: DurabilityArg,
    #[arg(long, default_value_t = 60.0)]
    scan_interval: f64,
    #[arg(long)]
    recover_incomplete_renames: bool,
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    run(&args)
}

fn run(args: &Args) -> io::Result<()> {
    let checkpointer = build_checkpointer(args)?;
    let rename_recovery = checkpointer.recover_pending_renames(args.recover_incomplete_renames)?;
    let (startup_seen, startup_dirty, startup_pruned) = checkpointer.scan_existing()?;
    println!(
        "checkpoint daemon ready socket={} root={} mode=batch batch_max_files={} batch_delay={} settle_delay={} durability={:?} rename_recovery={rename_recovery:?} startup_seen={startup_seen} startup_dirty={startup_dirty} startup_pruned={startup_pruned}",
        args.socket.display(),
        checkpointer.layout().overlay_root.display(),
        args.batch_max_files,
        args.batch_delay,
        args.settle_delay,
        args.durability,
    );

    let socket = bind_socket(&args.socket)?;
    let worker_cp = Arc::clone(&checkpointer);
    let worker = thread::Builder::new()
        .name("moraine-checkpoint-batch".to_owned())
        .spawn(move || worker_cp.run_worker())?;
    let result = event_loop(
        &checkpointer,
        &socket,
        duration(args.scan_interval, "scan-interval")?,
    );
    checkpointer.stop();
    let worker_result = worker
        .join()
        .map_err(|_| io::Error::other("checkpoint worker panicked"))?;
    drop(socket);
    let _ = fs::remove_file(&args.socket);
    result.and(worker_result)
}

fn build_checkpointer(args: &Args) -> io::Result<Arc<Checkpointer>> {
    let layout = Layout {
        overlay_root: args.overlay_root.clone(),
        source_root: args.source_root.clone(),
    };
    for root in [
        &layout.overlay_root,
        &args.generation_root,
        &args.journal_root,
    ] {
        fs::create_dir_all(root)?;
    }
    let store: Arc<dyn MetadataStore> = Arc::new(FileMetadataStore::new(
        layout.clone(),
        args.generation_root.clone(),
    ));
    let journal = Arc::new(FileNamespaceJournal::new(
        layout.clone(),
        args.journal_root.clone(),
    ));
    Ok(Arc::new(Checkpointer::new(
        layout,
        store,
        journal,
        CheckpointConfig {
            batch_max_files: args.batch_max_files,
            batch_delay: duration(args.batch_delay, "batch-delay")?,
            settle_delay: duration(args.settle_delay, "settle-delay")?,
            durability: args.durability.into(),
        },
    )?))
}

fn bind_socket(path: &PathBuf) -> io::Result<UnixDatagram> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "checkpoint socket has no parent",
        )
    })?;
    fs::create_dir_all(parent)?;
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let socket = UnixDatagram::bind(path)?;
    SockRef::from(&socket).set_recv_buffer_size(1024 * 1024)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    Ok(socket)
}

fn event_loop(
    checkpointer: &Checkpointer,
    socket: &UnixDatagram,
    scan_interval: Duration,
) -> io::Result<()> {
    let stopping = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&stopping);
    ctrlc::set_handler(move || signal_flag.store(true, Ordering::Release))
        .map_err(io::Error::other)?;
    let mut next_scan = Instant::now() + scan_interval;
    let mut buffer = vec![0_u8; 4096];
    while !stopping.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= next_scan {
            let (seen, dirty, pruned) = checkpointer.scan_existing()?;
            if dirty != 0 || pruned != 0 {
                println!("periodic_scan seen={seen} dirty={dirty} pruned={pruned}");
            }
            next_scan = now + scan_interval;
        }
        receive_event(checkpointer, socket, &mut buffer)?;
    }
    Ok(())
}

fn receive_event(
    checkpointer: &Checkpointer,
    socket: &UnixDatagram,
    buffer: &mut [u8],
) -> io::Result<()> {
    match socket.recv(buffer) {
        Ok(length) if !buffer[..length].contains(&0) => {
            let source = PathBuf::from(OsString::from_vec(buffer[..length].to_vec()));
            let _ = checkpointer.enqueue(&source, true);
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn duration(value: f64, name: &str) -> io::Result<Duration> {
    if !value.is_finite() || value < 0.0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be a finite non-negative number"),
        ));
    }
    Ok(Duration::from_secs_f64(value))
}
