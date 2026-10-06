use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use fuser::{Config, MountOption, SessionACL};
use morainefs::{
    DirectoryMicroStore, FileMetadataStore, FileNamespaceJournal, ForegroundConfig, Layout,
    MetadataStore, MicroStore, MoraineFs, NamespaceJournal, Policy, RangeCache, TierState,
};

#[derive(Debug, Parser)]
#[command(about = "MoraineFS tiered FUSE filesystem")]
struct Args {
    #[arg(long)]
    source_root: PathBuf,
    mountpoint: PathBuf,
    #[arg(long)]
    policy: Option<PathBuf>,

    #[arg(long, default_value = "/var/lib/morainefs/overlay")]
    durable_overlay: PathBuf,
    #[arg(long, default_value = "/var/lib/morainefs/metadata/generations")]
    durable_generations: PathBuf,
    #[arg(long, default_value = "/var/lib/morainefs/metadata/journal")]
    durable_journal: PathBuf,
    #[arg(long, default_value = "/run/morainefs/checkpoint.sock")]
    durable_checkpoint_socket: PathBuf,

    #[arg(long, default_value = "/run/morainefs/volatile/overlay")]
    volatile_overlay: PathBuf,
    #[arg(long, default_value = "/run/morainefs/volatile/metadata/generations")]
    volatile_generations: PathBuf,
    #[arg(long, default_value = "/run/morainefs/volatile/metadata/journal")]
    volatile_journal: PathBuf,
    #[arg(long, default_value = "/run/morainefs/volatile/checkpoint.sock")]
    volatile_checkpoint_socket: PathBuf,

    #[arg(long, default_value = "/mnt/morainefs-hot")]
    micro_root: PathBuf,
    #[arg(long, default_value = "/var/cache/morainefs/ranges")]
    range_cache_root: PathBuf,
    #[arg(long, default_value_t = 512 * 1024 * 1024)]
    range_cache_capacity: u64,
    #[arg(long)]
    no_range_cache: bool,
    #[arg(long, default_value = "/run/morainefs/admit.sock")]
    admission_socket: PathBuf,
    #[arg(long)]
    defer_file_namespace: bool,
    #[arg(long)]
    relaxed_create_durability: bool,
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    writeback_copy_max: u64,
    #[arg(long)]
    threads: Option<usize>,
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    if !rustix::process::geteuid().is_root() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "MoraineFS foreground must run as root for Linux FUSE passthrough",
        ));
    }
    fs::create_dir_all(&args.mountpoint)?;
    let policy = Policy::load(args.policy.as_deref())?;

    let durable = tier(
        &args.source_root,
        args.durable_overlay,
        args.durable_generations,
        args.durable_journal,
        args.durable_checkpoint_socket,
    )?;
    let volatile = tier(
        &args.source_root,
        args.volatile_overlay,
        args.volatile_generations,
        args.volatile_journal,
        args.volatile_checkpoint_socket,
    )?;
    let micro: Arc<dyn MicroStore> = Arc::new(DirectoryMicroStore::new(args.micro_root)?);
    let range_cache = if args.no_range_cache {
        None
    } else {
        Some(Arc::new(RangeCache::new(
            args.range_cache_root,
            args.range_cache_capacity,
        )?))
    };
    let filesystem = MoraineFs::new(ForegroundConfig {
        source_root: args.source_root,
        policy,
        durable,
        volatile,
        micro,
        range_cache,
        admission_socket: args.admission_socket,
        defer_file_namespace: args.defer_file_namespace,
        relaxed_create_durability: args.relaxed_create_durability,
        writeback_copy_max: args.writeback_copy_max,
    })?;

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RW,
        MountOption::DefaultPermissions,
        MountOption::AutoUnmount,
        MountOption::FSName("morainefs".to_owned()),
        MountOption::Subtype("morainefs".to_owned()),
    ];
    config.acl = SessionACL::All;
    config.n_threads = args
        .threads
        .or_else(|| std::thread::available_parallelism().ok().map(usize::from));
    config.clone_fd = config.n_threads.is_some_and(|threads| threads > 1);
    fuser::mount(filesystem, &args.mountpoint, &config)
}

fn tier(
    source_root: &Path,
    overlay_root: PathBuf,
    generation_root: PathBuf,
    journal_root: PathBuf,
    checkpoint_socket: PathBuf,
) -> io::Result<TierState> {
    let layout = Layout {
        overlay_root,
        source_root: source_root.to_path_buf(),
    };
    for root in [&layout.overlay_root, &generation_root, &journal_root] {
        fs::create_dir_all(root)?;
    }
    let metadata: Arc<dyn MetadataStore> =
        Arc::new(FileMetadataStore::new(layout.clone(), generation_root));
    let journal: Arc<dyn NamespaceJournal> =
        Arc::new(FileNamespaceJournal::new(layout.clone(), journal_root));
    Ok(TierState {
        layout,
        metadata,
        journal,
        checkpoint_socket,
    })
}
