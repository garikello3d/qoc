use std::path::PathBuf;
use std::process;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use qoc::{Arch, Debian, DebianVersion, Distro};

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
    fn build(self, debian_version: Option<DebianVersion>) -> Result<Box<dyn Distro>> {
        match (self, debian_version) {
            (DistroKind::Debian, version) => Ok(Box::new(Debian::new(version.unwrap_or_default()))),
            (DistroKind::Arch, None) => Ok(Box::new(Arch)),
            (DistroKind::Arch, Some(_)) => {
                bail!("--debian-version can only be used with --distro debian")
            }
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

        /// Debian release codename to bootstrap (bookworm, bullseye, trixie)
        #[arg(long, value_name = "VERSION")]
        debian_version: Option<DebianVersion>,
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
        Commands::MakeInitrd {
            rootfs,
            kernel_version,
        } => qoc::make_initrd(&rootfs, &kernel_version)
            .map(|path| println!("initrd created: {}", path.display())),
        Commands::Create {
            rootfs,
            distro,
            debian_version,
        } => (|| -> Result<()> {
            let distro = distro.build(debian_version)?;
            qoc::create(distro.as_ref(), rootfs).map(|kernels| {
                for v in &kernels {
                    println!("kernel version available: {v}");
                }
            })
        })(),
        Commands::ListKernels { rootfs } => qoc::list_kernels(&rootfs).map(|kernels| {
            for v in &kernels {
                println!("{v}");
            }
        }),
        Commands::Run {
            rootfs,
            nr_network_cards,
            show_log,
            kernel_version,
        } => (|| -> anyhow::Result<()> {
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
            let (handle, kver) = qoc::start(rootfs, nr_network_cards, show_log, &kver)?;
            println!("kernel version selected: {kver}");
            let qemu_pid = handle.qemu_pid();
            ctrlc::set_handler(move || {
                unsafe { libc::kill(qemu_pid, libc::SIGTERM) };
            })
            .context("failed to set Ctrl-C handler")?;
            handle.wait_for_info()?;
            handle.wait()
        })(),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debian_distro_build_accepts_debian_version() {
        let distro = DistroKind::Debian
            .build(Some(DebianVersion::Trixie))
            .unwrap();
        assert_eq!(distro.name(), "debian");
    }

    #[test]
    fn debian_distro_build_defaults_to_bookworm() {
        let distro = DistroKind::Debian.build(None).unwrap();
        assert_eq!(distro.name(), "debian");
    }

    #[test]
    fn arch_distro_build_rejects_debian_version() {
        let err = match DistroKind::Arch.build(Some(DebianVersion::Bullseye)) {
            Ok(_) => panic!("arch accepted --debian-version"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("--debian-version"));
    }
}
