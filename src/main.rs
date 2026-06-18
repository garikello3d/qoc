use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand, ValueEnum};
use qoc::{Arch, Debian, Distro};

#[derive(Parser)]
#[command(name = "qoc")]
#[command(about = "Create and run Linux distros")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, ValueEnum)]
enum DistroKind {
    Debian,
    Arch,
}

impl DistroKind {
    fn build(self) -> Box<dyn Distro> {
        match self {
            DistroKind::Debian => Box::new(Debian),
            DistroKind::Arch => Box::new(Arch),
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new rootfs for the chosen distro
    Create {
        /// Path to the rootfs directory (absolute or relative)
        #[arg(short, long)]
        rootfs: PathBuf,

        /// Which distribution to bootstrap
        #[arg(long, value_enum)]
        distro: DistroKind,
    },
    /// Run a VM mounting an existing rootfs (distro is auto-detected from etc/os-release)
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
    let result: Result<(), _> = match cli.command {
        Commands::Create { rootfs, distro } => qoc::create(distro.build().as_ref(), rootfs),
        Commands::Run { rootfs, nr_network_cards, show_log } => {
            qoc::detect_distro(&rootfs)
                .and_then(|distro| qoc::run(distro.as_ref(), rootfs, nr_network_cards, show_log))
                .map(|_| ())
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        process::exit(1);
    }
}
