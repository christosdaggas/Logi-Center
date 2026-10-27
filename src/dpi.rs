use anyhow::{Result, bail};

pub fn decode_list(data: &[u8]) -> Option<Vec<u16>> {
    if data.len() > 8192 {
        return None;
    }
    let words: Vec<u16> = data
        .chunks_exact(2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .collect();
    let mut values = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let value = words[i];
        if value == 0 {
            values.sort_unstable();
            values.dedup();
            return (!values.is_empty()).then_some(values);
        }
        if value & 0xE000 == 0xE000 {
            let step = value & 0x1FFF;
            let start = *values.last()?;
            let end = *words.get(i + 1)?;
            if step == 0 || end <= start || end > 30000 {
                return None;
            }
            let mut next = u32::from(start) + u32::from(step);
            while next <= u32::from(end) {
                values.push(next as u16);
                if values.len() > 4096 {
                    return None;
                }
                next += u32::from(step);
            }
            i += 2;
        } else {
            if value > 30000 {
                return None;
            }
            values.push(value);
            i += 1;
        }
    }
    None
}

pub fn append_page(bytes: &mut Vec<u8>, reply: &[u8]) -> Option<Vec<u16>> {
    if reply.len() < 3 || reply[0] != 0 {
        return None;
    }
    // Sensor id + seven 16-bit values + one padding byte in a long reply.
    let end = 1 + (reply.len() - 1) / 2 * 2;
    bytes.extend_from_slice(&reply[1..end]);
    decode_list(bytes)
}

pub fn current(reply: &[u8]) -> Option<u16> {
    let value = u16::from_be_bytes([*reply.get(1)?, *reply.get(2)?]);
    let value = if value == 0 {
        u16::from_be_bytes([*reply.get(3)?, *reply.get(4)?])
    } else {
        value
    };
    (value > 0).then_some(value)
}

pub fn supported(values: &[u16], current: Option<u16>) -> Vec<u16> {
    if !values.is_empty() {
        return values.to_vec();
    }
    let mut fallback: Vec<_> = (100..=4000).step_by(50).collect();
    if let Some(current) = current.filter(|v| *v > 0 && *v <= 30000) {
        fallback.push(current);
        fallback.sort_unstable();
        fallback.dedup();
    }
    fallback
}

pub fn presets(text: &str, supported: &[u16]) -> Result<Vec<u16>> {
    let mut values = Vec::new();
    for item in text.split(',') {
        let value = item
            .trim()
            .parse::<u16>()
            .map_err(|_| anyhow::anyhow!(crate::i18n::tr("Use comma-separated DPI numbers")))?;
        if !supported.contains(&value) {
            bail!(
                crate::i18n::tr("DPI {value} is not supported by this device")
                    .replace("{value}", &value.to_string())
            );
        }
        if !values.contains(&value) {
            values.push(value);
        }
    }
    if !(1..=8).contains(&values.len()) {
        bail!(crate::i18n::tr("Choose between 1 and 8 DPI presets"));
    }
    Ok(values)
}

pub fn next(presets: &[u16], current: u16) -> Option<u16> {
    if presets.is_empty() {
        return None;
    }
    let index = presets
        .iter()
        .position(|v| *v == current)
        .map_or(0, |i| (i + 1) % presets.len());
    presets.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranges_lists_and_invalid_packets() {
        let values = decode_list(&[0, 100, 0xE0, 50, 0x1F, 0x40, 0, 0]).unwrap();
        assert_eq!(values.first(), Some(&100));
        assert_eq!(values.last(), Some(&8000));
        assert_eq!(decode_list(&[1, 144, 3, 32, 0, 0]).unwrap(), vec![400, 800]);
        for packet in [
            vec![0xE0, 50, 0, 0],
            vec![0, 100, 0xE0, 0, 1, 0, 0, 0],
            vec![0, 100],
        ] {
            assert!(decode_list(&packet).is_none());
        }
    }
    #[test]
    fn preset_validation_and_cycle() {
        assert_eq!(
            presets("800, 1600,800", &[800, 1600]).unwrap(),
            vec![800, 1600]
        );
        assert!(presets("999", &[800]).is_err());
        assert!(presets("", &[800]).is_err());
        assert_eq!(next(&[800, 1600], 1600), Some(800));
        assert_eq!(next(&[800, 1600], 999), Some(800));
        assert_eq!(next(&[], 800), None);
    }
    #[test]
    fn full_length_replies_drop_padding_and_default_dpi_is_used() {
        let mut reply = vec![0; 16];
        reply[1..7].copy_from_slice(&[0, 100, 0xE0, 50, 0x1F, 0x40]);
        let values = append_page(&mut Vec::new(), &reply).unwrap();
        assert_eq!(values.last(), Some(&8000));
        assert_eq!(current(&[0, 0, 0, 3, 32]), Some(800));
        assert_eq!(current(&[0, 6, 64, 0, 0]), Some(1600));
        assert_eq!(current(&[0, 0, 0]), None);
        let mut pages = Vec::new();
        assert!(append_page(&mut pages, &[0, 0, 100, 0xE0, 50, 0]).is_none());
        assert_eq!(
            append_page(&mut pages, &[0, 1, 44, 0, 0, 0]).unwrap(),
            vec![100, 150, 200, 250, 300]
        );
    }
}
