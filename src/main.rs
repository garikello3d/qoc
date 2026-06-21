use std::path::PathBuf;
use std::process;

use anyhow::Context;
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
    /// Regenerate an initrd image inside an existing rootfs
    MakeInitrd {
        /// Path to the rootfs directory
        #[arg(short, long)]
        rootfs: PathBuf,

        /// Kernel version string (e.g. "6.1.0-47-amd64" or "7.0.12-arch1-1")
        #[arg(long)]
        kernel_version: String,
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

        /// Custom kernel image; auto-detected from rootfs/boot/ when absent
        #[arg(long)]
        kernel: Option<PathBuf>,

        /// Custom initrd image; auto-detected from rootfs/boot/ when absent
        #[arg(long)]
        initrd: Option<PathBuf>,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::MakeInitrd { rootfs, kernel_version } => {
            qoc::make_initrd(&rootfs, &kernel_version)
                .map(|path| println!("initrd created: {}", path.display()))
        }
        Commands::Create { rootfs, distro } => qoc::create(distro.build().as_ref(), rootfs),
        Commands::Run { rootfs, nr_network_cards, show_log, kernel, initrd } => {
            qoc::detect_distro(&rootfs).and_then(|distro| {
                let handle = qoc::start(distro.as_ref(), rootfs, nr_network_cards, show_log, kernel, initrd)?;
                let qemu_pid = handle.qemu_pid();
                ctrlc::set_handler(move || {
                    unsafe { libc::kill(qemu_pid, libc::SIGTERM) };
                })
                .context("failed to set Ctrl-C handler")?;
                handle.wait_for_info()?;
                handle.wait()
            })
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        process::exit(1);
    }
}
