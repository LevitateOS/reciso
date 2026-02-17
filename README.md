# reciso

Create bootable UEFI ISOs from kernel + initramfs + rootfs.

Builds ISOs with systemd-boot and UKI support. Outputs a single bootable ISO that works on UEFI systems.

## Status

**Beta.** Works for UEFI boot with EROFS rootfs.

## Usage

```bash
# Basic ISO with auto-generated UKI
reciso -k vmlinuz -i initramfs.img -r rootfs.erofs -l MYISO -o output.iso

# With prebuilt UKIs
reciso -k vmlinuz -i initramfs.img -r rootfs.erofs -l MYISO \
       --uki myos-live.efi --uki myos-emergency.efi -o output.iso

# Build UKIs inline (format: name:extra_cmdline:filename)
reciso -k vmlinuz -i initramfs.img -r rootfs.erofs -l MYISO \
       --build-uki 'Normal::myos.efi' \
       --build-uki 'Emergency:emergency:myos-emergency.efi' \
       --os-name MyOS --os-id myos --os-version 1.0 \
       -o output.iso

# With live overlay image (EROFS)
reciso -k vmlinuz -i initramfs.img -r rootfs.erofs -l MYISO \
       --overlay-image live-overlay.erofs -o output.iso
```

## Options

```
reciso [OPTIONS] -k <KERNEL> -i <INITRD> -r <ROOTFS> -l <LABEL> -o <OUTPUT>

Required:
-k, --kernel <PATH>      Path to kernel (vmlinuz)
-i, --initrd <PATH>      Path to initramfs
-r, --rootfs <PATH>      Path to rootfs image (EROFS)
-l, --label <STRING>     ISO volume label
-o, --output <PATH>      Output ISO path

UKI options:
--uki <PATH>             Add prebuilt UKI (can repeat)
--build-uki <SPEC>       Build UKI inline: 'name:cmdline:filename'

Branding:
--os-name <NAME>         OS name for boot menu
--os-id <ID>             OS ID (lowercase, no spaces)
--os-version <VERSION>   OS version string

Other:
--overlay-image <FILE>   Live overlay payload image (EROFS)
--extra-file <SRC:DEST>  Add extra file to ISO
--checksum               Generate SHA256 checksum
```

## Library Usage

```rust
use reciso::{IsoConfig, UkiSource, create_iso};

let config = IsoConfig::new("vmlinuz", "initramfs.img", "rootfs.erofs", "MYISO", "output.iso")
    .with_os_release("MyOS", "myos", "1.0")
    .with_uki("Normal", "", "myos.efi")
    .with_uki("Emergency", "emergency", "myos-emergency.efi");
create_iso(&config)?;
```

## What It Does

1. Creates EFI boot image with systemd-boot
2. Builds UKIs (or uses prebuilt ones)
3. Assembles ISO with xorriso
4. Optionally generates SHA256 checksum

## What It Does NOT Do

- Create rootfs images (use `mkfs.erofs`)
- Build initramfs (use `recinit`)
- Partition disks
- Support legacy BIOS boot

## Requirements

- systemd-boot: `sudo dnf install systemd-boot`
- xorriso: `sudo dnf install xorriso`
- mtools: `sudo dnf install mtools`
- ukify (for building UKIs): `sudo dnf install systemd-ukify`

## Building

```bash
cargo build --release
```

## License

MIT OR Apache-2.0
