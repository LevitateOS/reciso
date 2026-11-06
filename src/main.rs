//! reciso - Create bootable UEFI ISOs.
//!
//! Creates ISOs with UKI (Unified Kernel Image) boot using systemd-boot.

use anyhow::{bail, Result};
use clap::Parser;
use std::path::PathBuf;

use reciso::{create_iso, IsoConfig, LivePayloadLayout, UkiSource};
use recuki::OsReleaseArgs;

#[derive(Parser, Debug)]
#[command(
    name = "reciso",
    about = "Create bootable UEFI ISOs from kernel + initramfs + rootfs",
    version,
    after_help = "\
EXAMPLES:
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

REQUIREMENTS:
    - systemd-boot (dnf install systemd-boot)
    - xorriso (dnf install xorriso)
    - mtools (dnf install mtools)
    - ukify (dnf install systemd-ukify) - if building UKIs
"
)]
struct Args {
    /// Path to kernel image (vmlinuz)
    #[arg(short = 'k', long = "kernel")]
    kernel: PathBuf,

    /// Path to initramfs image
    #[arg(short = 'i', long = "initrd")]
    initrd: PathBuf,

    /// Path to rootfs image (EROFS)
    #[arg(short = 'r', long = "rootfs")]
    rootfs: PathBuf,

    /// ISO volume label
    #[arg(short = 'l', long = "label")]
    label: String,

    /// Output ISO path
    #[arg(short = 'o', long = "output")]
    output: PathBuf,

    /// Prebuilt UKI files to include (can be specified multiple times)
    #[arg(long = "uki")]
    ukis: Vec<PathBuf>,

    /// Build UKIs inline. Format: "name:extra_cmdline:filename"
    /// Example: "Emergency:emergency:myos-emergency.efi"
    #[arg(long = "build-uki")]
    build_ukis: Vec<String>,

    /// Add an extra file to the ISO. Format: "src:dst"
    /// Example: "/tmp/initramfs-installed.img:boot/initramfs-installed.img"
    #[arg(long = "extra-file")]
    extra_files: Vec<String>,

    /// Live overlay payload image (EROFS) to include
    #[arg(long = "overlay-image")]
    overlay_image: Option<PathBuf>,

    #[command(flatten)]
    os: OsReleaseArgs,

    /// Skip checksum generation
    #[arg(long = "no-checksum")]
    no_checksum: bool,

    /// Layout for live payloads (`iso-files` or `appended-partitions`)
    #[arg(
        long = "live-payload-layout",
        default_value = "appended-partitions",
        value_enum,
        value_name = "LAYOUT"
    )]
    live_payload_layout: LivePayloadLayout,

    /// systemd-boot UEFI binary (default: the build host's)
    #[arg(long = "systemd-boot", default_value = reciso::DEFAULT_SYSTEMD_BOOT_EFI)]
    systemd_boot: PathBuf,

    /// Quiet mode - only print errors
    #[arg(short = 'q', long = "quiet")]
    quiet: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let mut config = IsoConfig::new(
        &args.kernel,
        &args.initrd,
        &args.rootfs,
        &args.label,
        &args.output,
    );

    for uki in &args.ukis {
        config.ukis.push(UkiSource::Prebuilt(uki.clone()));
    }

    for spec in &args.build_ukis {
        // name:cmdline:filename — the name ends at the first ':' and the file
        // name starts after the last, so the command line may contain ':'.
        let parts: Option<(&str, &str, &str)> = spec.split_once(':').and_then(|(name, rest)| {
            rest.rsplit_once(':').map(|(cmdline, file)| (name, cmdline, file))
        });
        let Some((name, cmdline, filename)) = parts else {
            bail!(
                "Invalid --build-uki format: '{}'\n\
                 Expected: 'name:extra_cmdline:filename'\n\
                 Example: 'Emergency:emergency:myos-emergency.efi'",
                spec
            );
        };
        config.ukis.push(UkiSource::Build {
            name: name.to_string(),
            extra_cmdline: cmdline.to_string(),
            filename: filename.to_string(),
        });
    }

    for spec in &args.extra_files {
        let parts: Vec<&str> = spec.splitn(2, ':').collect();
        if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
            bail!(
                "Invalid --extra-file format: '{}'\n\
                 Expected: 'src:dst'\n\
                 Example: '/tmp/initramfs-installed.img:boot/initramfs-installed.img'",
                spec
            );
        }
        config
            .extra_files
            .push((PathBuf::from(parts[0]), parts[1].to_string()));
    }

    if let Some(overlay_image) = args.overlay_image {
        config.overlay_image = Some(overlay_image);
    }

    config.os_release = args.os.os_release();

    if args.no_checksum {
        config.generate_checksum = false;
    }

    config.live_payload_layout = args.live_payload_layout;
    config.systemd_boot = args.systemd_boot.clone();

    if !args.quiet {
        println!("Creating ISO: {}", args.output.display());
        println!("  Kernel: {}", args.kernel.display());
        println!("  Initrd: {}", args.initrd.display());
        println!("  Rootfs: {}", args.rootfs.display());
        println!("  Label: {}", args.label);
        if let Some(ref overlay_image) = config.overlay_image {
            println!("  Overlay image: {}", overlay_image.display());
        }
        if !config.extra_files.is_empty() {
            println!("  Extra files: {}", config.extra_files.len());
        }
        if !config.ukis.is_empty() {
            println!("  UKIs: {}", config.ukis.len());
        }
        if let Some(ref os) = config.os_release {
            println!("  OS: {} {} ({})", os.name, os.version, os.id);
        }
        println!();
    }

    let quiet = args.quiet;
    create_iso(&config, &|message: &str| {
        if !quiet {
            println!("{message}");
        }
    })?;

    if !args.quiet {
        println!("\nDone.");
    }

    Ok(())
}
