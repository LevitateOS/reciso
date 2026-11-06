//! reciso - Bootable ISO creator.
//!
//! Creates UEFI-bootable ISOs from kernel + initramfs + rootfs inputs.
//! Supports UKI (Unified Kernel Image) boot with systemd-boot.
//!
//! # Example
//!
//! ```ignore
//! use reciso::{IsoConfig, create_iso};
//!
//! let config = IsoConfig::new("vmlinuz", "initramfs.img", "rootfs.erofs", "MYISO", "output.iso");
//! create_iso(&config, &|msg| eprintln!("{msg}"))?;
//! ```

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

mod iso;

use distro_spec::shared::{
    LoaderConfig, EFIBOOT_FILENAME, EFI_BOOTLOADER, INITRAMFS_LIVE_ISO_PATH, ISO_EFI_DIR,
    KERNEL_ISO_PATH, LIVE_OVERLAYFS_ISO_PATH, ROOTFS_ISO_PATH,
};
use iso::{create_fat16_image, mcopy, mmd, run_xorriso, setup_iso_structure, AppendedPartition};

use recuki::OsRelease;

/// Default systemd-boot UEFI binary (the build host's); a variant should
/// pass its own rootfs's copy.
pub use distro_spec::shared::SYSTEMD_BOOT_EFI as DEFAULT_SYSTEMD_BOOT_EFI;

#[derive(Debug, Clone)]
pub struct IsoConfig {
    /// Path to kernel image (vmlinuz).
    pub kernel: PathBuf,
    /// Path to initramfs image.
    pub initrd: PathBuf,
    /// Path to rootfs image (EROFS).
    pub rootfs: PathBuf,
    /// ISO volume label; also the `root=LABEL=` on the UKI command line.
    pub label: String,
    pub output: PathBuf,
    /// UKI sources (prebuilt files or build specs).
    pub ukis: Vec<UkiSource>,
    /// Optional live overlay payload image (EROFS).
    pub overlay_image: Option<PathBuf>,
    /// OS release information for UKI branding.
    pub os_release: Option<OsRelease>,
    /// Additional files to copy to ISO root.
    pub extra_files: Vec<(PathBuf, String)>,
    /// Generate SHA512 checksum.
    pub generate_checksum: bool,
    /// How to place rootfs/overlay payloads on boot media.
    pub live_payload_layout: LivePayloadLayout,
    /// systemd-boot UEFI binary to boot the ISO with.
    pub systemd_boot: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LivePayloadLayout {
    /// Keep payloads as files under `live/` in ISO filesystem.
    IsoFiles,
    /// Append payload images as GPT partitions (direct block mounts in initramfs).
    AppendedPartitions,
}

/// Source for a UKI - either prebuilt or to be built.
#[derive(Debug, Clone)]
pub enum UkiSource {
    /// Path to a prebuilt UKI file.
    Prebuilt(PathBuf),
    /// Build a UKI with these parameters.
    Build {
        /// Display name for boot menu.
        name: String,
        /// Extra cmdline parameters (appended to base).
        extra_cmdline: String,
        /// Output filename (e.g., "myos-live.efi").
        filename: String,
    },
}

#[derive(Debug, Clone)]
struct UkiMenuEntry {
    title: String,
    filename: String,
    path: PathBuf,
}

impl IsoConfig {
    pub fn new(
        kernel: impl Into<PathBuf>,
        initrd: impl Into<PathBuf>,
        rootfs: impl Into<PathBuf>,
        label: impl Into<String>,
        output: impl Into<PathBuf>,
    ) -> Self {
        Self {
            kernel: kernel.into(),
            initrd: initrd.into(),
            rootfs: rootfs.into(),
            label: label.into(),
            output: output.into(),
            ukis: Vec::new(),
            overlay_image: None,
            os_release: None,
            extra_files: Vec::new(),
            generate_checksum: true,
            live_payload_layout: LivePayloadLayout::AppendedPartitions,
            systemd_boot: PathBuf::from(DEFAULT_SYSTEMD_BOOT_EFI),
        }
    }
}

/// Create a bootable ISO and return its path.
///
/// The rootfs and overlay images go in as files under `live/` or as appended
/// GPT partitions, per `live_payload_layout`. Everything is built in a private
/// directory next to the output, and the ISO is moved into place only when
/// complete.
///
/// # Errors
///
/// Returns an error if:
/// - Required input files don't exist
/// - The systemd-boot binary is missing
/// - Building a UKI or running xorriso fails
pub fn create_iso(config: &IsoConfig, progress: &dyn Fn(&str)) -> Result<PathBuf> {
    validate_inputs(config)?;

    // Everything is built in a private directory next to the output and
    // removed afterwards; the ISO is moved into place only when complete.
    let output_dir = config
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let work = tempfile::Builder::new()
        .prefix(".reciso-")
        .tempdir_in(&output_dir)
        .with_context(|| format!("creating a work directory in {}", output_dir.display()))?;
    let iso_root = work.path().join("iso-root");

    progress("Setting up ISO structure...");
    setup_iso_structure(&iso_root)?;

    copy_core_files(config, &iso_root, progress)?;

    setup_uefi_boot(config, &iso_root, progress)?;

    for (src, dst) in &config.extra_files {
        let dst_path = iso_root.join(dst);
        if let Some(parent) = dst_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, &dst_path)
            .with_context(|| format!("Failed to copy {} to {}", src.display(), dst))?;
    }

    progress("Creating ISO with xorriso...");
    let mut appended = Vec::new();
    if config.live_payload_layout == LivePayloadLayout::AppendedPartitions {
        appended.push(AppendedPartition {
            index: distro_spec::shared::live_boot::ROOTFS_PAYLOAD_PARTITION,
            type_code: "0x83",
            path: &config.rootfs,
        });
        if let Some(ref overlay_image) = config.overlay_image {
            appended.push(AppendedPartition {
                index: distro_spec::shared::live_boot::OVERLAY_PAYLOAD_PARTITION,
                type_code: "0x83",
                path: overlay_image,
            });
        }
    }
    let built = work.path().join("output.iso");
    run_xorriso(&iso_root, &built, &config.label, EFIBOOT_FILENAME, &appended)?;
    fs::rename(&built, &config.output)
        .with_context(|| format!("moving the ISO to {}", config.output.display()))?;

    // Checksum the final file, under its final name.
    if config.generate_checksum {
        progress("Generating checksum...");
        let checksum = iso::write_checksum(&config.output)?;
        progress(&format!("  Wrote: {}", checksum.display()));
    }

    progress(&format!("\nISO created: {}", config.output.display()));
    if let Ok(meta) = fs::metadata(&config.output) {
        progress(&format!("  Size: {} MB", meta.len() / 1024 / 1024));
    }

    Ok(config.output.clone())
}

fn validate_inputs(config: &IsoConfig) -> Result<()> {
    if !config.kernel.exists() {
        bail!("Kernel not found: {}", config.kernel.display());
    }
    if !config.initrd.exists() {
        bail!("Initrd not found: {}", config.initrd.display());
    }
    if !config.rootfs.exists() {
        bail!("Rootfs not found: {}", config.rootfs.display());
    }
    if let Some(ref overlay_image) = config.overlay_image {
        if !overlay_image.exists() {
            bail!("Overlay image not found: {}", overlay_image.display());
        }
        if !overlay_image.is_file() {
            bail!(
                "Overlay image path is not a file: {}",
                overlay_image.display()
            );
        }
    }
    for uki in &config.ukis {
        if let UkiSource::Prebuilt(path) = uki {
            if !path.exists() {
                bail!("UKI not found: {}", path.display());
            }
        }
    }

    if !config.systemd_boot.is_file() {
        bail!(
            "systemd-boot not found at {} (pass --systemd-boot)",
            config.systemd_boot.display()
        );
    }

    Ok(())
}

/// Copy kernel and initramfs into the ISO tree, and the payload images too
/// when they are laid out as ISO files.
fn copy_core_files(config: &IsoConfig, iso_root: &Path, progress: &dyn Fn(&str)) -> Result<()> {
    progress("Copying boot files...");

    fs::copy(&config.kernel, iso_root.join(KERNEL_ISO_PATH)).context("Failed to copy kernel")?;

    fs::copy(&config.initrd, iso_root.join(INITRAMFS_LIVE_ISO_PATH))
        .context("Failed to copy initramfs")?;

    if config.live_payload_layout == LivePayloadLayout::IsoFiles {
        progress("Copying rootfs...");
        fs::copy(&config.rootfs, iso_root.join(ROOTFS_ISO_PATH))
            .context("Failed to copy rootfs")?;

        if let Some(ref overlay_image) = config.overlay_image {
            progress("Copying live overlay image...");
            fs::copy(overlay_image, iso_root.join(LIVE_OVERLAYFS_ISO_PATH))
                .context("Failed to copy live overlay image")?;
        }
    } else {
        progress(
            "Live payload layout: appended partitions (rootfs/overlay not copied into ISO filesystem)",
        );
    }

    Ok(())
}

/// Set up UEFI boot with systemd-boot and UKIs.
fn setup_uefi_boot(config: &IsoConfig, iso_root: &Path, progress: &dyn Fn(&str)) -> Result<()> {
    progress("Setting up UEFI boot...");

    let uki_dir = iso_root.join(distro_spec::shared::UKI_EFI_DIR);
    fs::create_dir_all(&uki_dir)?;

    // The live init only reports root=LABEL= (it finds its boot medium by
    // probing); everything else on the command line (consoles, distro
    // switches) comes from the caller.
    let base_cmdline = format!("root=LABEL={}", config.label);

    let mut uki_entries: Vec<UkiMenuEntry> = Vec::new();

    for uki in &config.ukis {
        match uki {
            UkiSource::Prebuilt(path) => {
                let filename = path.file_name().context("UKI has no filename")?;
                let dst = uki_dir.join(filename);
                fs::copy(path, &dst)?;
                let filename_str = filename.to_string_lossy().to_string();
                let title = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| filename_str.clone());
                uki_entries.push(UkiMenuEntry {
                    title,
                    filename: filename_str,
                    path: dst,
                });
            }
            UkiSource::Build {
                name,
                extra_cmdline,
                filename,
            } => {
                let cmdline = if extra_cmdline.is_empty() {
                    base_cmdline.clone()
                } else {
                    format!("{} {}", base_cmdline, extra_cmdline)
                };

                let output = uki_dir.join(filename);
                let mut uki_config =
                    recuki::UkiConfig::new(&config.kernel, &config.initrd, &cmdline, &output);
                uki_config.os_release = config.os_release.clone();

                progress(&format!("  Building UKI: {}", filename));
                recuki::build_uki(&uki_config)?;
                uki_entries.push(UkiMenuEntry {
                    title: name.clone(),
                    filename: filename.clone(),
                    path: output,
                });
            }
        }
    }

    if uki_entries.is_empty() {
        let filename = format!("{}.efi", config.label.to_lowercase());
        let output = uki_dir.join(&filename);
        let mut uki_config =
            recuki::UkiConfig::new(&config.kernel, &config.initrd, &base_cmdline, &output);
        uki_config.os_release = config.os_release.clone();

        progress(&format!("  Building UKI: {}", filename));
        recuki::build_uki(&uki_config)?;
        let title = config
            .os_release
            .as_ref()
            .map(|os| format!("{} {}", os.name, os.version))
            .unwrap_or_else(|| config.label.clone());
        uki_entries.push(UkiMenuEntry {
            title,
            filename,
            path: output,
        });
    }

    fs::copy(&config.systemd_boot, iso_root.join(ISO_EFI_DIR).join(EFI_BOOTLOADER))?;

    let loader_dir = iso_root.join("loader");
    fs::create_dir_all(&loader_dir)?;

    let default_entry = uki_entries
        .first()
        .map(|entry| entry_stem(&entry.filename).to_string())
        .unwrap_or_else(|| "default".to_string());
    let loader_config = LoaderConfig::with_defaults("default")
        .with_default_entry(default_entry)
        .with_timeout(5)
        .with_console_mode("max");

    fs::write(
        loader_dir.join("loader.conf"),
        loader_config.to_loader_conf(),
    )?;
    write_loader_entries(&loader_dir, &uki_entries)?;

    build_efi_boot_image(iso_root, &uki_entries, progress)?;

    Ok(())
}

/// Create the EFI system partition image (systemd-boot, UKIs, loader config)
/// inside the ISO tree, sized from what goes into it.
fn build_efi_boot_image(
    iso_root: &Path,
    uki_entries: &[UkiMenuEntry],
    progress: &dyn Fn(&str),
) -> Result<()> {
    progress("Creating EFI boot image...");

    let bootloader = iso_root.join(ISO_EFI_DIR).join(EFI_BOOTLOADER);
    let entries_dir = iso_root.join("loader/entries");
    let mut entries: Vec<_> = if entries_dir.is_dir() {
        fs::read_dir(&entries_dir)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<_>>()?
    } else {
        Vec::new()
    };
    entries.sort();

    let payload: u64 = std::iter::once(bootloader.as_path())
        .chain(uki_entries.iter().map(|u| u.path.as_path()))
        .map(|p| fs::metadata(p).map(|m| m.len()))
        .sum::<std::io::Result<u64>>()?;
    // FAT overhead and slack: 20% plus 8 MB, at least 16 MB.
    let size_mb = (payload * 12 / 10 / (1024 * 1024) + 8).max(16);

    let image = iso_root.join(EFIBOOT_FILENAME);
    create_fat16_image(&image, size_mb)?;
    for dir in ["::EFI", "::EFI/BOOT", "::EFI/Linux", "::loader", "::loader/entries"] {
        mmd(&image, dir)?;
    }
    mcopy(&image, &bootloader, "::EFI/BOOT/")?;
    for uki in uki_entries {
        mcopy(&image, &uki.path, "::EFI/Linux/")?;
    }
    mcopy(&image, &iso_root.join("loader/loader.conf"), "::loader/")?;
    for entry in entries {
        mcopy(&image, &entry, "::loader/entries/")?;
    }
    Ok(())
}

/// The loader entry name for a UKI file (`name.efi` → `name`).
fn entry_stem(filename: &str) -> &str {
    filename.trim_end_matches(".efi")
}

fn write_loader_entries(loader_dir: &Path, entries: &[UkiMenuEntry]) -> Result<()> {
    let entries_dir = loader_dir.join("entries");
    fs::create_dir_all(&entries_dir)
        .with_context(|| format!("creating loader entries dir '{}'", entries_dir.display()))?;
    for entry in entries {
        let entry_path = entries_dir.join(format!("{}.conf", entry_stem(&entry.filename)));
        let content = format!(
            "title {}\nlinux /EFI/Linux/{}\n",
            entry.title, entry.filename
        );
        fs::write(&entry_path, content)
            .with_context(|| format!("writing loader entry '{}'", entry_path.display()))?;
    }
    Ok(())
}
