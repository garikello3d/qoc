use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "qoc")]
#[command(about = "Create and run Linux distros")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new Debian rootfs
    Create {
        /// Path to the rootfs directory (absolute or relative)
        #[arg(short, long)]
        rootfs: PathBuf,
    },
    /// Run a VM mounting an existing rootfs
    Run {
        /// Path to the rootfs directory created by `create`
        #[arg(short, long)]
        rootfs: PathBuf,

        /// Number of emulated network cards (1–14)
        #[arg(short, long, default_value_t = 1)]
        nr_network_cards: usize,

        /// Print virtiofsd and QEMU output to the terminal
        #[arg(long)]
        show_log: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Create { rootfs } => qoc::create(rootfs),
        Commands::Run { rootfs, nr_network_cards, show_log } => qoc::run(rootfs, nr_network_cards, show_log),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        process::exit(1);
    }
}
