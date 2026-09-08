use std::thread;
use std::time::Duration;

use crate::openocd::Session;
use crate::{AppError, AppResult};

pub(crate) const FLASH_BASE: u32 = 0x0040_0000;
pub(crate) const SRAM_BASE: u32 = 0x2000_0000;
const CHIPID_CIDR: u32 = 0x400e_0740;
const EEFC_FCR: u32 = 0x400e_0a04;
const EEFC_FRR: u32 = 0x400e_0a0c;
const SAM4E8C_EXID: u32 = 0x0012_0209;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FlashDescriptor {
    pub(crate) size: u32,
    pub(crate) page_size: u32,
    pub(crate) lock_regions: u32,
    pub(crate) lock_size: u32,
}

pub(crate) struct ChipInfo {
    pub(crate) cidr: u32,
    pub(crate) exid: u32,
    pub(crate) flash: FlashDescriptor,
}

impl ChipInfo {
    pub(crate) fn read(session: &mut Session) -> AppResult<Self> {
        let [cidr, exid] = read_identity(session)?;
        require_identity(exid)?;
        session.write32(EEFC_FCR, &[0x5a00_0000])?; // GETD: get flash descriptor.
        thread::sleep(Duration::from_millis(20));
        let mut descriptor = Vec::new();
        for _ in 0..4 {
            descriptor.push(read_frr(session)?);
        }
        let planes = usize::try_from(descriptor[3])
            .map_err(|_| AppError::Runtime("invalid EEFC plane count".into()))?;
        if planes == 0 || planes > 8 {
            return Err(AppError::Runtime(format!(
                "invalid EEFC plane count: {planes}"
            )));
        }
        for _ in 0..planes + 2 {
            descriptor.push(read_frr(session)?);
        }
        Ok(Self {
            cidr,
            exid,
            flash: parse_descriptor(&descriptor)?,
        })
    }

    pub(crate) fn cidr_flash_kb(&self) -> u32 {
        match (self.cidr >> 8) & 0xf {
            0 => 0,
            1 => 8,
            2 => 16,
            3 => 32,
            5 => 64,
            7 => 128,
            8 => 160,
            9 => 256,
            10 => 512,
            12 => 1024,
            14 => 2048,
            _ => 0,
        }
    }

    pub(crate) fn sram_kb(&self) -> u32 {
        const SRAM_KB: [u32; 16] = [
            48, 192, 384, 6, 24, 4, 80, 160, 8, 16, 32, 64, 128, 256, 96, 512,
        ];
        SRAM_KB[((self.cidr >> 16) & 0xf) as usize]
    }

    pub(crate) fn pages(&self) -> u32 {
        self.flash.size / self.flash.page_size
    }

    pub(crate) fn flash_end(&self) -> u32 {
        FLASH_BASE + self.flash.size
    }
}

pub(crate) fn require_sam4e8c(session: &mut Session) -> AppResult<()> {
    require_identity(read_identity(session)?[1])
}

fn read_identity(session: &mut Session) -> AppResult<[u32; 2]> {
    session
        .read32(CHIPID_CIDR, 2)?
        .try_into()
        .map_err(|_| AppError::Runtime("CHIPID returned incomplete data".into()))
}

fn require_identity(exid: u32) -> AppResult<()> {
    if exid == SAM4E8C_EXID {
        Ok(())
    } else {
        Err(AppError::Runtime(format!(
            "target is not an ATSAM4E8C (EXID {exid:#010x})"
        )))
    }
}

fn read_frr(session: &mut Session) -> AppResult<u32> {
    session
        .read32(EEFC_FRR, 1)?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Runtime("EEFC returned incomplete descriptor data".into()))
}

fn parse_descriptor(words: &[u32]) -> AppResult<FlashDescriptor> {
    if words.len() < 7 {
        return Err(AppError::Runtime(
            "EEFC flash descriptor is too short".into(),
        ));
    }
    let planes = words[3] as usize;
    let lock_index = 4_usize
        .checked_add(planes)
        .ok_or_else(|| AppError::Runtime("invalid EEFC flash descriptor".into()))?;
    if planes == 0 || planes > 8 || lock_index + 1 >= words.len() {
        return Err(AppError::Runtime("invalid EEFC flash descriptor".into()));
    }
    let size = words[1];
    let page_size = words[2];
    let lock_regions = words[lock_index];
    let lock_size = words[lock_index + 1];
    let plane_bytes: u64 = words[4..lock_index]
        .iter()
        .map(|size| u64::from(*size))
        .sum();
    if size == 0
        || FLASH_BASE.checked_add(size).is_none()
        || page_size == 0
        || !size.is_multiple_of(page_size)
        || lock_regions == 0
        || lock_size == 0
        || !lock_size.is_multiple_of(page_size)
        || u64::from(lock_regions) * u64::from(lock_size) != u64::from(size)
        || plane_bytes != u64::from(size)
    {
        return Err(AppError::Runtime("invalid EEFC flash geometry".into()));
    }
    Ok(FlashDescriptor {
        size,
        page_size,
        lock_regions,
        lock_size,
    })
}

pub(crate) fn read_gpnvm(session: &mut Session) -> AppResult<[u8; 2]> {
    session.write32(EEFC_FCR, &[0x5a00_000d])?; // GGPB: get GPNVM bits.
    thread::sleep(Duration::from_millis(20));
    let word = read_frr(session)?;
    Ok([(word & 1) as u8, ((word >> 1) & 1) as u8])
}

pub(crate) fn normalize_flash_addr(value: u64) -> AppResult<u32> {
    let value = if value < u64::from(FLASH_BASE) {
        value
            .checked_add(u64::from(FLASH_BASE))
            .ok_or_else(|| AppError::Usage("address is too large".into()))?
    } else {
        value
    };
    u32::try_from(value).map_err(|_| AppError::Usage("address is too large".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_descriptor_with_reported_plane_count() {
        let descriptor =
            parse_descriptor(&[1, 1024 * 1024, 512, 2, 512 * 1024, 512 * 1024, 128, 8192]).unwrap();
        assert_eq!(
            descriptor,
            FlashDescriptor {
                size: 1024 * 1024,
                page_size: 512,
                lock_regions: 128,
                lock_size: 8192,
            }
        );
        assert!(parse_descriptor(&[1, 2, 0, 1, 2, 3, 4]).is_err());
    }

    #[test]
    fn rejects_inconsistent_flash_geometry() {
        let valid = [1, 512 * 1024, 512, 1, 512 * 1024, 64, 8192];
        assert!(parse_descriptor(&valid).is_ok());
        for (index, value) in [
            (1, 0),
            (1, u32::MAX),
            (2, 0),
            (3, 0),
            (4, 1),
            (5, 0),
            (6, 1),
        ] {
            let mut invalid = valid;
            invalid[index] = value;
            assert!(parse_descriptor(&invalid).is_err(), "index {index}");
        }
    }

    #[test]
    fn rejects_wrong_target_before_peripheral_access() {
        assert!(require_identity(SAM4E8C_EXID).is_ok());
        assert!(require_identity(0).is_err());
    }
}
