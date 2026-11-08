//! The ISO's on-disk layout and the external tools that produce it.

use anyhow::{bail, Context, Result};
use crate::ISO_EFI_DIR;
use recinit::live::{ISO_BOOT_DIR, ISO_LIVE_DIR};

/// The checksum written next to the ISO, in `sha512sum` format.
const ISO_CHECKSUM_SUFFIX: &str = ".sha512";
const SHA512_SEPARATOR: &str = "  ";
/// MBR partition offset for the hybrid ISO.
const XORRISO_PARTITION_OFFSET: u32 = 16;
const XORRISO_FS_FLAGS: &[&str] = &["-full-iso9660-filenames", "-joliet", "-rational-rock"];
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run a tool and fail with its stderr if it does not succeed.
fn run(cmd: &mut Command, what: &str) -> Result<String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let output = cmd
        .output()
        .with_context(|| format!("{what}: cannot run {program} (is it installed?)"))?;
    if !output.status.success() {
        bail!(
            "{what}: {program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Create the ISO tree: `boot/`, `live/` and `EFI/BOOT/`.
pub fn setup_iso_structure(iso_root: &Path) -> Result<()> {
    for dir in [ISO_BOOT_DIR, ISO_LIVE_DIR, ISO_EFI_DIR] {
        fs::create_dir_all(iso_root.join(dir))?;
    }
    Ok(())
}

/// Write `<name>.sha512` next to the ISO, naming the ISO by its file name.
pub fn write_checksum(iso_path: &Path) -> Result<PathBuf> {
    let stdout = run(Command::new("sha512sum").arg(iso_path), "checksumming the ISO")?;
    let hash = stdout
        .split_whitespace()
        .next()
        .context("sha512sum printed no hash")?;
    let filename = iso_path
        .file_name()
        .context("ISO path has no file name")?
        .to_string_lossy();
    let checksum_path = iso_path.with_extension(ISO_CHECKSUM_SUFFIX.trim_start_matches('.'));
    fs::write(&checksum_path, format!("{hash}{SHA512_SEPARATOR}{filename}\n"))?;
    Ok(checksum_path)
}

/// Create an empty FAT16 image of `size_mb`.
pub fn create_fat16_image(output: &Path, size_mb: u64) -> Result<()> {
    let file = fs::File::create(output)?;
    file.set_len(size_mb * 1024 * 1024)?;
    run(
        Command::new("mkfs.fat").args(["-F", "16"]).arg(output),
        "formatting the EFI image",
    )?;
    Ok(())
}

/// Create a directory inside a FAT image.
pub fn mmd(fat_image: &Path, dir: &str) -> Result<()> {
    run(
        Command::new("mmd").arg("-i").arg(fat_image).arg(dir),
        &format!("creating {dir} in the EFI image"),
    )?;
    Ok(())
}

/// Copy a file into a FAT image directory.
pub fn mcopy(fat_image: &Path, src: &Path, dst: &str) -> Result<()> {
    run(
        Command::new("mcopy").arg("-i").arg(fat_image).arg(src).arg(dst),
        &format!("copying {} into the EFI image", src.display()),
    )?;
    Ok(())
}

/// Build the hybrid ISO with xorriso; `efiboot` is relative to `iso_root`.
pub fn run_xorriso(
    iso_root: &Path,
    output: &Path,
    label: &str,
    efiboot: &str,
) -> Result<()> {
    let mut cmd = Command::new("xorriso");
    cmd.args(["-as", "mkisofs", "-o"])
        .arg(output)
        .args(["-V", label])
        .args(["-partition_offset", &XORRISO_PARTITION_OFFSET.to_string()])
        .args(XORRISO_FS_FLAGS)
        .args(["-e", efiboot, "-no-emul-boot", "-isohybrid-gpt-basdat"]);
    cmd.arg(iso_root);
    run(&mut cmd, "building the ISO")?;
    Ok(())
}
