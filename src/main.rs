//! reciso - Create bootable UEFI ISOs.
//!
//! Creates ISOs with UKI (Unified Kernel Image) boot using systemd-boot.

use anyhow::{bail, Result};
use clap::{Parser, ValueEnum};
use std::path::PathBuf;

use reciso::{create_iso, IsoConfig, LivePayloadLayout, UkiSource};

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

    /// ISO volume label (used for boot device detection)
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

    /// OS name for UKI branding (e.g., "LevitateOS")
    #[arg(long = "os-name")]
    os_name: Option<String>,

    /// OS identifier for UKI branding (e.g., "levitateos")
    #[arg(long = "os-id")]
    os_id: Option<String>,

    /// OS version for UKI branding (e.g., "1.0")
    #[arg(long = "os-version")]
    os_version: Option<String>,

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
    live_payload_layout: RecisoPayloadLayoutArg,

    /// Quiet mode - only print errors
    #[arg(short = 'q', long = "quiet")]
    quiet: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Build config
    let mut config = IsoConfig::new(
        &args.kernel,
        &args.initrd,
        &args.rootfs,
        &args.label,
        &args.output,
    );

    // Add prebuilt UKIs
    for uki in &args.ukis {
        config.ukis.push(UkiSource::Prebuilt(uki.clone()));
    }

    // Parse and add build-uki specs
    for spec in &args.build_ukis {
        let parts: Vec<&str> = spec.splitn(3, ':').collect();
        if parts.len() != 3 {
            bail!(
                "Invalid --build-uki format: '{}'\n\
                 Expected: 'name:extra_cmdline:filename'\n\
                 Example: 'Emergency:emergency:myos-emergency.efi'",
                spec
            );
        }
        config.ukis.push(UkiSource::Build {
            name: parts[0].to_string(),
            extra_cmdline: parts[1].to_string(),
            filename: parts[2].to_string(),
        });
    }

    // Parse and add extra files
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

    // Add live overlay payload image
    if let Some(overlay_image) = args.overlay_image {
        config.overlay_image = Some(overlay_image);
    }

    // Add OS branding
    if let (Some(name), Some(id), Some(version)) = (&args.os_name, &args.os_id, &args.os_version) {
        config = config.with_os_release(name, id, version);
    } else if args.os_name.is_some() || args.os_id.is_some() || args.os_version.is_some() {
        eprintln!(
            "Warning: Partial OS release info provided. \
             Need all of --os-name, --os-id, and --os-version for branding."
        );
    }

    // Checksum setting
    if args.no_checksum {
        config.generate_checksum = false;
    }

    config.live_payload_layout = match args.live_payload_layout {
        RecisoPayloadLayoutArg::AppendedPartitions => LivePayloadLayout::AppendedPartitions,
        RecisoPayloadLayoutArg::IsoFiles => LivePayloadLayout::IsoFiles,
    };

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

    create_iso(&config)?;

    if !args.quiet {
        println!("\nDone.");
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum RecisoPayloadLayoutArg {
    /// Keep payloads as files under `live/` in ISO filesystem.
    #[value(name = "iso-files")]
    IsoFiles,
    /// Append payloads as GPT partitions.
    #[value(name = "appended-partitions")]
    AppendedPartitions,
}
