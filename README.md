# qoc

Spinning up a VM usually means fighting `sudo`, editing `/etc/sudoers`, or convincing your admin to whitelist `qemu-system-*` — just to boot something you'll throw away in an hour. `qoc` skips all of that: create and run a full Debian, Arch, or Alpine VM as a plain user, no elevated privileges required.

Inspired by [virtme-ng](https://github.com/arighi/virtme-ng).

`qoc create` builds a rootfs on the host using `proot` plus a distro bootstrap tool or rootfs tarball. `qoc run` boots it inside QEMU (KVM) with the rootfs exposed via virtiofs, waits for SSH, and leaves you with a live shell target. The SSH host port is picked automatically so multiple VMs can run side by side.

Your `~/.ssh/id_*.pub` key is injected during create, so passwordless SSH works the moment the VM is up.

## Supported distros

| `--distro` | Base | Notes |
|---|---|---|
| `debian` | Bookworm (amd64) by default | two-stage `debootstrap`; `--debian-version` supports `bookworm`, `bullseye`, `trixie` |
| `arch` | latest rolling | bootstrap tarball + `pacman` |
| `alpine` | 3.22.1 (x86_64) | pinned minirootfs + `apk`; `linux-lts` follows the v3.22 repositories; firmware omitted for emulated hardware |

## Dependencies

| Command | Needed for |
|---|---|
| `proot` | rootfs configuration without root |
| `fakeroot` + `debootstrap` | Debian create |
| `curl` + `bsdtar` | Arch or Alpine create |
| `virtiofsd` | run (any distro) |
| `qemu-system-x86_64` | run (any distro) |

## Build

```sh
cargo build --release
# binary: target/release/qoc
```

## Examples

### Debian VM

```sh
# 1. Create the rootfs (takes a few minutes)
qoc create --rootfs ~/vms/debian-test --distro debian

# Or choose a supported Debian release explicitly
qoc create --rootfs ~/vms/debian-trixie --distro debian --debian-version trixie

# 2. Boot it (distro is auto-detected)
qoc run --rootfs ~/vms/debian-test

# VM is up — port is printed in the "VM is up" line, e.g.:
ssh -p <port> root@localhost
```

### Arch Linux VM with multiple NICs

```sh
# 1. Create
qoc create --rootfs ~/vms/arch-net --distro arch

# 2. Boot with 4 emulated NICs (virtio-net, igb, e1000, rtl8139)
qoc run --rootfs ~/vms/arch-net --nr-network-cards 4

# Inspect all interfaces from the host (use the port printed at startup)
ssh -p <port> -o StrictHostKeyChecking=no root@localhost ip -br a
```

### Alpine Linux VM

```sh
# 1. Create the pinned Alpine 3.22.1 rootfs
qoc create --rootfs ~/vms/alpine-test --distro alpine

# 2. Boot it (the paired boot-file selector is "lts")
qoc run --rootfs ~/vms/alpine-test

# Inspect the Alpine release and installed v3.22 linux-lts kernel
ssh -p <port> -o StrictHostKeyChecking=no root@localhost \
  'cat /etc/alpine-release; uname -r'
```

Each NIC gets its own `/24` subnet starting at `10.0.2.0/24`; the first card also carries the SSH forward to `guest:22` on an automatically chosen host port.

## Options

```
qoc create
  -r, --rootfs <PATH>              destination directory (must not exist)
      --distro <DISTRO>            debian | arch | alpine
      --debian-version <VERSION>   bookworm | bullseye | trixie (debian only; default: bookworm)

qoc run
  -r, --rootfs <PATH>     directory created by 'create'
  -n, --nr-network-cards  emulated NICs 1–14 (default: 1)
      --show-log          print virtiofsd and QEMU output
```

Press **Ctrl-C** to shut the VM down cleanly.
