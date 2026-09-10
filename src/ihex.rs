#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Chunk {
    pub(crate) address: u32,
    pub(crate) data: Vec<u8>,
}

/// Validate Intel HEX records and return the addressed data, sorted and merged.
pub(crate) fn parse(data: &[u8], flash_base: u32, flash_end: u32) -> Result<Vec<Chunk>, String> {
    let mut base = 0u32;
    let mut segmented = false;
    let mut eof = false;
    let mut ranges = Vec::new();
    for (index, line) in data.split(|byte| *byte == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let fail = |message: &str| format!("Intel HEX line {}: {message}", index + 1);
        if eof {
            return Err(fail("unexpected content after end-of-file record"));
        }
        if line.first() != Some(&b':') {
            return Err(fail("record must start with ':'"));
        }
        let hex = &line[1..];
        if hex.len() < 10 || hex.len() % 2 != 0 {
            return Err(fail("truncated record or invalid record length"));
        }
        let bytes = hex
            .chunks_exact(2)
            .map(|pair| {
                let high = digit(pair[0]).ok_or_else(|| fail("invalid hexadecimal digit"))?;
                let low = digit(pair[1]).ok_or_else(|| fail("invalid hexadecimal digit"))?;
                Ok(high * 16 + low)
            })
            .collect::<Result<Vec<u8>, String>>()?;
        let count = bytes[0] as usize;
        if bytes.len() != count + 5 {
            return Err(fail("byte count does not match record length"));
        }
        if bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
            return Err(fail("invalid checksum"));
        }
        let offset = u16::from_be_bytes([bytes[1], bytes[2]]) as u32;
        let kind = bytes[3];
        let payload = &bytes[4..4 + count];
        if kind != 0 && offset != 0 {
            return Err(fail("non-data record address must be zero"));
        }
        match kind {
            0 => {
                if count == 0 {
                    continue;
                }
                // Segment addressing wraps the offset at 64 KiB. Linear
                // addressing carries into the next 64 KiB block.
                let first_len = if segmented {
                    count.min(0x10000 - offset as usize)
                } else {
                    count
                };
                for (offset, payload) in
                    [(offset, &payload[..first_len]), (0, &payload[first_len..])]
                {
                    if payload.is_empty() {
                        continue;
                    }
                    let start = base
                        .checked_add(offset)
                        .ok_or_else(|| fail("address overflow"))?;
                    let end = start
                        .checked_add(payload.len() as u32)
                        .ok_or_else(|| fail("address overflow"))?;
                    if start < flash_base || end > flash_end {
                        return Err(fail(&format!(
                            "data range 0x{start:08x}..0x{end:08x} is outside flash 0x{flash_base:08x}..0x{flash_end:08x}"
                        )));
                    }
                    ranges.push(Chunk {
                        address: start,
                        data: payload.to_vec(),
                    });
                }
            }
            1 if count == 0 => eof = true,
            2 | 4 if count == 2 => {
                let address = u16::from_be_bytes([payload[0], payload[1]]) as u32;
                segmented = kind == 2;
                base = address << if segmented { 4 } else { 16 };
            }
            // Entry points do not change where data is programmed.
            3 | 5 if count == 4 => {}
            1..=5 => return Err(fail("invalid byte count for record type")),
            _ => return Err(fail(&format!("unsupported record type 0x{kind:02x}"))),
        }
    }
    if !eof {
        return Err("Intel HEX is missing an end-of-file record".into());
    }
    if ranges.is_empty() {
        return Err("Intel HEX contains no data".into());
    }
    ranges.sort_unstable_by_key(|chunk| chunk.address);
    let mut merged: Vec<Chunk> = Vec::new();
    for chunk in ranges {
        if let Some(previous) = merged.last_mut() {
            let end = previous.address + previous.data.len() as u32;
            if chunk.address < end {
                return Err(format!(
                    "Intel HEX data overlaps at 0x{:08x}",
                    chunk.address
                ));
            }
            if chunk.address == end {
                previous.data.extend(chunk.data);
                continue;
            }
        }
        merged.push(chunk);
    }
    Ok(merged)
}

fn digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Range;

    fn ranges(data: &[u8], flash_base: u32, flash_end: u32) -> Result<Vec<Range<u32>>, String> {
        super::parse(data, flash_base, flash_end).map(|chunks| {
            chunks
                .into_iter()
                .map(|chunk| chunk.address..chunk.address + chunk.data.len() as u32)
                .collect()
        })
    }

    fn record(kind: u8, address: u16, payload: &[u8]) -> String {
        let mut bytes = vec![
            payload.len() as u8,
            (address >> 8) as u8,
            address as u8,
            kind,
        ];
        bytes.extend(payload);
        let checksum = bytes.iter().fold(0u8, |sum, byte| sum.wrapping_sub(*byte));
        bytes.push(checksum);
        format!(
            ":{}\n",
            bytes
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        )
    }

    fn parse(records: &[String]) -> Result<Vec<Range<u32>>, String> {
        ranges(records.concat().as_bytes(), 0x00400000, 0x00480000)
    }

    fn linear() -> String {
        record(4, 0, &[0, 0x40])
    }
    fn eof() -> String {
        record(1, 0, &[])
    }

    #[test]
    fn decoded_bytes_follow_address_order() {
        let text = [
            linear(),
            record(0, 2, &[0xab, 0xcd]),
            record(0, 0, &[0x12, 0x34]),
            record(0, 0x100, &[0x56]),
            eof(),
        ]
        .concat();
        assert_eq!(
            super::parse(text.as_bytes(), 0x400000, 0x480000).unwrap(),
            vec![
                Chunk {
                    address: 0x400000,
                    data: vec![0x12, 0x34, 0xab, 0xcd]
                },
                Chunk {
                    address: 0x400100,
                    data: vec![0x56]
                },
            ]
        );
    }

    #[test]
    fn separated_ranges_are_sorted_and_adjacent_records_merge() {
        assert_eq!(
            parse(&[
                record(4, 0, &[0, 0x42]),
                record(0, 0x100, &[1, 2]),
                linear(),
                record(0, 2, &[3, 4]),
                record(0, 0, &[1, 2]),
                eof(),
            ])
            .unwrap(),
            vec![0x400000..0x400004, 0x420100..0x420102]
        );
    }

    #[test]
    fn segment_and_linear_bases_replace_each_other() {
        let data = [
            record(2, 0, &[0x12, 0x34]),
            record(0, 0x56, &[1]),
            record(4, 0, &[0, 2]),
            record(0, 0, &[2]),
            eof(),
        ]
        .concat();
        assert_eq!(
            ranges(data.as_bytes(), 0, 0x400000).unwrap(),
            vec![0x12396..0x12397, 0x20000..0x20001]
        );
    }

    #[test]
    fn segment_offset_wraps_within_the_segment() {
        let data = [
            record(2, 0, &[0x12, 0x34]),
            record(0, 0xffff, &[0xab, 0xcd]),
            eof(),
        ]
        .concat();
        assert_eq!(
            super::parse(data.as_bytes(), 0, 0x400000).unwrap(),
            vec![
                Chunk {
                    address: 0x12340,
                    data: vec![0xcd]
                },
                Chunk {
                    address: 0x2233f,
                    data: vec![0xab]
                },
            ]
        );
    }

    #[test]
    fn record_crossing_64k_does_not_change_base_for_later_records() {
        assert_eq!(
            parse(&[
                linear(),
                record(0, 0xffff, &[1, 2]),
                record(0, 0, &[3]),
                eof()
            ])
            .unwrap(),
            vec![0x400000..0x400001, 0x40ffff..0x410001]
        );
    }

    #[test]
    fn accepts_crlf_lowercase_blank_lines_and_no_final_newline() {
        let text = [linear(), record(0, 0, &[0xab]), eof()]
            .concat()
            .to_lowercase()
            .replace('\n', "\r\n\r\n");
        assert_eq!(
            ranges(text.trim_end().as_bytes(), 0x400000, 0x480000).unwrap(),
            vec![0x400000..0x400001]
        );
    }

    #[test]
    fn entry_points_do_not_change_data_addresses() {
        assert_eq!(
            parse(&[
                linear(),
                record(3, 0, &[1, 2, 3, 4]),
                record(5, 0, &[5, 6, 7, 8]),
                record(0, 0, &[1]),
                eof()
            ])
            .unwrap(),
            vec![0x400000..0x400001]
        );
    }

    #[test]
    fn rejects_identical_partial_and_contained_overlaps() {
        for (offset, bytes) in [(0, vec![1, 2]), (1, vec![2, 3]), (1, vec![2])] {
            assert!(
                parse(&[
                    linear(),
                    record(0, 0, &[1, 2]),
                    record(0, offset, &bytes),
                    eof()
                ])
                .unwrap_err()
                .contains("overlap")
            );
        }
    }

    #[test]
    fn checks_flash_bounds_and_address_overflow() {
        for records in [
            vec![record(0, 0, &[1]), eof()],
            vec![record(4, 0, &[0, 0x48]), record(0, 0, &[1]), eof()],
            vec![record(4, 0, &[0, 0x47]), record(0, 0xffff, &[1, 2]), eof()],
        ] {
            assert!(parse(&records).unwrap_err().contains("outside flash"));
        }
        let data = [record(4, 0, &[0xff, 0xff]), record(0, 0xffff, &[1]), eof()].concat();
        assert!(
            ranges(data.as_bytes(), 0, u32::MAX)
                .unwrap_err()
                .contains("overflow")
        );
        assert_eq!(
            parse(&[record(4, 0, &[0, 0x47]), record(0, 0xffff, &[1]), eof()]).unwrap(),
            vec![0x47ffff..0x480000]
        );
    }

    #[test]
    fn rejects_missing_eof_empty_images_and_content_after_eof() {
        for records in [
            vec![],
            vec![eof()],
            vec![linear(), record(0, 0, &[]), eof()],
            vec![linear(), record(0, 0, &[1])],
            vec![linear(), record(0, 0, &[1]), eof(), eof()],
            vec![linear(), record(0, 0, &[1]), eof(), record(0, 1, &[2])],
        ] {
            assert!(parse(&records).is_err());
        }
    }

    #[test]
    fn rejects_bad_record_shapes_and_unknown_types() {
        for kind in 1..=5 {
            assert!(parse(&[linear(), record(kind, 1, &[]), eof()]).is_err());
            let wrong_count = if kind == 1 { vec![0] } else { vec![] };
            assert!(parse(&[linear(), record(kind, 0, &wrong_count), eof()]).is_err());
        }
        assert!(
            parse(&[linear(), record(6, 0, &[]), eof()])
                .unwrap_err()
                .contains("unsupported")
        );
    }

    #[test]
    fn rejects_bad_checksums_counts_truncation_and_non_ascii() {
        for bad in [
            ":010000000100\n",
            ":0200000001FE\n",
            ":0100000001F\n",
            ":01000000GGFE\n",
            ":\n",
            "0100000001FF\n",
            ":00000001FFextra\n",
            ":00000001Fé\n",
            " :00000001FF\n",
            "# comment\n",
        ] {
            assert!(parse(&[linear(), bad.into(), eof()]).is_err(), "{bad:?}");
        }
    }
}
