use std::path::PathBuf;
use std::process;

use anyhow::{bail, Context};
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
    /// List kernel versions available in the rootfs (paired kernel image + initrd)
    ListKernels {
        /// Path to the rootfs directory
        #[arg(short, long)]
        rootfs: PathBuf,
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

        /// Kernel version to boot (e.g. "6.1.0-47-amd64"). Auto-selected when exactly one is available.
        #[arg(long)]
        kernel_version: Option<String>,
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
        Commands::ListKernels { rootfs } => {
            qoc::list_kernels(&rootfs).map(|kernels| {
                for v in &kernels {
                    println!("{v}");
                }
            })
        }
        Commands::Run { rootfs, nr_network_cards, show_log, kernel_version } => {
            (|| -> anyhow::Result<()> {
                let kver = match kernel_version {
                    Some(v) => v,
                    None => {
                        let kernels = qoc::list_kernels(&rootfs)?;
                        match kernels.as_slice() {
                            [v] => v.clone(),
                            [] => bail!("no kernels found in {}/boot", rootfs.display()),
                            _ => bail!(
                                "multiple kernels found; specify one with --kernel-version:\n  {}",
                                kernels.join("\n  ")
                            ),
                        }
                    }
                };
                let handle = qoc::start(rootfs, nr_network_cards, show_log, &kver)?;
                let qemu_pid = handle.qemu_pid();
                ctrlc::set_handler(move || {
                    unsafe { libc::kill(qemu_pid, libc::SIGTERM) };
                })
                .context("failed to set Ctrl-C handler")?;
                handle.wait_for_info()?;
                handle.wait()
            })()
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        process::exit(1);
    }
}
