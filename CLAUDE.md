# CLAUDE.md - reciso

## What is reciso?

Standalone UEFI ISO builder. Creates bootable ISOs from kernel + initramfs + rootfs using systemd-boot and UKIs.

## What Belongs Here

- ISO creation logic
- EFI boot image creation
- xorriso wrapper
- UKI integration (via recuki)

## What Does NOT Belong Here

| Don't put here | Put it in |
|----------------|-----------|
| LevitateOS-specific UKI entries | `leviso/src/artifact/iso.rs` |
| Live overlay creation | `leviso/` |
| Hardware compatibility checks | `leviso/` |
| Boot entry constants | `distro-spec/` |

## Commands

```bash
cargo build --release
cargo test
```

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

## Library Usage

```rust
use reciso::{IsoConfig, UkiSource, create_iso};

let config = IsoConfig::new("vmlinuz", "initramfs.img", "rootfs.erofs", "MYISO", "output.iso")
    .with_os_release("MyOS", "myos", "1.0")
    .with_uki("Normal", "", "myos.efi")
    .with_uki("Emergency", "emergency", "myos-emergency.efi");
create_iso(&config)?;
```

## Requirements

- systemd-boot: `sudo dnf install systemd-boot`
- xorriso: `sudo dnf install xorriso`
- mtools: `sudo dnf install mtools`
- ukify (for building UKIs): `sudo dnf install systemd-ukify`
