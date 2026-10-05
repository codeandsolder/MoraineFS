use std::io;
use std::path::PathBuf;

use clap::Parser;
use morainefs_control::range_gc::collect_stale_process_dirs;

#[derive(Debug, Parser)]
#[command(about = "Remove stale per-process MoraineFS range-cache directories")]
struct Args {
    #[arg(long, default_value = "/var/cache/io-tierfs/range-cache")]
    root: PathBuf,
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let stats = collect_stale_process_dirs(&args.root)?;
    println!(
        "removed_dirs={} removed_bytes={} active_dirs={}",
        stats.removed_dirs, stats.removed_bytes, stats.active_dirs
    );
    Ok(())
}
