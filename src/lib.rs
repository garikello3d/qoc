use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use regex::Regex;

mod distro;
pub use distro::{Arch, Debian, Distro};

const KERNEL_BASENAMES: &[&str] = &["vmlinuz", "bzImage", "kernel", "linux"];
const INITRD_BASENAMES: &[&str] = &["initramfs", "initrd"];
const IMAGE_EXTENSIONS: &[&str] = &[".img", ".gz", ".zst", ".xz"];

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

pub struct VmInfo {
    pub kernel_version: Option<String>,
    pub upstream_version: Option<String>,
    pub kernel_config: Option<String>,
}

pub struct SshOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

/// Handle to a running VM. Dropping this terminates the VM.
pub struct VmHandle {
    /// Host-side SSH port forwarded to guest port 22.
    pub ssh_port: u16,
    qemu_pid: libc::pid_t,
    info_rx: mpsc::Receiver<Result<VmInfo>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl VmHandle {
    pub fn qemu_pid(&self) -> libc::pid_t {
        self.qemu_pid
    }

    /// Block until the VM is up and `VmInfo` has been collected.
    /// The VM keeps running after this returns.
    pub fn wait_for_info(&self) -> Result<VmInfo> {
        self.info_rx
            .recv()
            .context("VM background thread exited before sending VmInfo")?
    }

    /// Block until the VM exits on its own (guest shutdown, etc.).
    pub fn wait(mut self) -> Result<()> {
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
        Ok(())
    }

    /// Run an arbitrary command on the guest and return its output.
    /// Call [`VmHandle::wait_for_info`] first to ensure the VM is up.
    pub fn ssh(&self, remote_args: &[&str]) -> Result<SshOutput> {
        let out = ssh_exec(self.ssh_port, remote_args)
            .context("failed to spawn ssh")?;
        Ok(SshOutput {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            success: out.status.success(),
        })
    }

    /// Send SIGTERM to QEMU and wait for full cleanup.
    pub fn stop(mut self) -> Result<()> {
        unsafe { libc::kill(self.qemu_pid, libc::SIGTERM) };
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
        Ok(())
    }
}

impl Drop for VmHandle {
    fn drop(&mut self) {
        unsafe { libc::kill(self.qemu_pid, libc::SIGTERM) };
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
    }
}

pub fn create(distro: &dyn Distro, rootfs: PathBuf) -> Result<Vec<String>> {
    let mut binaries = vec!["proot"];
    binaries.extend_from_slice(distro.extra_binaries());
    for binary in &binaries {
        if !binary_in_path(binary) {
            bail!("required binary not found in PATH: {binary}");
        }
    }

    if rootfs.exists() {
        bail!("rootfs path already exists: {}", rootfs.display());
    }

    println!("creating {} rootfs at {}", distro.name(), rootfs.display());

    distro.bootstrap(&rootfs)?;

    run_proot(&rootfs, distro.proot_binds(), &distro.configure_script())?;

    install_ssh_key(&rootfs).context("failed to install SSH public key")?;

    list_kernels(&rootfs)
}

/// List kernel versions in `rootfs/boot/` that have exactly one paired kernel image and initrd.
pub fn list_kernels(rootfs: &Path) -> Result<Vec<String>> {
    let boot_dir = rootfs.join("boot");
    let mut kernel_counts: HashMap<String, usize> = HashMap::new();
    let mut initrd_counts: HashMap<String, usize> = HashMap::new();

    for entry in fs::read_dir(&boot_dir)
        .with_context(|| format!("failed to read {}", boot_dir.display()))?
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some((basename, version)) = split_boot_filename(name) else { continue };
        if contains_any(basename, KERNEL_BASENAMES) {
            *kernel_counts.entry(version.to_string()).or_insert(0) += 1;
        } else if contains_any(basename, INITRD_BASENAMES) {
            *initrd_counts.entry(version.to_string()).or_insert(0) += 1;
        }
    }

    let mut result: Vec<String> = kernel_counts
        .iter()
        .filter(|(ver, &count)| count == 1 && initrd_counts.get(ver.as_str()) == Some(&1))
        .map(|(ver, _)| ver.clone())
        .collect();
    result.sort();
    Ok(result)
}

/// Start a VM in the background and return immediately with a [`VmHandle`].
///
/// The caller must use [`VmHandle::wait_for_info`] to learn when the VM is
/// reachable and to obtain [`VmInfo`].  The VM runs until the handle is
/// dropped, [`VmHandle::stop`] is called, or the guest shuts itself down.
pub fn start(
    rootfs: PathBuf,
    nr_network_cards: usize,
    show_log: bool,
    kernel_ver: &str,
) -> Result<(VmHandle, String)> {
    for binary in ["virtiofsd", "qemu-system-x86_64"] {
        if !binary_in_path(binary) {
            bail!("required binary not found in PATH: {binary}");
        }
    }

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
    let boot_dir = rootfs.join("boot");
    let vmlinuz = find_boot_by_version(&boot_dir, KERNEL_BASENAMES, kernel_ver)?;
    let initrd  = find_boot_by_version(&boot_dir, INITRD_BASENAMES, kernel_ver)?;
    let vmlinuz_str = vmlinuz.to_str().context("vmlinuz path is not valid UTF-8")?;
    let initrd_str = initrd.to_str().context("initrd path is not valid UTF-8")?;

    let log_stdio = || if show_log { Stdio::inherit() } else { Stdio::null() };

    let ssh_port = find_free_port().context("failed to find a free SSH host port")?;
    let socket_path = format!("/tmp/vhost-fs-{}.sock", process::id());

    let mut virtiofsd_child = Command::new("virtiofsd")
        .args([
            &format!("--socket-path={socket_path}"),
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
        "-chardev".into(), format!("socket,id=char0,path={socket_path}"),
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
            format!("user,id={net_id},net={subnet},hostfwd=tcp::{ssh_port}-:22")
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
    println!("started qemu with {nr_network_cards} network card(s), chosen kernel {kernel_ver}");

    let qemu_pid = qemu.id() as libc::pid_t;
    let (info_tx, info_rx) = mpsc::channel::<Result<VmInfo>>();

    let thread = thread::spawn(move || {
        // Drain QEMU stdout in a nested thread; print only when show_log is set.
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

        let mut proc_version: Option<String> = None;
        for attempt in 1..=MAX_SSH_ATTEMPTS {
            if show_log {
                println!("ssh attempt {attempt}/{MAX_SSH_ATTEMPTS}");
            }
            let out = match ssh_exec(ssh_port, &["cat", "/proc/version"]) {
                Ok(o) => o,
                Err(e) => {
                    let _ = info_tx.send(Err(anyhow::anyhow!("failed to spawn ssh: {e}")));
                    return;
                }
            };
            if out.status.success() {
                proc_version = Some(String::from_utf8_lossy(&out.stdout).into_owned());
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }

        if proc_version.is_none() {
            unsafe { libc::kill(qemu_pid, libc::SIGTERM) };
            let _ = qemu.wait();
            let _ = virtiofsd_child.wait();
            let _ = info_tx.send(Err(anyhow::anyhow!(
                "VM did not become reachable after {MAX_SSH_ATTEMPTS} SSH attempts"
            )));
            return;
        }

        let kver_re = Regex::new(r"Linux version ([A-Za-z0-9._-]+) \(").unwrap();
        let kernel_version: Option<String> = proc_version
            .as_deref()
            .and_then(|s| kver_re.captures(s))
            .map(|c| c[1].to_string());

        // Debian embeds the upstream stable version in a trailing "Debian X.Y.Z-N (" token.
        // For other distros (e.g. Arch), strip the distro suffix from kernel_version directly.
        let debian_re = Regex::new(r"\bDebian\s+([0-9]+\.[0-9]+\.[0-9]+)-[0-9]+\s+\(").unwrap();
        let numeric_re = Regex::new(r"^([0-9]+\.[0-9]+\.[0-9]+)").unwrap();
        let upstream_version: Option<String> = proc_version
            .as_deref()
            .and_then(|s| debian_re.captures(s))
            .map(|c| c[1].to_string())
            .or_else(|| {
                kernel_version.as_deref()
                    .and_then(|v| numeric_re.captures(v))
                    .map(|c| c[1].to_string())
            });

        let kernel_config = fetch_kernel_config(ssh_port, kernel_version.as_deref());

        println!("VM is up — connect with: ssh -p {ssh_port} root@localhost");
        match (kernel_version.as_deref(), upstream_version.as_deref()) {
            (Some(kv), Some(uv)) => println!("active kernel version: {kv}  (upstream: {uv})"),
            (Some(kv), None)     => println!("active kernel version: {kv}"),
            _                    => println!("active kernel version: unknown"),
        }
        println!("kernel config: {}", if kernel_config.is_some() { "extracted" } else { "not available" });

        // Deliver VmInfo to the caller; channel is done after this send.
        let _ = info_tx.send(Ok(VmInfo { kernel_version, upstream_version, kernel_config }));

        // Keep running until QEMU exits (guest shutdown or external signal).
        let _ = qemu.wait();
        let _ = virtiofsd_child.wait();
    });

    Ok((VmHandle { ssh_port, qemu_pid, info_rx, thread: Some(thread) }, kernel_ver.to_string()))
}

pub fn detect_distro(rootfs: &Path) -> Result<Box<dyn Distro>> {
    let os_release = rootfs.join("etc/os-release");
    let content = fs::read_to_string(&os_release)
        .with_context(|| format!("failed to read {}", os_release.display()))?;

    let id = content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let (key, val) = line.split_once('=')?;
            if key.trim() == "ID" {
                Some(val.trim().trim_matches('"').trim_matches('\'').to_string())
            } else {
                None
            }
        })
        .next()
        .with_context(|| format!("no ID field found in {}", os_release.display()))?;

    match id.as_str() {
        "debian" => Ok(Box::new(Debian)),
        "arch" => Ok(Box::new(Arch)),
        other => bail!("unrecognised distro ID {:?} in {}", other, os_release.display()),
    }
}

// Runs `script` inside the rootfs via proot acting as root. The shared preamble
// makes the script fail fast (`set -e`) and fixes PATH. `binds` are passed as
// `-b <path>`; they are per-distro because Debian's debootstrap second stage
// aborts proot (compare_paths2 assertion) when /dev,/proc,/sys are bound.
fn run_proot(rootfs: &Path, binds: &[&str], script: &str) -> Result<()> {
    let rootfs_str = rootfs.to_str().context("rootfs path is not valid UTF-8")?;
    let full = format!("set -e\nexport PATH=/usr/bin:/usr/sbin:/bin:/sbin\n{script}");

    let mut args: Vec<&str> = Vec::new();
    for bind in binds {
        args.push("-b");
        args.push(bind);
    }
    args.extend(["-0", "-r", rootfs_str, "/bin/bash", "-c", &full]);

    let status = Command::new("proot")
        .args(&args)
        .status()
        .context("failed to spawn proot")?;

    if !status.success() {
        bail!("proot configuration stage failed");
    }
    Ok(())
}

// Like run_proot but captures stdout+stderr instead of inheriting them.
// Returns combined output so callers can search both streams with a single regex.
fn run_proot_capture(rootfs: &Path, binds: &[&str], script: &str) -> Result<String> {
    let rootfs_str = rootfs.to_str().context("rootfs path is not valid UTF-8")?;
    let full = format!("set -e\nexport PATH=/usr/bin:/usr/sbin:/bin:/sbin\n{script}");

    let mut args: Vec<&str> = Vec::new();
    for bind in binds {
        args.push("-b");
        args.push(bind);
    }
    args.extend(["-0", "-r", rootfs_str, "/bin/bash", "-c", &full]);

    let out = Command::new("proot")
        .args(&args)
        .output()
        .context("failed to spawn proot")?;

    if !out.status.success() {
        bail!(
            "proot script failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

pub fn make_initrd(rootfs: &Path, kernel_version: &str) -> Result<PathBuf> {
    let distro = detect_distro(rootfs)?;

    match distro.name() {
        "arch" => {
            let out = format!("/boot/initramfs-{kernel_version}.img");
            let script = format!(
                "mkinitcpio -k {kernel_version} -c /etc/mkinitcpio.conf -g {out}"
            );
            run_proot(rootfs, distro.proot_binds(), &script)?;
            Ok(PathBuf::from(out))
        }
        "debian" => {
            let script = format!("update-initramfs -c -u -k {kernel_version}");
            let captured = run_proot_capture(rootfs, distro.proot_binds(), &script)?;
            let re = Regex::new(r"update-initramfs: Generating (/boot/\S+)").unwrap();
            re.captures(&captured)
                .map(|c| PathBuf::from(&c[1]))
                .context("update-initramfs succeeded but generated path not found in output")
        }
        other => bail!("make_initrd: unsupported distro '{other}'"),
    }
}

fn ssh_exec(port: u16, remote_args: &[&str]) -> std::io::Result<std::process::Output> {
    let port_str = port.to_string();
    let mut args = vec![
        "-p", &port_str,
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "root@localhost",
    ];
    args.extend_from_slice(remote_args);
    Command::new("ssh").args(&args).output()
}

fn fetch_kernel_config(port: u16, kver: Option<&str>) -> Option<String> {
    // Prefer /proc/config.gz if the kernel was built with CONFIG_IKCONFIG_PROC.
    if let Ok(out) = ssh_exec(port, &["test", "-f", "/proc/config.gz"]) {
        if out.status.success() {
            if let Ok(gz) = ssh_exec(port, &["zcat", "/proc/config.gz"]) {
                if gz.status.success() {
                    return Some(String::from_utf8_lossy(&gz.stdout).into_owned());
                }
            }
            return None;
        }
    }

    // Fall back to /boot/config-<kver>.
    let kver = kver?;
    let ls_out = ssh_exec(port, &["ls", "/boot/config-*"]).ok()?;
    if !ls_out.status.success() {
        return None;
    }
    let listing = String::from_utf8_lossy(&ls_out.stdout);
    let path = listing.lines().find(|l| l.contains(kver))?.trim().to_string();
    let cat = ssh_exec(port, &["cat", &path]).ok()?;
    if cat.status.success() {
        Some(String::from_utf8_lossy(&cat.stdout).into_owned())
    } else {
        None
    }
}

fn find_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("failed to bind TCP listener")?;
    Ok(listener.local_addr().context("failed to get local address")?.port())
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

    // corresponds to the ssh client tool order
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

// Returns (basename, version) by splitting on the first '-' and stripping any trailing
// file extension from the version (e.g. "initramfs-linux.img" → ("initramfs", "linux")).
fn split_boot_filename(name: &str) -> Option<(&str, &str)> {
    let dash = name.find('-')?;
    let basename = &name[..dash];
    let raw = &name[dash + 1..];
    let version = IMAGE_EXTENSIONS.iter().find_map(|ext| raw.strip_suffix(ext)).unwrap_or(raw);
    if version.is_empty() { None } else { Some((basename, version)) }
}

fn contains_any(s: &str, keywords: &[&str]) -> bool {
    keywords.iter().any(|k| s.contains(k))
}

// Scans `boot_dir` for the single file whose basename contains a keyword from `basenames`
// and whose version (right of first '-') equals `ver`. Errors if not exactly one match.
fn find_boot_by_version(boot_dir: &Path, basenames: &[&str], ver: &str) -> Result<PathBuf> {
    let matches: Vec<PathBuf> = fs::read_dir(boot_dir)
        .with_context(|| format!("failed to read {}", boot_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(split_boot_filename)
                .map(|(basename, version)| contains_any(basename, basenames) && version == ver)
                .unwrap_or(false)
        })
        .collect();
    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => bail!("no boot file with version {ver:?} found in {}", boot_dir.display()),
        _ => bail!("multiple boot files with version {ver:?} found in {}", boot_dir.display()),
    }
}
