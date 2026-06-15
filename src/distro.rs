use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Hostname set inside every guest, regardless of distro.
pub const HOSTNAME: &str = "qoc-vm";

/// Per-distribution behaviour for `create` (bootstrap + in-proot configuration)
/// and the boot-file naming that `run` needs. The shared orchestration lives in
/// `lib.rs`; only the parts that genuinely differ between distros are here.
pub trait Distro {
    /// Short identifier, used in progress messages.
    fn name(&self) -> &str;

    /// systemd unit name for the SSH server (`ssh` on Debian, `sshd` on Arch).
    fn ssh_service(&self) -> &str;

    /// Extra host binaries this distro's `bootstrap` needs, beyond the common
    /// `proot` required by every `create`.
    fn extra_binaries(&self) -> &[&str];

    /// Boot-file name prefixes produced under `<rootfs>/boot`, consumed by `run`.
    fn kernel_prefix(&self) -> &str;
    fn initramfs_prefix(&self) -> &str;

    /// Filesystem paths to bind into the proot environment (`-b <path>`).
    /// Default: none — Debian's debootstrap second stage aborts proot with a
    /// `compare_paths2` assertion when these binds are present.
    fn proot_binds(&self) -> &[&str] {
        &[]
    }

    /// Stage 1 (runs on the host): populate `rootfs` with a base system.
    fn bootstrap(&self, rootfs: &Path) -> Result<()>;

    /// Distro-specific portion of the in-proot script: repo/keyring prep,
    /// package install, and kernel + initramfs generation.
    fn install_and_kernel_script(&self) -> String;

    /// Optional trailing cleanup inside proot. Default: nothing.
    fn cleanup_script(&self) -> String {
        String::new()
    }

    /// Full script body run inside proot, after the `set -e` / `export PATH`
    /// preamble that `run_proot` prepends. Template method: distro-specific
    /// install/kernel work, then the shared system config, then cleanup.
    fn configure_script(&self) -> String {
        format!(
            "{}\n{}\n{}",
            self.install_and_kernel_script(),
            common_system_config(self.ssh_service()),
            self.cleanup_script(),
        )
    }
}

/// Shared in-proot configuration: root password, hostname, DHCP networking, and
/// enabling the SSH / networkd / resolved services. The single place these live.
fn common_system_config(ssh_service: &str) -> String {
    format!(
        "echo 'root:1111' | chpasswd
echo '{HOSTNAME}' > /etc/hostname
printf '[Match]\\nName=en*\\n[Network]\\nDHCP=yes\\n' > /etc/systemd/network/01-all.network
systemctl enable {ssh_service}
systemctl enable systemd-networkd
systemctl enable systemd-resolved"
    )
}

/// Debian bookworm via two-stage debootstrap + initramfs-tools.
pub struct Debian;

impl Distro for Debian {
    fn name(&self) -> &str {
        "debian"
    }

    fn ssh_service(&self) -> &str {
        "ssh"
    }

    fn extra_binaries(&self) -> &[&str] {
        &["fakeroot", "debootstrap"]
    }

    fn kernel_prefix(&self) -> &str {
        "vmlinuz-"
    }

    fn initramfs_prefix(&self) -> &str {
        "initrd.img-"
    }

    fn bootstrap(&self, rootfs: &Path) -> Result<()> {
        let rootfs_str = rootfs.to_str().context("rootfs path is not valid UTF-8")?;
        let status = Command::new("fakeroot")
            .args([
                "debootstrap",
                "--foreign",
                "--include=linux-image-amd64,initramfs-tools,openssh-server,systemd-sysv,systemd-resolved,dbus,locales,ethtool,firmware-linux-nonfree,firmware-misc-nonfree",
                "--components=main,contrib,non-free-firmware",
                "--arch=amd64",
                "bookworm",
                rootfs_str,
                "http://deb.debian.org/debian",
            ])
            .status()
            .context("failed to spawn fakeroot debootstrap")?;
        if !status.success() {
            bail!("debootstrap first stage failed");
        }
        Ok(())
    }

    fn install_and_kernel_script(&self) -> String {
        // Raw string keeps \n literal for the printf format inside the chroot.
        r#"/debootstrap/debootstrap --second-stage
printf 'virtio\nvirtio_pci\nvirtiofs\n' >> /etc/initramfs-tools/modules
update-initramfs -c -u -k all
echo 'en_GB.UTF-8 UTF-8' >> /etc/locale.gen
locale-gen"#
            .to_string()
    }
}

/// Arch Linux via the bootstrap tarball + pacman + mkinitcpio.
pub struct Arch;

impl Distro for Arch {
    fn name(&self) -> &str {
        "arch"
    }

    fn ssh_service(&self) -> &str {
        "sshd"
    }

    fn extra_binaries(&self) -> &[&str] {
        &["curl", "bsdtar"]
    }

    fn kernel_prefix(&self) -> &str {
        "vmlinuz-"
    }

    fn initramfs_prefix(&self) -> &str {
        "initramfs-linux"
    }

    fn proot_binds(&self) -> &[&str] {
        &["/dev", "/proc", "/sys"]
    }

    fn bootstrap(&self, rootfs: &Path) -> Result<()> {
        let rootfs_str = rootfs.to_str().context("rootfs path is not valid UTF-8")?;
        let tarball = "/tmp/archlinux-bootstrap-x86_64.tar.zst";

        let status = Command::new("curl")
            .args([
                "-fL",
                "-o",
                tarball,
                "https://geo.mirror.pkgbuild.com/iso/latest/archlinux-bootstrap-x86_64.tar.zst",
            ])
            .status()
            .context("failed to spawn curl")?;
        if !status.success() {
            bail!("downloading arch bootstrap tarball failed");
        }

        // bsdtar -C needs the target dir to exist; `create` guarantees it did
        // not exist beforehand, so this is a fresh directory.
        fs::create_dir_all(rootfs)
            .with_context(|| format!("failed to create {}", rootfs.display()))?;

        let status = Command::new("bsdtar")
            .args(["-x", "--strip-components=1", "-f", tarball, "-C", rootfs_str])
            .status()
            .context("failed to spawn bsdtar")?;
        if !status.success() {
            bail!("extracting arch bootstrap tarball failed");
        }
        Ok(())
    }

    fn install_and_kernel_script(&self) -> String {
        // Raw string: $repo/$arch/$KVER are shell expansions, and the sed
        // backslashes are literal. The script is passed straight to `bash -c`
        // (no outer shell), so no extra quoting/escaping is required.
        r#"echo 'nameserver 1.1.1.1' > /etc/resolv.conf
echo 'Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch' > /etc/pacman.d/mirrorlist
pacman-key --init
pacman-key --populate archlinux
sed -i 's/^\s*CheckSpace/#CheckSpace/' /etc/pacman.conf
pacman -Sy --noconfirm
rm -f /usr/share/libalpm/hooks/*
sed -i 's/#NoExtract   =/NoExtract = \/usr\/share\/libalpm\/hooks\/\*/g' /etc/pacman.conf
pacman -S --noconfirm --needed base openssh dbus ethtool linux mkinitcpio
echo linux images available:
ls /usr/lib/modules/*/vmlinuz
KVER=$(ls /usr/lib/modules/*/vmlinuz | cut -f5 -d'/')
echo KVER: $KVER
cp -v /usr/lib/modules/$KVER/vmlinuz /boot/vmlinuz-linux
sed -i 's/^MODULES=.*/MODULES=(virtio virtio_pci virtiofs)/' /etc/mkinitcpio.conf
grep virtio /etc/mkinitcpio.conf
depmod $KVER
mkinitcpio -k $KVER -c /etc/mkinitcpio.conf -g /boot/initramfs-linux.img
systemctl mask systemd-firstboot.service
ln -sf /usr/share/zoneinfo/UTC /etc/localtime
echo 'LANG=en_GB.UTF-8' > /etc/locale.conf
locale-gen
echo 'KEYMAP=us' > /etc/vconsole.conf"#
            .to_string()
    }

    fn cleanup_script(&self) -> String {
        // Gracefully stop the gpg-agent pacman-key spawned, otherwise proot
        // keeps tracing it and never returns to the host shell.
        "gpgconf --homedir /etc/pacman.d/gnupg --kill gpg-agent".to_string()
    }
}
