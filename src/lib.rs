use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

const REQUIRED_BINARIES: &[&str] = &[
    "virtiofsd",
    "qemu-system-x86_64",
    "fakeroot",
    "debootstrap",
    "proot",
];

const NIC_MODELS: &[&str] = &[
    "virtio-net-pci",
    "igb",
    "e1000",
    "rtl8139",
    "vmxnet3",
    "i82550",
    "i82551",
    "i82557a",
    "i82558a",
    "i82559a",
    "i82562",
    "i82801",
    "ne2k_pci",
    "pcnet",
];

pub fn create(rootfs: PathBuf) -> Result<()> {
    for binary in REQUIRED_BINARIES {
        if !binary_in_path(binary) {
            bail!("required binary not found in PATH: {binary}");
        }
    }

    if rootfs.exists() {
        bail!("rootfs path already exists: {}", rootfs.display());
    }

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

    // Raw string keeps \n as literal backslash-n for printf format strings inside the chroot.
    let proot_script = r#"export PATH=/usr/bin:/usr/sbin:/bin:/sbin
/debootstrap/debootstrap --second-stage
printf 'virtio\nvirtio_pci\nvirtiofs\n' >> /etc/initramfs-tools/modules
update-initramfs -c -u -k all
echo 'root:1111' | chpasswd
echo 'debian-vm' > /etc/hostname
printf '[Match]\nName=en*\n[Network]\nDHCP=yes\n' > /etc/systemd/network/01-all.network
systemctl enable ssh
systemctl enable systemd-networkd
systemctl enable systemd-resolved
echo 'en_GB.UTF-8 UTF-8' >> /etc/locale.gen
locale-gen"#;

    let status = Command::new("proot")
        .args(["-0", "-r", rootfs_str, "/bin/bash", "-c", proot_script])
        .status()
        .context("failed to spawn proot")?;

    if !status.success() {
        bail!("proot second stage failed");
    }

    install_ssh_key(&rootfs).context("failed to install SSH public key")?;

    Ok(())
}

pub fn run(rootfs: PathBuf, nr_network_cards: usize, show_log: bool) -> Result<()> {
    if nr_network_cards == 0 {
        bail!("nr_network_cards must be at least 1");
    }
    if nr_network_cards > NIC_MODELS.len() {
        bail!(
            "nr_network_cards ({nr_network_cards}) exceeds available NIC models ({})",
            NIC_MODELS.len()
        );
    }

    let rootfs_str = rootfs.to_str().context("rootfs path is not valid UTF-8")?;
    let vmlinuz = find_boot_file(&rootfs, "vmlinuz-")?;
    let initrd = find_boot_file(&rootfs, "initrd.img-")?;
    let vmlinuz_str = vmlinuz.to_str().context("vmlinuz path is not valid UTF-8")?;
    let initrd_str = initrd.to_str().context("initrd path is not valid UTF-8")?;

    let log_stdio = || if show_log { Stdio::inherit() } else { Stdio::null() };

    let mut virtiofsd_child = Command::new("virtiofsd")
        .args([
            "--socket-path=/tmp/vhost-fs.sock",
            &format!("--shared-dir={rootfs_str}"),
            "--uid-map=:0:1000:1:",
            "--uid-map=:1:100000:65535:",
            "--gid-map=:0:1000:1:",
            "--gid-map=:1:100000:65535:",
        ])
        .stdout(log_stdio())
        .stderr(log_stdio())
        .spawn()
        .context("failed to spawn virtiofsd")?;
    println!("started virtiofsd (folder {rootfs_str})");

    thread::sleep(Duration::from_secs(1));

    let mut qemu_args: Vec<String> = vec![
        "-enable-kvm".into(),
        "-cpu".into(), "host".into(),
        "-smp".into(), "2".into(),
        "-m".into(), "2G".into(),
        "-object".into(), "memory-backend-memfd,id=mem,size=2G,share=on".into(),
        "-numa".into(), "node,memdev=mem".into(),
        "-chardev".into(), "socket,id=char0,path=/tmp/vhost-fs.sock".into(),
        "-device".into(), "vhost-user-fs-pci,queue-size=1024,chardev=char0,tag=rootfs".into(),
        "-kernel".into(), vmlinuz_str.into(),
        "-initrd".into(), initrd_str.into(),
        "-append".into(), "root=rootfs rootfstype=virtiofs rw console=ttyS0".into(),
        "-nographic".into(),
    ];

    for i in 0..nr_network_cards {
        let net_id = format!("net{i}");
        let mac = format!("52:54:00:11:22:{i:02x}");
        let subnet = format!("10.0.{}.0/24", i + 2);
        let netdev = if i == 0 {
            format!("user,id={net_id},net={subnet},hostfwd=tcp::40022-:22")
        } else {
            format!("user,id={net_id},net={subnet}")
        };
        qemu_args.extend([
            "-netdev".into(), netdev,
            "-device".into(), format!("{},netdev={net_id},mac={mac}", NIC_MODELS[i]),
        ]);
    }

    let mut qemu = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log_stdio())
        .spawn()
        .context("failed to spawn qemu-system-x86_64")?;
    println!("started qemu with {nr_network_cards} network card(s)");

    // Send SIGTERM to QEMU on Ctrl-C; qemu.wait() below will then return and
    // we proceed to wait for virtiofsd (which shuts down automatically).
    let qemu_pid = qemu.id() as libc::pid_t;
    ctrlc::set_handler(move || {
        unsafe { libc::kill(qemu_pid, libc::SIGTERM) };
    })
    .context("failed to set Ctrl-C handler")?;

    // Drain QEMU stdout in a background thread; print lines only when show_log is set.
    let stdout = qemu.stdout.take().unwrap();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if show_log {
                println!("{line}");
            }
        }
    });

    thread::sleep(Duration::from_secs(1));

    const MAX_SSH_ATTEMPTS: u32 = 10;
    let ssh_args = [
        "-p", "40022",
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "root@localhost",
        "ip", "-br", "a",
    ];

    let mut success = false;
    for attempt in 1..=MAX_SSH_ATTEMPTS {
        if show_log {
            println!("ssh attempt {attempt}/{MAX_SSH_ATTEMPTS}: ssh {}", ssh_args.join(" "));
        }
        let out = Command::new("ssh")
            .args(ssh_args)
            .output()
            .context("failed to spawn ssh")?;
        if out.status.success() {
            println!("VM is up (ssh {})\n{}", ssh_args.join(" "), String::from_utf8_lossy(&out.stdout));
            success = true;
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }

    if !success {
        unsafe { libc::kill(qemu_pid, libc::SIGTERM) };
        qemu.wait().context("waiting for QEMU")?;
        virtiofsd_child.wait().context("waiting for virtiofsd")?;
        bail!("VM did not become reachable after {MAX_SSH_ATTEMPTS} SSH attempts");
    }

    qemu.wait().context("waiting for QEMU")?;
    virtiofsd_child.wait().context("waiting for virtiofsd")?;
    Ok(())
}

fn binary_in_path(name: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file()
            && candidate
                .metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    })
}

fn install_ssh_key(rootfs: &Path) -> Result<()> {
    let home = PathBuf::from(env::var("HOME").context("HOME not set")?);

    let pubkey_path = ["rsa", "ecdsa", "ecdsa_sk", "ed25519", "ed25519_sk"]
        .iter()
        .map(|t| home.join(".ssh").join(format!("id_{t}.pub")))
        .find(|p| p.exists())
        .context("no SSH public key found in ~/.ssh")?;

    let ssh_dir = rootfs.join("root/.ssh");
    fs::create_dir_all(&ssh_dir)?;
    fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700))?;

    let authorized_keys = ssh_dir.join("authorized_keys");
    fs::copy(&pubkey_path, &authorized_keys)
        .with_context(|| format!("failed to copy {} into rootfs", pubkey_path.display()))?;
    fs::set_permissions(&authorized_keys, fs::Permissions::from_mode(0o600))?;

    Ok(())
}

// Sorts alphabetically and picks the last entry, which gives the highest kernel version.
fn find_boot_file(rootfs: &Path, prefix: &str) -> Result<PathBuf> {
    let boot_dir = rootfs.join("boot");
    let mut entries: Vec<PathBuf> = fs::read_dir(&boot_dir)
        .with_context(|| format!("failed to read {}", boot_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(prefix))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();
    entries
        .into_iter()
        .last()
        .with_context(|| format!("no file matching {prefix}* in {}", boot_dir.display()))
}
