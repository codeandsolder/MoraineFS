use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use morainefs_control::admission::{
    AdmissionPolicy, AdmissionStats, AdmissionWorker, DirectoryMicroStore, MicroStore,
};

#[derive(Debug, Parser)]
#[command(about = "MoraineFS hot-tier admission worker")]
struct Args {
    #[arg(long, default_value = "/mnt/io-tier-zram-full")]
    micro_root: PathBuf,
    #[arg(long, default_value_t = 2 * 1024 * 1024)]
    max_size: u64,
    #[arg(long, default_value_t = 4)]
    workers: usize,
    #[arg(long, default_value_t = 32 * 1024 * 1024)]
    parent_budget: u64,
    #[arg(long, default_value_t = 4096)]
    parent_files: usize,
    #[arg(long, default_value_t = 180 * 1024 * 1024)]
    high_watermark: u64,
    #[arg(long, default_value_t = 160 * 1024 * 1024)]
    low_watermark: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Admit {
        directory: PathBuf,
    },
    Serve {
        #[arg(long, default_value = "/run/io-tierfs-admit.sock")]
        socket: PathBuf,
        #[arg(long, default_value_t = 2.0)]
        cooldown: f64,
    },
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    if args.low_watermark > args.high_watermark {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "low-watermark must not exceed high-watermark",
        ));
    }
    let policy = AdmissionPolicy {
        max_size: args.max_size,
        parent_budget: args.parent_budget,
        parent_files: args.parent_files,
        high_watermark: args.high_watermark,
        low_watermark: args.low_watermark,
    };
    let store: Arc<dyn MicroStore> = Arc::new(DirectoryMicroStore::new(args.micro_root.clone())?);
    let worker = AdmissionWorker::new(Arc::clone(&store), policy, args.workers)?;

    match args.command {
        Command::Admit { directory } => {
            let started = Instant::now();
            let result = worker.admit_dir(&directory);
            println!(
                "elapsed_ms={:.1} {:?}",
                started.elapsed().as_secs_f64() * 1000.0,
                result.counters
            );
            Ok(())
        }
        Command::Serve { socket, cooldown } => serve(&worker, &socket, cooldown),
    }
}

fn serve(worker: &AdmissionWorker, socket_path: &Path, cooldown_seconds: f64) -> io::Result<()> {
    let cooldown = duration(cooldown_seconds)?;
    let startup_eviction = worker.evict_if_needed(None)?;
    if startup_eviction
        .counters
        .get("evicted_files")
        .copied()
        .unwrap_or_default()
        != 0
    {
        println!("startup eviction {:?}", startup_eviction.counters);
    }
    let parent = socket_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "admission socket has no parent",
        )
    })?;
    fs::create_dir_all(parent)?;
    match fs::remove_file(socket_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let socket = UnixDatagram::bind(socket_path)?;
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o666))?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    println!("admission worker ready socket={}", socket_path.display());

    let stopping = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&stopping);
    ctrlc::set_handler(move || signal_flag.store(true, Ordering::Release))
        .map_err(io::Error::other)?;

    let mut last = HashMap::<PathBuf, Instant>::new();
    let mut buffer = vec![0_u8; 4096];
    while !stopping.load(Ordering::Acquire) {
        let length = match socket.recv(&mut buffer) {
            Ok(length) => length,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        if buffer[..length].contains(&0) {
            continue;
        }
        let directory = PathBuf::from(OsString::from_vec(buffer[..length].to_vec()));
        if !directory.is_absolute() {
            continue;
        }
        let now = Instant::now();
        if last
            .get(&directory)
            .is_some_and(|previous| now.duration_since(*previous) < cooldown)
        {
            continue;
        }
        last.insert(directory.clone(), now);
        let started = Instant::now();
        let mut result = worker.admit_dir(&directory);
        let eviction = worker.evict_if_needed(Some(&directory))?;
        merge_eviction(&mut result, eviction);
        println!(
            "admit dir={} elapsed_ms={:.1} {:?}",
            directory.display(),
            started.elapsed().as_secs_f64() * 1000.0,
            result.counters
        );
    }
    drop(socket);
    let _ = fs::remove_file(socket_path);
    Ok(())
}

fn merge_eviction(target: &mut AdmissionStats, source: AdmissionStats) {
    for (key, value) in source.counters {
        target.counters.insert(format!("evict_{key}"), value);
    }
}

fn duration(value: f64) -> io::Result<Duration> {
    if !value.is_finite() || value < 0.0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cooldown must be a finite non-negative number",
        ));
    }
    Ok(Duration::from_secs_f64(value))
}
