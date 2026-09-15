use std::fmt;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::{self, Command};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

const DEBIAN_COMMON_BOOTSTRAP_PACKAGES: &[&str] = &[
    "linux-image-amd64",
    "initramfs-tools",
    "openssh-server",
    "systemd-sysv",
    "dbus",
    "locales",
    "ethtool",
    "firmware-linux-nonfree",
    "firmware-misc-nonfree",
];

const ALPINE_VERSION: &str = "3.22.1";
const ALPINE_MINIROOTFS_URL: &str = "https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/alpine-minirootfs-3.22.1-x86_64.tar.gz";
const ALPINE_MINIROOTFS_SHA256: &str =
    "0e5cc5702ad72a4e151f219976ba946d50161c3acce210ef3b122a529aba1270";

/// Hostname set inside every guest, regardless of distro.
pub const HOSTNAME: &str = "qoc-vm";

/// Per-distribution behaviour for `create` (bootstrap + in-proot configuration)
/// and the boot-file naming that `run` needs. The shared orchestration lives in
/// `lib.rs`; only the parts that genuinely differ between distros are here.
pub trait Distro {
    /// Short identifier, used in progress messages.
    fn name(&self) -> &str;

    /// Init-system service name for the SSH server (`ssh` on Debian, `sshd` elsewhere).
    fn ssh_service(&self) -> &str;

    /// Shell used to execute configuration commands inside proot.
    fn proot_shell(&self) -> &str {
        "/bin/bash"
    }

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

    /// Init-system-specific network and service configuration.
    fn system_config_script(&self) -> String {
        systemd_system_config(self.ssh_service())
    }

    /// Full script body run inside proot, after the `set -e` / `export PATH`
    /// preamble that `run_proot` prepends. Template method: distro-specific
    /// install/kernel work, then the shared system config, then cleanup.
    fn configure_script(&self) -> String {
        format!(
            "{}\n{}\n{}\n{}",
            self.install_and_kernel_script(),
            common_identity_config(),
            self.system_config_script(),
            self.cleanup_script(),
        )
    }
}

fn common_identity_config() -> String {
    format!("echo 'root:1111' | chpasswd\necho '{HOSTNAME}' > /etc/hostname")
}

/// Shared systemd configuration used by Debian and Arch.
fn systemd_system_config(ssh_service: &str) -> String {
    format!(
        "printf '[Match]\\nName=en*\\n[Network]\\nDHCP=yes\\n' > /etc/systemd/network/01-all.network
systemctl enable {ssh_service}
systemctl enable systemd-networkd
systemctl enable systemd-resolved"
    )
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open downloaded file {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read downloaded file {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        bail!(
            "SHA-256 mismatch for {}: expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DebianVersion {
    Bookworm,
    Bullseye,
    Trixie,
}

impl DebianVersion {
    pub const fn as_str(self) -> &'static str {
        match self {
            DebianVersion::Bookworm => "bookworm",
            DebianVersion::Bullseye => "bullseye",
            DebianVersion::Trixie => "trixie",
        }
    }

    const fn components(self) -> &'static str {
        match self {
            DebianVersion::Bullseye => "main,contrib,non-free",
            DebianVersion::Bookworm | DebianVersion::Trixie => "main,contrib,non-free-firmware",
        }
    }

    const fn version_bootstrap_packages(self) -> &'static [&'static str] {
        match self {
            DebianVersion::Bullseye => &["systemd"],
            DebianVersion::Bookworm | DebianVersion::Trixie => &["systemd-resolved"],
        }
    }

    fn bootstrap_packages(self) -> Vec<&'static str> {
        DEBIAN_COMMON_BOOTSTRAP_PACKAGES
            .iter()
            .copied()
            .chain(self.version_bootstrap_packages().iter().copied())
            .collect()
    }
}

impl Default for DebianVersion {
    fn default() -> Self {
        DebianVersion::Bookworm
    }
}

impl fmt::Display for DebianVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DebianVersion {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "bookworm" => Ok(DebianVersion::Bookworm),
            "bullseye" => Ok(DebianVersion::Bullseye),
            "trixie" => Ok(DebianVersion::Trixie),
            other => Err(format!(
                "invalid Debian version {other:?}; expected one of: bookworm, bullseye, trixie"
            )),
        }
    }
}

/// Debian via two-stage debootstrap + initramfs-tools.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Debian {
    version: DebianVersion,
}

#[allow(non_upper_case_globals)]
pub const Debian: Debian = Debian::new(DebianVersion::Bookworm);

impl Debian {
    pub const fn new(version: DebianVersion) -> Self {
        Self { version }
    }

    pub const fn version(self) -> DebianVersion {
        self.version
    }
}

impl Default for Debian {
    fn default() -> Self {
        Self::new(DebianVersion::default())
    }
}

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
        let components = format!("--components={}", self.version.components());
        let include = format!("--include={}", self.version.bootstrap_packages().join(","));
        let status = Command::new("fakeroot")
            .args([
                "debootstrap",
                "--foreign",
                &include,
                &components,
                "--arch=amd64",
                self.version.as_str(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debian_version_defaults_to_bookworm() {
        assert_eq!(DebianVersion::default(), DebianVersion::Bookworm);
        assert_eq!(Debian::default().version(), DebianVersion::Bookworm);
        assert_eq!(Debian.version(), DebianVersion::Bookworm);
    }

    #[test]
    fn debian_version_parses_supported_codenames() {
        assert_eq!(
            "bookworm".parse::<DebianVersion>().unwrap(),
            DebianVersion::Bookworm
        );
        assert_eq!(
            "bullseye".parse::<DebianVersion>().unwrap(),
            DebianVersion::Bullseye
        );
        assert_eq!(
            "trixie".parse::<DebianVersion>().unwrap(),
            DebianVersion::Trixie
        );
    }

    #[test]
    fn debian_version_displays_as_codename() {
        assert_eq!(DebianVersion::Bookworm.to_string(), "bookworm");
        assert_eq!(DebianVersion::Bullseye.to_string(), "bullseye");
        assert_eq!(DebianVersion::Trixie.to_string(), "trixie");
    }

    #[test]
    fn debian_version_rejects_unknown_codename() {
        assert!("stretch".parse::<DebianVersion>().is_err());
    }

    #[test]
    fn debian_proot_binds_are_empty_for_all_versions() {
        assert_eq!(
            Debian::new(DebianVersion::Bookworm).proot_binds(),
            &[] as &[&str]
        );
        assert_eq!(
            Debian::new(DebianVersion::Bullseye).proot_binds(),
            &[] as &[&str]
        );
        assert_eq!(
            Debian::new(DebianVersion::Trixie).proot_binds(),
            &[] as &[&str]
        );
    }

    #[test]
    fn debian_script_has_no_proc_mount_check_or_awk_dependency() {
        let script = Debian::new(DebianVersion::Trixie).install_and_kernel_script();
        assert!(script.starts_with("/debootstrap/debootstrap --second-stage"));
        assert!(!script.contains("mountinfo"));
        assert!(!script.contains("requires /proc mounted inside proot"));
        assert!(!script.contains("awk"));
    }

    #[test]
    fn debian_bootstrap_packages_vary_by_version() {
        assert!(DEBIAN_COMMON_BOOTSTRAP_PACKAGES.contains(&"systemd-sysv"));
        assert!(!DEBIAN_COMMON_BOOTSTRAP_PACKAGES.contains(&"systemd"));
        assert!(!DEBIAN_COMMON_BOOTSTRAP_PACKAGES.contains(&"systemd-resolved"));

        let bullseye = DebianVersion::Bullseye.bootstrap_packages();
        assert!(bullseye.contains(&"systemd"));
        assert!(!bullseye.contains(&"systemd-resolved"));

        let bookworm = DebianVersion::Bookworm.bootstrap_packages();
        assert!(bookworm.contains(&"systemd-resolved"));
        assert!(!bookworm.contains(&"systemd"));

        let trixie = DebianVersion::Trixie.bootstrap_packages();
        assert!(trixie.contains(&"systemd-resolved"));
        assert!(!trixie.contains(&"systemd"));
    }

    #[test]
    fn alpine_uses_pinned_release_and_busybox_shell() {
        assert_eq!(ALPINE_VERSION, "3.22.1");
        assert!(ALPINE_MINIROOTFS_URL.contains("/v3.22/"));
        assert!(ALPINE_MINIROOTFS_URL.contains("3.22.1"));
        assert_eq!(ALPINE_MINIROOTFS_SHA256.len(), 64);
        assert_eq!(Alpine.proot_shell(), "/bin/sh");
        assert_eq!(Alpine.proot_binds(), &["/dev", "/proc", "/sys"]);
    }

    #[test]
    fn alpine_script_installs_lts_kernel_and_uses_openrc() {
        let install = Alpine.install_and_kernel_script();
        let system = Alpine.system_config_script();

        assert!(install.contains("apk add --no-cache"));
        assert!(install.contains("linux-lts"));
        assert!(install.contains("linux-firmware-none"));
        assert!(install.contains("mkinitfs"));
        assert!(install.contains("openssh-server-common-openrc"));
        assert!(install.contains("mv /boot/vmlinuz-lts \"/boot/vmlinuz-$KVER\""));
        assert!(install.contains("mv /boot/config-lts \"/boot/config-$KVER\""));
        assert!(install.contains("-o \"/boot/initramfs-$KVER\" \"$KVER\""));
        assert!(system.contains("rc-update add networking boot"));
        assert!(system.contains("rc-update add modules boot"));
        assert!(system.contains("rc-update add sshd default"));
        assert!(system.contains("igb"));
        assert!(system.contains("vmxnet3"));
        assert!(system.contains("seq 0 13"));
        assert!(!system.contains("systemctl"));
    }

    #[test]
    fn arch_script_uses_full_kernel_release_for_boot_files() {
        let install = Arch.install_and_kernel_script();

        assert!(install.contains("/boot/vmlinuz-$KVER"));
        assert!(install.contains("/boot/initramfs-$KVER.img"));
        assert!(install.contains("rm -f /boot/vmlinuz-linux /boot/initramfs-linux.img"));
    }
}

/// Alpine Linux 3.22.1 via its x86_64 minirootfs + apk + mkinitfs.
pub struct Alpine;

impl Distro for Alpine {
    fn name(&self) -> &str {
        "alpine"
    }

    fn ssh_service(&self) -> &str {
        "sshd"
    }

    fn proot_shell(&self) -> &str {
        "/bin/sh"
    }

    fn extra_binaries(&self) -> &[&str] {
        &["curl", "bsdtar"]
    }

    fn kernel_prefix(&self) -> &str {
        "vmlinuz-"
    }

    fn initramfs_prefix(&self) -> &str {
        "initramfs-"
    }

    fn proot_binds(&self) -> &[&str] {
        &["/dev", "/proc", "/sys"]
    }

    fn bootstrap(&self, rootfs: &Path) -> Result<()> {
        let rootfs_str = rootfs.to_str().context("rootfs path is not valid UTF-8")?;
        let tarball = std::env::temp_dir().join(format!(
            "qoc-alpine-minirootfs-{ALPINE_VERSION}-x86_64-{}.tar.gz",
            process::id()
        ));

        let result = (|| -> Result<()> {
            let status = Command::new("curl")
                .args(["-fL", "-o"])
                .arg(&tarball)
                .arg(ALPINE_MINIROOTFS_URL)
                .status()
                .context("failed to spawn curl")?;
            if !status.success() {
                bail!("downloading Alpine {ALPINE_VERSION} minirootfs failed");
            }

            verify_sha256(&tarball, ALPINE_MINIROOTFS_SHA256)?;

            fs::create_dir_all(rootfs)
                .with_context(|| format!("failed to create {}", rootfs.display()))?;

            let status = Command::new("bsdtar")
                .arg("-x")
                .arg("-f")
                .arg(&tarball)
                .args(["-C", rootfs_str])
                .status()
                .context("failed to spawn bsdtar")?;
            if !status.success() {
                bail!("extracting Alpine {ALPINE_VERSION} minirootfs failed");
            }
            Ok(())
        })();

        let _ = fs::remove_file(&tarball);
        result
    }

    fn install_and_kernel_script(&self) -> String {
        r#"echo 'nameserver 1.1.1.1' > /etc/resolv.conf
apk add --no-cache alpine-base linux-firmware-none linux-lts mkinitfs openssh-server openssh-server-common-openrc ifupdown-ng ethtool
KVER=$(find /lib/modules -mindepth 1 -maxdepth 1 -type d | head -n 1)
KVER=${KVER##*/}
test -n "$KVER"
echo KVER: "$KVER"
test -f /boot/vmlinuz-lts
mv /boot/vmlinuz-lts "/boot/vmlinuz-$KVER"
if [ -f /boot/config-lts ]; then
    mv /boot/config-lts "/boot/config-$KVER"
fi
rm -f /boot/initramfs-lts
mkinitfs -c /etc/mkinitfs/mkinitfs.conf -o "/boot/initramfs-$KVER" "$KVER""#
            .to_string()
    }

    fn system_config_script(&self) -> String {
        r#"printf 'virtio_net\nigb\ne1000\n8139cp\n8139too\nvmxnet3\ne100\nne2k_pci\npcnet32\n' > /etc/modules
printf 'auto lo\niface lo inet loopback\n' > /etc/network/interfaces
for i in $(seq 0 13); do
    printf 'auto eth%s\niface eth%s inet dhcp\n' "$i" "$i" >> /etc/network/interfaces
done
rc-update add modules boot
rc-update add networking boot
rc-update add sshd default"#
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
        "initramfs-"
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
            .args([
                "-x",
                "--strip-components=1",
                "-f",
                tarball,
                "-C",
                rootfs_str,
            ])
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
cp -v "/usr/lib/modules/$KVER/vmlinuz" "/boot/vmlinuz-$KVER"
sed -i 's/^MODULES=.*/MODULES=(virtio virtio_pci virtiofs)/' /etc/mkinitcpio.conf
grep virtio /etc/mkinitcpio.conf
depmod $KVER
mkinitcpio -k "$KVER" -c /etc/mkinitcpio.conf -g "/boot/initramfs-$KVER.img"
rm -f /boot/vmlinuz-linux /boot/initramfs-linux.img
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
