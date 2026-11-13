//! `root=` resolution: plain device paths, `UUID=` and `PARTUUID=`.
//!
//! Initial ramdisks cannot rely on udev, so `UUID=` is resolved by reading
//! filesystem superblocks directly and `PARTUUID=` by parsing the GPT or MBR
//! partition table of candidate block devices.
//!
//! Supported filesystem UUID layouts: ext2/3/4, XFS, btrfs.

use anyhow::{bail, Result};
use std::collections::BTreeSet;
use std::fs::File;
use std::os::unix::fs::{FileExt, FileTypeExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Bytes read from the start of a device when looking for a filesystem UUID.
const SAMPLE_LEN: usize = 0x10000 + 0x1000;

pub fn resolve(spec: &str) -> Result<PathBuf> {
    let spec = spec.trim_matches('"');

    if let Some(uuid) = spec.strip_prefix("UUID=") {
        find_by_uuid(uuid.trim_matches('"'))
    } else if let Some(partuuid) = spec.strip_prefix("PARTUUID=") {
        find_by_partuuid(partuuid.trim_matches('"'))
    } else if spec.starts_with("LABEL=") || spec.starts_with("PARTLABEL=") {
        bail!("{spec} is not supported yet (only /dev/*, UUID= and PARTUUID=)")
    } else if spec.starts_with('/') {
        Ok(PathBuf::from(spec))
    } else {
        // Legacy syntax: root=sda1 means /dev/sda1.
        Ok(PathBuf::from("/dev").join(spec))
    }
}

/// Resolve, retrying while devices are still being created.
pub fn resolve_with_retry(spec: &str, attempts: usize, interval: Duration) -> Result<PathBuf> {
    let mut last_err = None;
    for attempt in 0..attempts.max(1) {
        match resolve(spec) {
            Ok(path) if path.exists() => return Ok(path),
            Ok(path) => last_err = Some(anyhow::anyhow!("{} does not exist", path.display())),
            Err(e) => last_err = Some(e),
        }
        if attempt + 1 < attempts {
            std::thread::sleep(interval);
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("cannot resolve root={spec}")))
}

fn block_devices() -> Vec<PathBuf> {
    let mut devices = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir("/dev") {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.file_type().is_block_device() {
                    devices.insert(entry.path());
                }
            }
        }
    }
    devices.into_iter().collect()
}

fn find_by_uuid(want: &str) -> Result<PathBuf> {
    let want = want.trim().to_ascii_lowercase();

    // On a normal system udev maintains these symlinks; use them when present.
    let by_uuid = PathBuf::from("/dev/disk/by-uuid").join(&want);
    if by_uuid.exists() {
        return Ok(by_uuid);
    }

    let mut seen = Vec::new();
    for device in block_devices() {
        if let Some(uuid) = fs_uuid(&device) {
            if uuid == want {
                return Ok(device);
            }
            seen.push(format!("{}={uuid}", device.display()));
        }
    }

    bail!(
        "no block device has UUID={want} (filesystem UUIDs seen: [{}])",
        seen.join(", ")
    )
}

fn find_by_partuuid(want: &str) -> Result<PathBuf> {
    let want = want.trim().to_ascii_lowercase();

    let by_partuuid = PathBuf::from("/dev/disk/by-partuuid").join(&want);
    if by_partuuid.exists() {
        return Ok(by_partuuid);
    }

    for device in block_devices() {
        if let Some(index) = partuuid_match(&device, &want) {
            let partition = partition_path(&device, index);
            if partition.exists() {
                return Ok(partition);
            }
        }
    }

    bail!("no partition has PARTUUID={want}")
}

fn fs_uuid(device: &Path) -> Option<String> {
    let sample = read_head(device, SAMPLE_LEN)?;
    uuid_from_sample(&sample)
}

fn read_head(path: &Path, len: usize) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        match file.read_at(&mut buf[filled..], filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) if filled > 0 => break,
            Err(_) => return None,
        }
    }
    buf.truncate(filled);
    Some(buf)
}

/// Detect a filesystem UUID from a leading sample of a block device.
pub fn uuid_from_sample(buf: &[u8]) -> Option<String> {
    // ext2/3/4: superblock at 1024, magic 0xEF53 at +0x38, UUID at +0x68.
    if buf.len() >= 1024 + 0x78 {
        let sb = &buf[1024..];
        if u16::from_le_bytes([sb[0x38], sb[0x39]]) == 0xEF53 {
            let uuid: &[u8; 16] = sb[0x68..0x78].try_into().ok()?;
            return Some(format_uuid(uuid));
        }
    }

    // btrfs: superblock at 0x10000, magic "_BHRfS_M" at +0x40, fsid at +0x20.
    if buf.len() >= 0x10000 + 0x48 {
        let sb = &buf[0x10000..];
        if &sb[0x40..0x48] == b"_BHRfS_M" {
            let uuid: &[u8; 16] = sb[0x20..0x30].try_into().ok()?;
            return Some(format_uuid(uuid));
        }
    }

    // XFS: superblock at 0, magic "XFSB", UUID at +0x20.
    if buf.len() >= 0x30 && &buf[0..4] == b"XFSB" {
        let uuid: &[u8; 16] = buf[0x20..0x30].try_into().ok()?;
        return Some(format_uuid(uuid));
    }

    None
}

fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Format a GPT GUID (mixed endianness) the way `blkid` and `/dev/disk` do.
fn format_gpt_guid(bytes: &[u8; 16]) -> String {
    let d1 = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let d2 = u16::from_le_bytes([bytes[4], bytes[5]]);
    let d3 = u16::from_le_bytes([bytes[6], bytes[7]]);
    let d4 = u16::from_be_bytes([bytes[8], bytes[9]]);
    let d5: String = bytes[10..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{d1:08x}-{d2:04x}-{d3:04x}-{d4:04x}-{d5}")
}

struct GptHeader {
    entries_lba: u64,
    entry_count: u32,
    entry_size: u32,
}

fn read_gpt_header(file: &File) -> Option<GptHeader> {
    let mut header = [0u8; 92];
    file.read_exact_at(&mut header, 512).ok()?;
    if &header[0..8] != b"EFI PART" {
        return None;
    }
    Some(GptHeader {
        entries_lba: u64::from_le_bytes(header[72..80].try_into().ok()?),
        entry_count: u32::from_le_bytes(header[80..84].try_into().ok()?),
        entry_size: u32::from_le_bytes(header[84..88].try_into().ok()?),
    })
}

fn gpt_lookup(file: &File, header: &GptHeader, want: &str) -> Option<usize> {
    if header.entry_size < 32 || header.entry_size > 4096 {
        return None;
    }
    let count = header.entry_count.min(128) as usize;
    let mut entries = vec![0u8; count * header.entry_size as usize];
    file.read_exact_at(&mut entries, header.entries_lba * 512)
        .ok()?;

    for index in 0..count {
        let offset = index * header.entry_size as usize;
        let guid: &[u8; 16] = entries.get(offset + 16..offset + 32)?.try_into().ok()?;
        if format_gpt_guid(guid) == want {
            return Some(index + 1);
        }
    }
    None
}

fn mbr_lookup(file: &File, want: &str) -> Option<usize> {
    let (disk_id, partno) = parse_mbr_partuuid(want)?;
    let mut sector = [0u8; 512];
    file.read_exact_at(&mut sector, 0).ok()?;
    if sector[510] != 0x55 || sector[511] != 0xAA {
        return None;
    }
    let signature = u32::from_le_bytes(sector[440..444].try_into().ok()?);
    if signature != disk_id {
        return None;
    }
    let index = partno as usize;
    if !(1..=4).contains(&index) || sector[446 + (index - 1) * 16 + 4] == 0 {
        return None;
    }
    Some(index)
}

fn parse_mbr_partuuid(spec: &str) -> Option<(u32, u8)> {
    let (disk_id, partno) = spec.split_once('-')?;
    if disk_id.len() != 8 || partno.len() != 2 {
        return None;
    }
    Some((
        u32::from_str_radix(disk_id, 16).ok()?,
        u8::from_str_radix(partno, 16).ok()?,
    ))
}

fn partuuid_match(device: &Path, want: &str) -> Option<usize> {
    let file = File::open(device).ok()?;
    if let Some(header) = read_gpt_header(&file) {
        if let Some(index) = gpt_lookup(&file, &header, want) {
            return Some(index);
        }
    }
    mbr_lookup(&file, want)
}

fn partition_path(disk: &Path, index: usize) -> PathBuf {
    let path = disk.to_string_lossy();
    let name = path.rsplit('/').next().unwrap_or("");
    if name.bytes().last().is_some_and(|b| b.is_ascii_digit()) {
        PathBuf::from(format!("{path}p{index}"))
    } else {
        PathBuf::from(format!("{path}{index}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_formatting() {
        let bytes: [u8; 16] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef,
        ];
        assert_eq!(format_uuid(&bytes), "01234567-89ab-cdef-0123-456789abcdef");
    }

    #[test]
    fn gpt_guid_formatting() {
        let bytes = [
            0x67, 0x45, 0x23, 0x01, 0xab, 0x89, 0xef, 0xcd, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef,
        ];
        assert_eq!(
            format_gpt_guid(&bytes),
            "01234567-89ab-cdef-0123-456789abcdef"
        );
    }

    #[test]
    fn ext4_uuid_detection() {
        let mut buf = vec![0u8; 0x2000];
        buf[1024 + 0x38..1024 + 0x3a].copy_from_slice(&0xEF53u16.to_le_bytes());
        let uuid = [
            0xde, 0xad, 0xbe, 0xef, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99,
            0xaa, 0xbb,
        ];
        buf[1024 + 0x68..1024 + 0x78].copy_from_slice(&uuid);
        assert_eq!(
            uuid_from_sample(&buf).as_deref(),
            Some("deadbeef-0011-2233-4455-66778899aabb")
        );
    }

    #[test]
    fn xfs_uuid_detection() {
        let mut buf = vec![0u8; 0x2000];
        buf[0..4].copy_from_slice(b"XFSB");
        buf[0x20..0x30].copy_from_slice(&[0x11; 16]);
        assert_eq!(
            uuid_from_sample(&buf).as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
    }

    #[test]
    fn zeros_have_no_uuid() {
        assert_eq!(uuid_from_sample(&[0u8; 0x2000]), None);
    }

    #[test]
    fn mbr_partuuid_parsing() {
        assert_eq!(parse_mbr_partuuid("4f5a1e2c-01"), Some((0x4f5a1e2c, 1)));
        assert_eq!(parse_mbr_partuuid("4f5a1e2c-0001"), None);
        assert_eq!(parse_mbr_partuuid("bad-format"), None);
    }

    #[test]
    fn partition_names() {
        assert_eq!(
            partition_path(Path::new("/dev/vda"), 2),
            PathBuf::from("/dev/vda2")
        );
        assert_eq!(
            partition_path(Path::new("/dev/nvme0n1"), 3),
            PathBuf::from("/dev/nvme0n1p3")
        );
        assert_eq!(
            partition_path(Path::new("/dev/mapper/root"), 1),
            PathBuf::from("/dev/mapper/root1")
        );
    }

    #[test]
    fn resolve_plain_paths() {
        assert_eq!(resolve("/dev/vda1").unwrap(), PathBuf::from("/dev/vda1"));
        assert_eq!(resolve("vda1").unwrap(), PathBuf::from("/dev/vda1"));
        assert!(resolve("LABEL=root").is_err());
    }
}
