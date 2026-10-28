use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

mod xiafs;

use xiafs::Xiafs;

#[derive(Parser)]
#[command(name = "mkfs-xiafs", version, about = "Create xiafs filesystem images")]
struct Cli {
    /// Populate from this directory tree (recursive)
    #[arg(short = 'd', long)]
    directory: Option<PathBuf>,

    /// Output filename
    filename: PathBuf,

    /// Image size in MiB (1–4095)
    size_mb: u32,
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut xiafs = Xiafs::new(cli.size_mb)?;
    if let Some(src) = &cli.directory {
        xiafs.populate_root(src)?;
    }
    let stats = xiafs.write_to(&cli.filename)?;

    println!(
        "Created xiafs image: {} ({}MB, {} inodes, {} zones, magic=0x{:X})",
        cli.filename.display(),
        cli.size_mb,
        stats.ninodes,
        stats.nzones,
        xiafs::MAGIC,
    );
    println!(
        "First data zone: {}, data zones: {}",
        stats.first_data_zone, stats.ndatazones,
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mkfs-xiafs: {e:#}");
            ExitCode::FAILURE
        }
    }
}
