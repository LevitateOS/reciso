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

use iso::{create_fat16_image, mcopy, mmd, run_xorriso, setup_iso_structure};
use recinit::live::{INITRAMFS_LIVE_ISO_PATH, KERNEL_ISO_PATH, LIVE_OVERLAYFS_ISO_PATH, ROOTFS_ISO_PATH};

use recuki::OsRelease;

/// The UEFI boot directory, on the ISO and in its EFI boot image.
pub const ISO_EFI_DIR: &str = "EFI/BOOT";
/// The UEFI loader file name in `ISO_EFI_DIR`.
pub const EFI_BOOTLOADER: &str = "BOOTX64.EFI";
/// The FAT EFI boot image at the ISO root.
pub const EFIBOOT_FILENAME: &str = "efiboot.img";
/// Where the UKIs go, on the ISO and in its EFI boot image.
pub const UKI_EFI_DIR: &str = "EFI/Linux";
/// systemd-boot's directory (`loader.conf` and `entries/`).
pub const LOADER_ENTRIES_DIR: &str = "loader";
pub const LOADER_CONF_FILENAME: &str = "loader.conf";
/// The boot entries' directory in `LOADER_ENTRIES_DIR`.
const ENTRIES_SUBDIR: &str = "entries";

/// Default systemd-boot UEFI binary (the build host's); a variant should
/// pass its own rootfs's copy.
pub const DEFAULT_SYSTEMD_BOOT_EFI: &str = "/usr/lib/systemd/boot/efi/systemd-bootx64.efi";

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
    pub ukis: Vec<UkiSpec>,
    /// Optional live overlay payload image (EROFS).
    pub overlay_image: Option<PathBuf>,
    /// OS release information for UKI branding.
    pub os_release: Option<OsRelease>,
    /// Additional files to copy into the ISO, each to a path relative to
    /// its root (without `..`).
    pub extra_files: Vec<(PathBuf, String)>,
    /// Generate SHA512 checksum.
    pub generate_checksum: bool,
    /// systemd-boot UEFI binary to boot the ISO with.
    pub systemd_boot: PathBuf,
}

/// Where the live payloads go on the medium. The live init finds them as files
/// under `live/` on the medium it found by label; there is no other layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum LivePayloadLayout {
    /// Payloads are files under `live/` in the ISO filesystem.
    #[default]
    IsoFiles,
}

/// A UKI to build for the ISO's boot menu.
#[derive(Debug, Clone)]
pub struct UkiSpec {
    /// Display name for boot menu.
    pub name: String,
    /// Extra cmdline parameters (appended to base).
    pub extra_cmdline: String,
    /// Output filename (e.g., "myos-live.efi").
    pub filename: String,
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
            systemd_boot: PathBuf::from(DEFAULT_SYSTEMD_BOOT_EFI),
        }
    }
}

/// Create a bootable ISO and return its path.
///
/// The rootfs and overlay images go in as files under `live/`. Everything is built in a private
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
    let built = work.path().join("output.iso");
    run_xorriso(&iso_root, &built, &config.label, EFIBOOT_FILENAME)?;
    fs::rename(&built, &config.output)
        .with_context(|| format!("moving the ISO to {}", config.output.display()))?;

    // Checksum the final file, under its final name; without one, no older
    // checksum is left beside the new ISO.
    if config.generate_checksum {
        progress("Generating checksum...");
        let checksum = iso::write_checksum(&config.output)?;
        progress(&format!("  Wrote: {}", checksum.display()));
    } else {
        iso::remove_checksum(&config.output)?;
    }

    progress(&format!("\nISO created: {}", config.output.display()));
    let size = fs::metadata(&config.output)
        .with_context(|| format!("reading {}", config.output.display()))?
        .len();
    progress(&format!("  Size: {} MiB", size / 1024 / 1024));

    Ok(config.output.clone())
}

/// Every input file, checked once before any work.
fn validate_inputs(config: &IsoConfig) -> Result<()> {
    let files = [
        ("kernel", &config.kernel),
        ("initrd", &config.initrd),
        ("rootfs", &config.rootfs),
    ]
    .into_iter()
    .chain(config.overlay_image.iter().map(|path| ("overlay image", path)))
    .chain(config.extra_files.iter().map(|(src, _)| ("extra file", src)));
    for (what, path) in files {
        if !path.is_file() {
            bail!("the {what} {} is not a file", path.display());
        }
    }
    for (_, dst) in &config.extra_files {
        let relative = Path::new(dst)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)));
        if dst.is_empty() || !relative {
            bail!("extra file destination '{dst}' must be a path inside the ISO, without '..' or a leading '/'");
        }
    }
    if config.ukis.is_empty() {
        bail!("the ISO needs at least one UKI to boot");
    }

    if !config.systemd_boot.is_file() {
        bail!(
            "systemd-boot not found at {} (pass --systemd-boot)",
            config.systemd_boot.display()
        );
    }

    Ok(())
}

/// Copy kernel, initramfs and the payload images into the ISO tree.
fn copy_core_files(config: &IsoConfig, iso_root: &Path, progress: &dyn Fn(&str)) -> Result<()> {
    progress("Copying boot files...");

    fs::copy(&config.kernel, iso_root.join(KERNEL_ISO_PATH)).context("Failed to copy kernel")?;

    fs::copy(&config.initrd, iso_root.join(INITRAMFS_LIVE_ISO_PATH))
        .context("Failed to copy initramfs")?;

    progress("Copying rootfs...");
    fs::copy(&config.rootfs, iso_root.join(ROOTFS_ISO_PATH)).context("Failed to copy rootfs")?;

    if let Some(ref overlay_image) = config.overlay_image {
        progress("Copying live overlay image...");
        fs::copy(overlay_image, iso_root.join(LIVE_OVERLAYFS_ISO_PATH))
            .context("Failed to copy live overlay image")?;
    }

    Ok(())
}

/// Set up UEFI boot with systemd-boot and UKIs.
fn setup_uefi_boot(config: &IsoConfig, iso_root: &Path, progress: &dyn Fn(&str)) -> Result<()> {
    progress("Setting up UEFI boot...");

    let uki_dir = iso_root.join(UKI_EFI_DIR);
    fs::create_dir_all(&uki_dir)?;

    // The live init finds its boot medium by this label; everything else on the command line (consoles, distro
    // switches) comes from the caller.
    let base_cmdline = format!("root=LABEL={}", config.label);

    let mut uki_entries: Vec<UkiMenuEntry> = Vec::new();

    for uki in &config.ukis {
        let cmdline = if uki.extra_cmdline.is_empty() {
            base_cmdline.clone()
        } else {
            format!("{} {}", base_cmdline, uki.extra_cmdline)
        };

        let output = uki_dir.join(&uki.filename);
        let mut uki_config =
            recuki::UkiConfig::new(&config.kernel, &config.initrd, &cmdline, &output);
        uki_config.os_release = config.os_release.clone();

        progress(&format!("  Building UKI: {}", uki.filename));
        recuki::build_uki(&uki_config)?;
        uki_entries.push(UkiMenuEntry {
            title: uki.name.clone(),
            filename: uki.filename.clone(),
            path: output,
        });
    }

    fs::copy(&config.systemd_boot, iso_root.join(ISO_EFI_DIR).join(EFI_BOOTLOADER))?;

    let loader_dir = iso_root.join(LOADER_ENTRIES_DIR);
    fs::create_dir_all(&loader_dir)?;

    let first = uki_entries.first().context("the ISO has no boot entries")?;
    fs::write(
        loader_dir.join(LOADER_CONF_FILENAME),
        format!(
            "default {}.conf\ntimeout 5\nconsole-mode max\n",
            entry_stem(&first.filename)
        ),
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
    let entries_dir = iso_root.join(LOADER_ENTRIES_DIR).join(ENTRIES_SUBDIR);
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
    // FAT overhead and slack: 20% plus 8 MiB, at least 16 MiB.
    let size_mib = (payload * 12 / 10 / (1024 * 1024) + 8).max(16);

    let image = iso_root.join(EFIBOOT_FILENAME);
    create_fat16_image(&image, size_mib)?;
    // The ESP holds the same tree as the ISO: mtools paths are `::<path>`.
    let esp = |path: &str| format!("::{path}");
    let entries_path = format!("{LOADER_ENTRIES_DIR}/{ENTRIES_SUBDIR}");
    for dir in ["EFI", ISO_EFI_DIR, UKI_EFI_DIR, LOADER_ENTRIES_DIR, entries_path.as_str()] {
        mmd(&image, &esp(dir))?;
    }
    mcopy(&image, &bootloader, &esp(&format!("{ISO_EFI_DIR}/")))?;
    for uki in uki_entries {
        mcopy(&image, &uki.path, &esp(&format!("{UKI_EFI_DIR}/")))?;
    }
    mcopy(
        &image,
        &iso_root.join(LOADER_ENTRIES_DIR).join(LOADER_CONF_FILENAME),
        &esp(&format!("{LOADER_ENTRIES_DIR}/")),
    )?;
    for entry in entries {
        mcopy(&image, &entry, &esp(&format!("{entries_path}/")))?;
    }
    Ok(())
}

/// The loader entry name for a UKI file (`name.efi` → `name`).
fn entry_stem(filename: &str) -> &str {
    filename.strip_suffix(".efi").unwrap_or(filename)
}

fn write_loader_entries(loader_dir: &Path, entries: &[UkiMenuEntry]) -> Result<()> {
    let entries_dir = loader_dir.join(ENTRIES_SUBDIR);
    fs::create_dir_all(&entries_dir)
        .with_context(|| format!("creating loader entries dir '{}'", entries_dir.display()))?;
    for entry in entries {
        let entry_path = entries_dir.join(format!("{}.conf", entry_stem(&entry.filename)));
        let content = format!(
            "title {}\nlinux /{UKI_EFI_DIR}/{}\n",
            entry.title, entry.filename
        );
        fs::write(&entry_path, content)
            .with_context(|| format!("writing loader entry '{}'", entry_path.display()))?;
    }
    Ok(())
}
