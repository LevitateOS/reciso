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
    copy_dir_recursive, create_efi_dirs_in_fat, create_fat16_image, generate_iso_checksum,
    mcopy_to_fat, run_xorriso, setup_iso_structure,
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
    /// Path to rootfs image (EROFS or squashfs).
    pub rootfs: PathBuf,
    /// ISO volume label (used for boot device detection).
    pub label: String,
    /// Output ISO path.
    pub output: PathBuf,
    /// UKI sources (prebuilt files or build specs).
    pub ukis: Vec<UkiSource>,
    /// Optional live overlay directory.
    pub overlay: Option<PathBuf>,
    /// OS release information for UKI branding.
    pub os_release: Option<OsRelease>,
    /// Additional files to copy to ISO root.
    pub extra_files: Vec<(PathBuf, String)>,
    /// Generate SHA512 checksum.
    pub generate_checksum: bool,
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
            overlay: None,
            os_release: None,
            extra_files: Vec::new(),
            generate_checksum: true,
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

    /// Set live overlay directory.
    pub fn with_overlay(mut self, path: impl Into<PathBuf>) -> Self {
        self.overlay = Some(path.into());
        self
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
    run_xorriso(&iso_root, &config.output, &config.label, "efiboot.img")?;

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
    if let Some(ref overlay) = config.overlay {
        if !overlay.exists() {
            bail!("Overlay directory not found: {}", overlay.display());
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
    fs::copy(&config.kernel, iso_root.join("boot/vmlinuz")).context("Failed to copy kernel")?;

    // Copy initramfs
    fs::copy(&config.initrd, iso_root.join("boot/initramfs.img"))
        .context("Failed to copy initramfs")?;

    // Copy rootfs
    println!("Copying rootfs...");
    let rootfs_name = config
        .rootfs
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "filesystem.img".to_string());
    fs::copy(&config.rootfs, iso_root.join("live").join(&rootfs_name))
        .context("Failed to copy rootfs")?;

    // Copy overlay if provided
    if let Some(ref overlay) = config.overlay {
        println!("Copying overlay...");
        copy_dir_recursive(overlay, &iso_root.join("live/overlay"))?;
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
        "root=LABEL={} console=ttyS0,115200n8 console=tty0",
        config.label
    );

    // Process UKIs
    let mut uki_files = Vec::new();

    for uki in &config.ukis {
        match uki {
            UkiSource::Prebuilt(path) => {
                let filename = path.file_name().context("UKI has no filename")?;
                let dst = uki_dir.join(filename);
                fs::copy(path, &dst)?;
                uki_files.push(dst);
            }
            UkiSource::Build {
                name: _,
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
                uki_files.push(output);
            }
        }
    }

    // If no UKIs specified, build a default one
    if uki_files.is_empty() {
        let filename = format!("{}.efi", config.label.to_lowercase());
        let output = uki_dir.join(&filename);
        let mut uki_config =
            recuki::UkiConfig::new(&config.kernel, &config.initrd, &base_cmdline, &output);

        if let Some(ref os) = config.os_release {
            uki_config = uki_config.with_os_release(&os.name, &os.id, &os.version);
        }

        println!("  Building UKI: {}", filename);
        recuki::build_uki(&uki_config)?;
        uki_files.push(output);
    }

    // Copy systemd-boot
    fs::copy(systemd_boot, iso_root.join("EFI/BOOT/BOOTX64.EFI"))?;

    // Create loader.conf
    let loader_dir = iso_root.join("loader");
    fs::create_dir_all(&loader_dir)?;

    let default_uki = uki_files
        .first()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    fs::write(
        loader_dir.join("loader.conf"),
        format!("timeout 5\ndefault {}\n", default_uki),
    )?;

    // Create EFI boot image
    build_efi_boot_image(iso_root, output_dir, &uki_files)?;

    Ok(())
}

/// Create EFI boot image with systemd-boot and UKIs.
fn build_efi_boot_image(iso_root: &Path, output_dir: &Path, uki_files: &[PathBuf]) -> Result<()> {
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
    for uki in uki_files {
        mcopy_to_fat(&efiboot_img, uki, "::EFI/Linux/")?;
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

    // Copy efiboot.img into iso-root for xorriso
    fs::copy(&efiboot_img, iso_root.join("efiboot.img"))?;

    // Cleanup temp file
    let _ = fs::remove_file(&efiboot_img);

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
