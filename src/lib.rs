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
//! create_iso(&config)?;
//! ```

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use distro_builder::{
    create_efi_dirs_in_fat, create_fat16_image, generate_iso_checksum, mcopy_to_fat, run_xorriso,
    setup_iso_structure, AppendedPartition,
};
use distro_spec::shared::{
    LoaderConfig, INITRAMFS_LIVE_ISO_PATH, KERNEL_ISO_PATH, LIVE_OVERLAYFS_ISO_PATH,
    ROOTFS_ISO_PATH, SELINUX_DISABLE,
};

// Re-export recuki types for convenience
pub use recuki::{OsRelease, UkiConfig};

/// Default EFI boot image size in MB.
/// UKIs require ~50MB each. With 3 UKIs + systemd-boot, need ~200MB.
pub const DEFAULT_EFIBOOT_SIZE_MB: u32 = 200;

/// Configuration for creating an ISO.
#[derive(Debug, Clone)]
pub struct IsoConfig {
    /// Path to kernel image (vmlinuz).
    pub kernel: PathBuf,
    /// Path to initramfs image.
    pub initrd: PathBuf,
    /// Path to rootfs image (EROFS).
    pub rootfs: PathBuf,
    /// ISO volume label (used for boot device detection).
    pub label: String,
    /// Output ISO path.
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Create a new ISO configuration.
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
        }
    }

    /// Add a prebuilt UKI file.
    pub fn with_prebuilt_uki(mut self, path: impl Into<PathBuf>) -> Self {
        self.ukis.push(UkiSource::Prebuilt(path.into()));
        self
    }

    /// Add a UKI to build.
    pub fn with_uki(
        mut self,
        name: impl Into<String>,
        extra_cmdline: impl Into<String>,
        filename: impl Into<String>,
    ) -> Self {
        self.ukis.push(UkiSource::Build {
            name: name.into(),
            extra_cmdline: extra_cmdline.into(),
            filename: filename.into(),
        });
        self
    }

    /// Set live overlay payload image (EROFS).
    pub fn with_overlay_image(mut self, path: impl Into<PathBuf>) -> Self {
        self.overlay_image = Some(path.into());
        self
    }

    /// Legacy compatibility alias for `with_overlay_image`.
    pub fn with_overlay(self, path: impl Into<PathBuf>) -> Self {
        self.with_overlay_image(path)
    }

    /// Set OS release branding.
    pub fn with_os_release(mut self, name: &str, id: &str, version: &str) -> Self {
        self.os_release = Some(OsRelease::new(name, id, version));
        self
    }

    /// Add an extra file to copy to ISO.
    pub fn with_extra_file(mut self, src: impl Into<PathBuf>, dst: impl Into<String>) -> Self {
        self.extra_files.push((src.into(), dst.into()));
        self
    }

    /// Disable checksum generation.
    pub fn without_checksum(mut self) -> Self {
        self.generate_checksum = false;
        self
    }

    /// Force legacy payload placement (`live/*.erofs` files in ISO filesystem).
    pub fn with_live_payload_iso_files(mut self) -> Self {
        self.live_payload_layout = LivePayloadLayout::IsoFiles;
        self
    }
}

/// Create a bootable ISO from the given configuration.
///
/// # Process
///
/// 1. Validate inputs exist
/// 2. Create ISO directory structure (boot/, live/, EFI/BOOT/)
/// 3. Copy kernel, initramfs, rootfs to ISO
/// 4. Build or copy UKIs
/// 5. Set up systemd-boot
/// 6. Create EFI boot image
/// 7. Run xorriso to create final ISO
/// 8. Generate checksum (optional)
///
/// # Errors
///
/// Returns an error if:
/// - Required input files don't exist
/// - systemd-boot is not installed
/// - xorriso fails
pub fn create_iso(config: &IsoConfig) -> Result<PathBuf> {
    // Stage 1: Validate inputs
    validate_inputs(config)?;

    // Create temp working directory alongside output
    let output_dir = config
        .output
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let iso_root = output_dir.join("iso-root");

    // Stage 2: Set up ISO directory structure
    println!("Setting up ISO structure...");
    setup_iso_structure(&iso_root)?;

    // Stage 3: Copy core files
    copy_core_files(config, &iso_root)?;

    // Stage 4: Set up UEFI boot with UKIs
    setup_uefi_boot(config, &iso_root, &output_dir)?;

    // Stage 5: Copy extra files
    for (src, dst) in &config.extra_files {
        let dst_path = iso_root.join(dst);
        if let Some(parent) = dst_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, &dst_path)
            .with_context(|| format!("Failed to copy {} to {}", src.display(), dst))?;
    }

    // Stage 6: Create ISO
    println!("Creating ISO with xorriso...");
    let mut appended = Vec::new();
    if config.live_payload_layout == LivePayloadLayout::AppendedPartitions {
        appended.push(AppendedPartition {
            index: 2,
            type_code: "0x83",
            path: &config.rootfs,
        });
        if let Some(ref overlay_image) = config.overlay_image {
            appended.push(AppendedPartition {
                index: 3,
                type_code: "0x83",
                path: overlay_image,
            });
        }
    }
    run_xorriso(
        &iso_root,
        &config.output,
        &config.label,
        "efiboot.img",
        &appended,
    )?;

    // Stage 7: Generate checksum
    if config.generate_checksum {
        println!("Generating checksum...");
        generate_iso_checksum(&config.output)?;
    }

    // Cleanup
    let _ = fs::remove_dir_all(&iso_root);

    println!("\nISO created: {}", config.output.display());
    if let Ok(meta) = fs::metadata(&config.output) {
        println!("  Size: {} MB", meta.len() / 1024 / 1024);
    }

    Ok(config.output.clone())
}

/// Validate that required input files exist.
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

    // Check for systemd-boot
    let systemd_boot = Path::new("/usr/lib/systemd/boot/efi/systemd-bootx64.efi");
    if !systemd_boot.exists() {
        bail!(
            "systemd-boot not found at {}.\n\
             Install: sudo dnf install systemd-boot",
            systemd_boot.display()
        );
    }

    Ok(())
}

/// Copy kernel, initramfs, rootfs to ISO structure.
fn copy_core_files(config: &IsoConfig, iso_root: &Path) -> Result<()> {
    println!("Copying boot files...");

    // Copy kernel
    fs::copy(&config.kernel, iso_root.join(KERNEL_ISO_PATH)).context("Failed to copy kernel")?;

    // Copy initramfs
    fs::copy(&config.initrd, iso_root.join(INITRAMFS_LIVE_ISO_PATH))
        .context("Failed to copy initramfs")?;

    if config.live_payload_layout == LivePayloadLayout::IsoFiles {
        println!("Copying rootfs...");
        fs::copy(&config.rootfs, iso_root.join(ROOTFS_ISO_PATH))
            .context("Failed to copy rootfs")?;

        if let Some(ref overlay_image) = config.overlay_image {
            println!("Copying live overlay image...");
            fs::copy(overlay_image, iso_root.join(LIVE_OVERLAYFS_ISO_PATH))
                .context("Failed to copy live overlay image")?;
        }
    } else {
        println!(
            "Live payload layout: appended partitions (rootfs/overlay not copied into ISO filesystem)"
        );
    }

    Ok(())
}

/// Set up UEFI boot with systemd-boot and UKIs.
fn setup_uefi_boot(config: &IsoConfig, iso_root: &Path, output_dir: &Path) -> Result<()> {
    println!("Setting up UEFI boot...");

    let systemd_boot = Path::new("/usr/lib/systemd/boot/efi/systemd-bootx64.efi");
    let uki_dir = iso_root.join("EFI/Linux");
    fs::create_dir_all(&uki_dir)?;

    // Build base cmdline
    let base_cmdline = format!(
        "root=LABEL={} console=ttyS0,115200n8 console=tty0 {}",
        config.label, SELINUX_DISABLE
    );

    // Process UKIs
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

                if let Some(ref os) = config.os_release {
                    uki_config = uki_config.with_os_release(&os.name, &os.id, &os.version);
                }

                println!("  Building UKI: {}", filename);
                recuki::build_uki(&uki_config)?;
                uki_entries.push(UkiMenuEntry {
                    title: name.clone(),
                    filename: filename.clone(),
                    path: output,
                });
            }
        }
    }

    // If no UKIs specified, build a default one
    if uki_entries.is_empty() {
        let filename = format!("{}.efi", config.label.to_lowercase());
        let output = uki_dir.join(&filename);
        let mut uki_config =
            recuki::UkiConfig::new(&config.kernel, &config.initrd, &base_cmdline, &output);

        if let Some(ref os) = config.os_release {
            uki_config = uki_config.with_os_release(&os.name, &os.id, &os.version);
        }

        println!("  Building UKI: {}", filename);
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

    // Copy systemd-boot
    fs::copy(systemd_boot, iso_root.join("EFI/BOOT/BOOTX64.EFI"))?;

    // Create loader.conf
    let loader_dir = iso_root.join("loader");
    fs::create_dir_all(&loader_dir)?;

    let default_entry = uki_entries
        .first()
        .map(|entry| entry_id(&entry.filename))
        .unwrap_or_else(|| "default.conf".to_string());
    let default_entry = default_entry.trim_end_matches(".conf").to_string();
    let loader_config = LoaderConfig::with_defaults("default")
        .with_default_entry(default_entry)
        .with_timeout(5)
        .with_console_mode("max");

    fs::write(
        loader_dir.join("loader.conf"),
        loader_config.to_loader_conf(),
    )?;
    write_loader_entries(&loader_dir, &uki_entries)?;

    // Create EFI boot image
    build_efi_boot_image(iso_root, output_dir, &uki_entries)?;

    Ok(())
}

/// Create EFI boot image with systemd-boot and UKIs.
fn build_efi_boot_image(
    iso_root: &Path,
    output_dir: &Path,
    uki_entries: &[UkiMenuEntry],
) -> Result<()> {
    println!("Creating EFI boot image...");

    let efiboot_img = output_dir.join("efiboot.img");

    // Create FAT16 image
    create_fat16_image(&efiboot_img, DEFAULT_EFIBOOT_SIZE_MB)?;

    // Create directory structure
    create_efi_dirs_in_fat(&efiboot_img)?;

    // Create EFI/Linux directory
    let img_str = efiboot_img.to_string_lossy();
    Command::new("mmd")
        .args(["-i", &img_str, "::EFI/Linux"])
        .output()
        .context("mmd failed to create ::EFI/Linux")?;

    // Copy systemd-boot
    mcopy_to_fat(
        &efiboot_img,
        &iso_root.join("EFI/BOOT/BOOTX64.EFI"),
        "::EFI/BOOT/",
    )?;

    // Copy UKIs
    for uki in uki_entries {
        mcopy_to_fat(&efiboot_img, &uki.path, "::EFI/Linux/")?;
    }

    // Create loader directory and copy loader.conf
    Command::new("mmd")
        .args(["-i", &img_str, "::loader"])
        .output()
        .context("mmd failed to create ::loader")?;

    mcopy_to_fat(
        &efiboot_img,
        &iso_root.join("loader/loader.conf"),
        "::loader/",
    )?;
    if iso_root.join("loader/entries").is_dir() {
        Command::new("mmd")
            .args(["-i", &img_str, "::loader/entries"])
            .output()
            .context("mmd failed to create ::loader/entries")?;
        let entries = fs::read_dir(iso_root.join("loader/entries"))
            .context("reading loader entries for efiboot image")?;
        for entry in entries {
            let entry = entry.context("reading loader entries dir entry")?;
            let entry_path = entry.path();
            if entry_path.extension().and_then(|e| e.to_str()) != Some("conf") {
                continue;
            }
            mcopy_to_fat(&efiboot_img, &entry_path, "::loader/entries/")?;
        }
    }

    // Copy efiboot.img into iso-root for xorriso
    fs::copy(&efiboot_img, iso_root.join("efiboot.img"))?;

    // Cleanup temp file
    let _ = fs::remove_file(&efiboot_img);

    Ok(())
}

fn entry_id(filename: &str) -> String {
    format!("{}.conf", filename.trim_end_matches(".efi"))
}

fn write_loader_entries(loader_dir: &Path, entries: &[UkiMenuEntry]) -> Result<()> {
    let entries_dir = loader_dir.join("entries");
    fs::create_dir_all(&entries_dir)
        .with_context(|| format!("creating loader entries dir '{}'", entries_dir.display()))?;
    for entry in entries {
        let entry_path = entries_dir.join(entry_id(&entry.filename));
        let content = format!(
            "title {}\nlinux /EFI/Linux/{}\n",
            entry.title, entry.filename
        );
        fs::write(&entry_path, content)
            .with_context(|| format!("writing loader entry '{}'", entry_path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_iso_config_builder() {
        let config = IsoConfig::new(
            "vmlinuz",
            "initramfs.img",
            "rootfs.erofs",
            "TESTISO",
            "test.iso",
        )
        .with_os_release("TestOS", "testos", "1.0")
        .with_uki("Normal", "", "testos.efi")
        .with_uki("Emergency", "emergency", "testos-emergency.efi");

        assert_eq!(config.kernel, PathBuf::from("vmlinuz"));
        assert_eq!(config.label, "TESTISO");
        assert_eq!(config.ukis.len(), 2);
        assert!(config.os_release.is_some());
    }

    #[test]
    fn test_validate_inputs_missing_kernel() {
        let config = IsoConfig::new(
            "/nonexistent/vmlinuz",
            "/nonexistent/initramfs.img",
            "/nonexistent/rootfs.erofs",
            "TEST",
            "test.iso",
        );

        let err = validate_inputs(&config).unwrap_err();
        assert!(err.to_string().contains("Kernel not found"));
    }
}
