# qoc

Spinning up a VM usually means fighting `sudo`, editing `/etc/sudoers`, or convincing your admin to whitelist `qemu-system-*` — just to boot something you'll throw away in an hour. `qoc` skips all of that: create and run a full Debian or Arch VM as a plain user, no elevated privileges required.

Inspired by [virtme-ng](https://github.com/arighi/virtme-ng).

`qoc create` builds a rootfs on the host using `proot` + `debootstrap` / `pacstrap`. `qoc run` boots it inside QEMU (KVM) with the rootfs exposed via virtiofs, waits for SSH, and leaves you with a live shell target on port 40022.

Your `~/.ssh/id_*.pub` key is injected during create, so `ssh -p 40022 root@localhost` works without a password the moment the VM is up.

## Supported distros

| `--distro` | Base | Notes |
|---|---|---|
| `debian` | Bookworm (amd64) | two-stage `debootstrap` |
| `arch` | latest rolling | bootstrap tarball + `pacman` |

## Dependencies

| Command | Needed for |
|---|---|
| `proot` | rootfs configuration without root |
| `fakeroot` + `debootstrap` | Debian create |
| `curl` + `bsdtar` | Arch create |
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

# 2. Boot it
qoc run --rootfs ~/vms/debian-test --distro debian

# VM is up — in another terminal:
ssh -p 40022 root@localhost
```

### Arch Linux VM with multiple NICs

```sh
# 1. Create
qoc create --rootfs ~/vms/arch-net --distro arch

# 2. Boot with 4 emulated NICs (virtio-net, igb, e1000, rtl8139)
qoc run --rootfs ~/vms/arch-net --distro arch --nr-network-cards 4

# Inspect all interfaces from the host
ssh -p 40022 -o StrictHostKeyChecking=no root@localhost ip -br a
```

Each NIC gets its own `/24` subnet starting at `10.0.2.0/24`; the first card also carries the SSH forward (`host:40022 → guest:22`).

## Options

```
qoc create
  -r, --rootfs <PATH>     destination directory (must not exist)
      --distro <DISTRO>   debian | arch

qoc run
  -r, --rootfs <PATH>     directory created by 'create'
      --distro <DISTRO>   debian | arch
  -n, --nr-network-cards  emulated NICs 1–14 (default: 1)
      --show-log          print virtiofsd and QEMU output
```

Press **Ctrl-C** to shut the VM down cleanly.
