use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::openocd::{Session, path_string, tcl_literal};
use crate::target::{FLASH_BASE, normalize_flash_addr};
use crate::{AppError, AppResult};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    images: BTreeMap<String, Image>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Image {
    pub(crate) description: Option<String>,
    pub(crate) parts: Vec<ImagePart>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImagePart {
    pub(crate) file: PathBuf,
    pub(crate) addr: Option<u64>,
}

pub(crate) struct LoadedConfig {
    pub(crate) path: Option<PathBuf>,
    pub(crate) images: BTreeMap<String, Image>,
}

pub(crate) struct FlashPart {
    pub(crate) file: PathBuf,
    pub(crate) addr: u32,
    elf: bool,
}

pub(crate) enum FlashPlan {
    Single,
    MultiBin(Vec<Range<u32>>),
}

pub(crate) fn config_directory() -> AppResult<PathBuf> {
    let nonempty_env = |name| env::var_os(name).filter(|value| !value.is_empty());
    if let Some(xdg) = nonempty_env("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("jog"));
    }
    let directory = if cfg!(windows) {
        nonempty_env("APPDATA").map(PathBuf::from)
    } else {
        nonempty_env("HOME").map(|home| PathBuf::from(home).join(".config"))
    };
    directory
        .map(|path| path.join("jog"))
        .ok_or_else(|| AppError::Usage("cannot locate the user configuration directory".into()))
}

pub(crate) fn load_config(explicit: Option<&Path>) -> AppResult<LoadedConfig> {
    let executable = env::current_exe()
        .map_err(|error| AppError::Usage(format!("cannot locate the jog executable: {error}")))?;
    let current_dir = env::current_dir().map_err(|error| {
        AppError::Usage(format!("cannot locate the current directory: {error}"))
    })?;
    let candidates = config_candidates(
        explicit,
        &current_dir,
        &executable,
        env::var_os("XDG_CONFIG_HOME"),
        env::var_os("HOME"),
        env::var_os("APPDATA"),
        cfg!(windows),
    );
    if explicit.is_some() && !candidates[0].is_file() {
        return Err(AppError::Usage(format!(
            "configuration file does not exist: {}",
            candidates[0].display()
        )));
    }
    let Some(path) = candidates.into_iter().find(|path| path.is_file()) else {
        return Ok(LoadedConfig {
            path: None,
            images: BTreeMap::new(),
        });
    };
    parse_config_file(&path)
}

fn config_candidates(
    explicit: Option<&Path>,
    current_dir: &Path,
    executable: &Path,
    xdg: Option<OsString>,
    home: Option<OsString>,
    appdata: Option<OsString>,
    windows: bool,
) -> Vec<PathBuf> {
    if let Some(path) = explicit {
        return vec![path.to_owned()];
    }
    let mut paths = vec![current_dir.join("jog.toml")];
    if let Some(parent) = executable.parent() {
        paths.push(parent.join("jog.toml"));
    }
    if let Some(xdg) = xdg.filter(|value| !value.is_empty()) {
        paths.push(PathBuf::from(xdg).join("jog").join("jog.toml"));
    }
    if windows {
        if let Some(appdata) = appdata.filter(|value| !value.is_empty()) {
            paths.push(PathBuf::from(appdata).join("jog").join("jog.toml"));
        }
    } else if let Some(home) = home.filter(|value| !value.is_empty()) {
        paths.push(
            PathBuf::from(home)
                .join(".config")
                .join("jog")
                .join("jog.toml"),
        );
    }
    paths.dedup();
    paths
}

fn parse_config_file(path: &Path) -> AppResult<LoadedConfig> {
    let text = fs::read_to_string(path)
        .map_err(|error| AppError::Usage(format!("cannot read {}: {error}", path.display())))?;
    let mut config: Config = toml::from_str(&text).map_err(|error| {
        let mut message = format!("cannot parse {}: {error}", path.display());
        if error.to_string().to_ascii_lowercase().contains("escape") {
            message.push_str("; use single quotes for Windows paths");
        }
        AppError::Usage(message)
    })?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    for (name, image) in &mut config.images {
        if name.trim().is_empty() {
            return Err(AppError::Usage("image names must not be empty".into()));
        }
        if image.parts.is_empty() {
            return Err(AppError::Usage(format!("image '{name}' has no parts")));
        }
        for part in &mut image.parts {
            if part.file.as_os_str().is_empty() {
                return Err(AppError::Usage(format!(
                    "an image part in '{name}' has no file"
                )));
            }
            let address =
                validate_image_address(&part.file, part.addr, &format!("image '{name}'"))?;
            part.addr = part.addr.map(|_| u64::from(address));
            if part.file.is_relative() {
                part.file = base.join(&part.file);
            }
        }
    }
    Ok(LoadedConfig {
        path: Some(path.to_owned()),
        images: config.images,
    })
}

pub(crate) fn prepare_flash_parts(
    target: &str,
    addr: Option<u32>,
    config: &LoadedConfig,
) -> AppResult<Vec<FlashPart>> {
    let mut parts = if let Some(image) = config.images.get(target) {
        if addr.is_some() {
            return Err(AppError::Usage(
                "--addr does not apply to a named image; edit jog.toml instead".into(),
            ));
        }
        image
            .parts
            .iter()
            .map(|part| {
                Ok(FlashPart {
                    elf: is_elf(&part.file),
                    file: part.file.clone(),
                    addr: validate_image_address(&part.file, part.addr, "named image")?,
                })
            })
            .collect::<AppResult<Vec<_>>>()?
    } else {
        let file = PathBuf::from(target);
        if !file.is_file() {
            let names = if config.images.is_empty() {
                "none defined".into()
            } else {
                config.images.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            return Err(AppError::Usage(format!(
                "'{target}' is not an existing file or named image ({names})"
            )));
        }
        let addr = validate_image_address(&file, addr.map(u64::from), "image file")?;
        vec![FlashPart {
            elf: is_elf(&file),
            file,
            addr,
        }]
    };
    for part in &mut parts {
        if !part.file.is_file() {
            return Err(AppError::Usage(format!(
                "image file does not exist: {}",
                part.file.display()
            )));
        }
        part.file = fs::canonicalize(&part.file).map_err(|error| {
            AppError::Usage(format!("cannot resolve {}: {error}", part.file.display()))
        })?;
        path_string(&part.file)?;
    }
    Ok(parts)
}

fn validate_image_format(path: &Path) -> AppResult<()> {
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| {
            AppError::Usage(format!(
                "image file has no supported extension: {}",
                path.display()
            ))
        })?;
    match extension.as_str() {
        "bin" | "elf" | "axf" => Ok(()),
        "hex" | "ihex" | "s19" | "srec" => Err(AppError::Usage(format!(
            "self-addressed image {} cannot be validated before erase; use a raw .bin with an address",
            path.display()
        ))),
        _ => Err(AppError::Usage(format!(
            "unsupported image extension '.{extension}' for {}",
            path.display()
        ))),
    }
}

fn validate_image_address(path: &Path, addr: Option<u64>, context: &str) -> AppResult<u32> {
    validate_image_format(path)?;
    if is_elf(path) {
        if addr.is_some() {
            return Err(AppError::Usage(format!(
                "ELF in {context} uses its own load addresses; remove the address"
            )));
        }
        return Ok(0);
    }
    normalize_flash_addr(
        addr.ok_or_else(|| AppError::Usage(format!("raw .bin in {context} requires an address")))?,
    )
}

pub(crate) fn plan_flash(parts: &[FlashPart], flash_end: u32) -> AppResult<FlashPlan> {
    if parts.is_empty() {
        return Err(AppError::Usage("image has no parts".into()));
    }
    let mut ranges = Vec::new();
    for part in parts {
        if part.elf {
            let data = fs::read(&part.file).map_err(|error| {
                AppError::Usage(format!("cannot read {}: {error}", part.file.display()))
            })?;
            ranges.extend(elf_ranges(&data, flash_end).map_err(|error| {
                AppError::Usage(format!("invalid ELF {}: {error}", part.file.display()))
            })?);
        } else {
            ranges.push(validate_part_range(part, flash_end)?);
        }
    }
    for (index, left) in ranges.iter().enumerate() {
        for right in &ranges[index + 1..] {
            if left.start < right.end && right.start < left.end {
                return Err(AppError::Usage(format!(
                    "multi-part image ranges overlap: {:#010x}-{:#010x} and {:#010x}-{:#010x}",
                    left.start,
                    left.end - 1,
                    right.start,
                    right.end - 1
                )));
            }
        }
    }
    if parts.len() == 1 && !parts[0].elf {
        Ok(FlashPlan::Single)
    } else {
        Ok(FlashPlan::MultiBin(ranges))
    }
}

fn is_elf(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("elf") || extension.eq_ignore_ascii_case("axf")
        })
}

fn image_type(part: &FlashPart) -> &'static str {
    if part.elf { "elf" } else { "bin" }
}

// Only the file-backed portion of PT_LOAD is programmed. p_paddr is the
// flash load address, including initial values that startup copies to SRAM.
fn elf_ranges(data: &[u8], flash_end: u32) -> Result<Vec<Range<u32>>, String> {
    let u16_at = |offset: usize| -> Result<u16, String> {
        data.get(offset..offset + 2)
            .map(|bytes| u16::from_le_bytes(bytes.try_into().unwrap()))
            .ok_or_else(|| "truncated header".into())
    };
    let u32_at = |offset: usize| -> Result<u32, String> {
        data.get(offset..offset + 4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
            .ok_or_else(|| "truncated header".into())
    };
    if data.len() < 52 || &data[..7] != b"\x7fELF\x01\x01\x01" {
        return Err("expected a little-endian ELF32 image".into());
    }
    if u16_at(16)? != 2 || u16_at(18)? != 40 || u32_at(20)? != 1 {
        return Err("expected an ARM executable ELF image".into());
    }
    let offset = u32_at(28)? as usize;
    let entry_size = u16_at(42)? as usize;
    let count = u16_at(44)? as usize;
    if u16_at(40)? != 52 || entry_size != 32 || count == 0 || count == 0xffff {
        return Err("unsupported ELF program header table".into());
    }
    let table_end = offset
        .checked_add(entry_size * count)
        .ok_or("program header table is too large")?;
    if table_end > data.len() {
        return Err("truncated program header table".into());
    }
    let mut ranges = Vec::new();
    for index in 0..count {
        let header = offset + index * entry_size;
        if u32_at(header)? != 1 {
            continue;
        }
        let file_offset = u32_at(header + 4)?;
        let address = u32_at(header + 12)?;
        let file_size = u32_at(header + 16)?;
        let memory_size = u32_at(header + 20)?;
        if file_size > memory_size {
            return Err("load segment file size exceeds memory size".into());
        }
        if u64::from(file_offset) + u64::from(file_size) > data.len() as u64 {
            return Err("truncated load segment".into());
        }
        if file_size == 0 {
            continue;
        }
        let end = address
            .checked_add(file_size)
            .ok_or("load address overflow")?;
        if address < FLASH_BASE || end > flash_end {
            return Err(format!(
                "load segment {address:#010x}-{end:#010x} is outside flash ({FLASH_BASE:#010x}-{flash_end:#010x})"
            ));
        }
        ranges.push(address..end);
    }
    if ranges.is_empty() {
        return Err("image has no flash load data".into());
    }
    Ok(ranges)
}

fn validate_part_range(part: &FlashPart, flash_end: u32) -> AppResult<Range<u32>> {
    let address = part.addr;
    if !(FLASH_BASE..flash_end).contains(&address) {
        return Err(AppError::Usage(format!(
            "{address:#010x} is outside flash ({FLASH_BASE:#010x}-{flash_end:#010x})"
        )));
    }
    let size = fs::metadata(&part.file)
        .map_err(|error| AppError::Usage(format!("cannot read {}: {error}", part.file.display())))?
        .len();
    if size == 0 {
        return Err(AppError::Usage(format!(
            "raw image is empty: {}",
            part.file.display()
        )));
    }
    let end = u64::from(address) + size;
    if end > u64::from(flash_end) {
        return Err(AppError::Usage(format!(
            "{} ({size} B) at {address:#010x} runs past flash end {flash_end:#010x}",
            part.file.display()
        )));
    }
    Ok(address..end as u32)
}

pub(crate) fn write_part(session: &mut Session, part: &FlashPart, erase: bool) -> AppResult<()> {
    let path = tcl_literal(&path_string(&part.file)?);
    let label = if part.elf {
        format!("{} at ELF load addresses", part.file.display())
    } else {
        format!("{} @ {:#010x}", part.file.display(), part.addr)
    };
    println!("Programming {label}...");
    let erase = if erase { " erase" } else { "" };
    let output = session
        .run(&format!(
            "flash write_image{erase} unlock {path} {:#x} {}",
            part.addr,
            image_type(part)
        ))
        .map_err(AppError::flash_incomplete)?;
    println!("ok: wrote {label}");
    if !output.is_empty() {
        println!("  {}", output.replace('\n', "\n  "));
    }
    Ok(())
}

pub(crate) fn verify_part(session: &mut Session, part: &FlashPart) -> AppResult<()> {
    let path = tcl_literal(&path_string(&part.file)?);
    println!("Verifying {}...", part.file.display());
    session
        .run(&format!(
            "verify_image {path} {:#x} {}",
            part.addr,
            image_type(part)
        ))
        .map_err(AppError::flash_incomplete)?;
    println!("ok: verified {}", part.file.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("jog-test-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn enforces_image_address_rules() {
        assert!(validate_image_address(Path::new("x.bin"), None, "test").is_err());
        assert_eq!(
            validate_image_address(Path::new("x.bin"), Some(0x10), "test").unwrap(),
            FLASH_BASE + 0x10
        );
        assert!(
            validate_image_address(Path::new("x.elf"), Some(FLASH_BASE.into()), "test").is_err()
        );
        assert_eq!(
            validate_image_address(Path::new("x.elf"), None, "test").unwrap(),
            0
        );
        assert!(validate_image_format(Path::new("x.txt")).is_err());
    }

    fn elf_fixture(segments: &[(u32, u32, u32, u32)]) -> Vec<u8> {
        let mut data = vec![0; 52 + segments.len() * 32];
        data[..7].copy_from_slice(b"\x7fELF\x01\x01\x01");
        data[16..18].copy_from_slice(&2_u16.to_le_bytes());
        data[18..20].copy_from_slice(&40_u16.to_le_bytes());
        data[20..24].copy_from_slice(&1_u32.to_le_bytes());
        data[28..32].copy_from_slice(&52_u32.to_le_bytes());
        data[40..42].copy_from_slice(&52_u16.to_le_bytes());
        data[42..44].copy_from_slice(&32_u16.to_le_bytes());
        data[44..46].copy_from_slice(&(segments.len() as u16).to_le_bytes());
        for (index, &(physical, virtual_addr, file_size, memory_size)) in
            segments.iter().enumerate()
        {
            let offset = data.len() as u32;
            let header = 52 + index * 32;
            for (field, value) in [
                (0, 1),
                (4, offset),
                (8, virtual_addr),
                (12, physical),
                (16, file_size),
                (20, memory_size),
            ] {
                data[header + field..header + field + 4].copy_from_slice(&value.to_le_bytes());
            }
            data.resize(data.len() + file_size as usize, 0xa5);
        }
        data
    }

    #[test]
    fn elf_uses_physical_load_addresses_and_keeps_gaps() {
        let data = elf_fixture(&[
            (FLASH_BASE, FLASH_BASE, 4, 4),
            (FLASH_BASE + 4, 0x20000000, 8, 0x1000),
            (0x47a000, 0x47a000, 4, 4),
            (0x20001000, 0x20001000, 0, 0x100),
        ]);
        assert_eq!(
            elf_ranges(&data, 0x480000).unwrap(),
            vec![
                FLASH_BASE..FLASH_BASE + 4,
                FLASH_BASE + 4..FLASH_BASE + 12,
                0x47a000..0x47a004,
            ]
        );
    }

    #[test]
    fn elf_rejects_truncation_wrong_machine_and_invalid_load_ranges() {
        let valid = elf_fixture(&[(FLASH_BASE, FLASH_BASE, 4, 4)]);
        for length in 0..valid.len() {
            assert!(elf_ranges(&valid[..length], 0x480000).is_err());
        }
        let mut wrong_machine = valid.clone();
        wrong_machine[18] = 3;
        assert!(elf_ranges(&wrong_machine, 0x480000).is_err());
        for segment in [
            (0x20000000, 0x20000000, 4, 4),
            (0x47ffff, 0x47ffff, 4, 4),
            (0xfffffffe, 0xfffffffe, 4, 4),
            (FLASH_BASE, FLASH_BASE, 4, 3),
            (FLASH_BASE, FLASH_BASE, 0, 4),
        ] {
            assert!(elf_ranges(&elf_fixture(&[segment]), 0x480000).is_err());
        }
    }

    #[test]
    fn elf_plan_rejects_overlapping_segments_before_erase() {
        let dir = temp_dir();
        let file = dir.join("overlap.elf");
        fs::write(
            &file,
            elf_fixture(&[
                (FLASH_BASE, FLASH_BASE, 8, 8),
                (FLASH_BASE + 4, FLASH_BASE + 4, 8, 8),
            ]),
        )
        .unwrap();
        assert!(
            plan_flash(
                &[FlashPart {
                    file,
                    addr: 0,
                    elf: true
                }],
                0x480000
            )
            .is_err()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn keeps_declared_format_when_resolving_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir();
        let data = elf_fixture(&[(FLASH_BASE, FLASH_BASE, 4, 4)]);
        let extensionless = dir.join("image-data");
        fs::write(&extensionless, &data).unwrap();
        let elf_link = dir.join("app.elf");
        symlink(&extensionless, &elf_link).unwrap();
        let elf_target = dir.join("target.elf");
        fs::write(&elf_target, &data).unwrap();
        let bin_link = dir.join("app.bin");
        symlink(&elf_target, &bin_link).unwrap();
        let config = LoadedConfig {
            path: None,
            images: BTreeMap::new(),
        };

        let elf = prepare_flash_parts(elf_link.to_str().unwrap(), None, &config).unwrap();
        assert_eq!(elf[0].file, fs::canonicalize(&extensionless).unwrap());
        assert_eq!(image_type(&elf[0]), "elf");
        assert_eq!(elf[0].addr, 0);
        match plan_flash(&elf, 0x480000).unwrap() {
            FlashPlan::MultiBin(ranges) => assert_eq!(ranges, vec![FLASH_BASE..FLASH_BASE + 4]),
            FlashPlan::Single => panic!("expected ELF load ranges"),
        }

        let bin =
            prepare_flash_parts(bin_link.to_str().unwrap(), Some(FLASH_BASE), &config).unwrap();
        assert_eq!(bin[0].file, fs::canonicalize(&elf_target).unwrap());
        assert_eq!(image_type(&bin[0]), "bin");
        assert_eq!(bin[0].addr, FLASH_BASE);
        assert_eq!(
            validate_part_range(&bin[0], 0x480000).unwrap(),
            FLASH_BASE..FLASH_BASE + data.len() as u32
        );
        assert!(matches!(
            plan_flash(&bin, 0x480000).unwrap(),
            FlashPlan::Single
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn accepts_named_elf_without_address() {
        let dir = temp_dir();
        let path = dir.join("jog.toml");
        fs::write(&path, "[images.app]\nparts = [{ file = 'app.elf' }]\n").unwrap();
        let config = parse_config_file(&path).unwrap();
        assert!(config.images["app"].parts[0].addr.is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn discovers_project_then_sidecar_then_platform_config() {
        let paths = config_candidates(
            None,
            Path::new("/project"),
            Path::new("/opt/jog/jog"),
            Some(OsString::from("/xdg")),
            Some(OsString::from("/home/user")),
            Some(OsString::from("C:\\AppData")),
            false,
        );
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/project/jog.toml"),
                PathBuf::from("/opt/jog/jog.toml"),
                PathBuf::from("/xdg/jog/jog.toml"),
                PathBuf::from("/home/user/.config/jog/jog.toml")
            ]
        );
        let explicit = config_candidates(
            Some(Path::new("chosen.toml")),
            Path::new("/project"),
            Path::new("/opt/jog/jog"),
            None,
            None,
            None,
            false,
        );
        assert_eq!(explicit, vec![PathBuf::from("chosen.toml")]);
    }

    #[test]
    fn uses_xdg_then_windows_appdata() {
        let paths = config_candidates(
            None,
            Path::new("/project"),
            Path::new("/portable/jog.exe"),
            Some(OsString::from("/xdg")),
            Some(OsString::from("C:\\Users\\u")),
            Some(OsString::from("C:\\Users\\u\\AppData\\Roaming")),
            true,
        );
        assert_eq!(paths[0], PathBuf::from("/project/jog.toml"));
        assert_eq!(paths[1], PathBuf::from("/portable/jog.toml"));
        assert_eq!(paths[2], PathBuf::from("/xdg/jog/jog.toml"));
        assert!(paths[3].ends_with(Path::new("jog/jog.toml")));
    }

    #[test]
    fn parses_config_and_resolves_relative_files() {
        let dir = temp_dir();
        let path = dir.join("jog.toml");
        fs::write(
            &path,
            "[images.app]\nparts = [{ file = 'build/app.bin', addr = 0x400000 }]\n",
        )
        .unwrap();
        let config = parse_config_file(&path).unwrap();
        assert_eq!(
            config.images["app"].parts[0].file,
            dir.join("build/app.bin")
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_invalid_config_shape() {
        let dir = temp_dir();
        let path = dir.join("jog.toml");
        fs::write(&path, "[images.app]\nparts = []\n").unwrap();
        assert!(parse_config_file(&path).is_err());
        fs::write(
            &path,
            "[images.app]\nparts = [{ file = 'app.elf', addr = 1 }]\n",
        )
        .unwrap();
        assert!(parse_config_file(&path).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn plans_multi_part_flash_before_writes() {
        let dir = temp_dir();
        let first = dir.join("first.bin");
        let second = dir.join("second.bin");
        fs::write(&first, [1, 2, 3, 4]).unwrap();
        fs::write(&second, [5, 6, 7, 8]).unwrap();
        let parts = [
            FlashPart {
                file: first,
                elf: false,
                addr: FLASH_BASE,
            },
            FlashPart {
                file: second,
                elf: false,
                addr: FLASH_BASE + 8,
            },
        ];
        match plan_flash(&parts, FLASH_BASE + 1024).unwrap() {
            FlashPlan::MultiBin(ranges) => assert_eq!(
                ranges,
                vec![FLASH_BASE..FLASH_BASE + 4, FLASH_BASE + 8..FLASH_BASE + 12]
            ),
            FlashPlan::Single => panic!("expected a multi-part plan"),
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_overlapping_multi_part_flash_plans() {
        let dir = temp_dir();
        let first = dir.join("first.bin");
        let second = dir.join("second.bin");
        fs::write(&first, [0; 8]).unwrap();
        fs::write(&second, [0; 8]).unwrap();
        let overlap = [
            FlashPart {
                file: first,
                elf: false,
                addr: FLASH_BASE,
            },
            FlashPart {
                file: second,
                elf: false,
                addr: FLASH_BASE + 4,
            },
        ];
        assert!(plan_flash(&overlap, FLASH_BASE + 1024).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn accepts_exact_flash_end_and_rejects_one_byte_past() {
        let dir = temp_dir();
        let exact = dir.join("exact.bin");
        let past = dir.join("past.bin");
        fs::write(&exact, [0; 4]).unwrap();
        fs::write(&past, [0; 5]).unwrap();
        let make = |file| FlashPart {
            file,
            elf: false,
            addr: FLASH_BASE,
        };
        assert!(validate_part_range(&make(exact), FLASH_BASE + 4).is_ok());
        assert!(validate_part_range(&make(past), FLASH_BASE + 4).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
