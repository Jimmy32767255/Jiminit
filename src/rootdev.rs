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
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, FileTypeExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Bytes read from the start of a device when looking for a filesystem UUID.
const SAMPLE_LEN: usize = 0x10000 + 0x1000;

pub fn resolve(spec: &str) -> Result<PathBuf> {
    let spec = spec.trim().trim_matches('"');
    if spec.is_empty() {
        bail!("empty root= specification");
    }

    if let Some(uuid) = spec.strip_prefix("UUID=") {
        let uuid = uuid.trim_matches('"').trim();
        if uuid.is_empty() {
            bail!("root=UUID= is missing a value");
        }
        find_by_uuid(uuid)
    } else if let Some(partuuid) = spec.strip_prefix("PARTUUID=") {
        let partuuid = partuuid.trim_matches('"').trim();
        if partuuid.is_empty() {
            bail!("root=PARTUUID= is missing a value");
        }
        find_by_partuuid(partuuid)
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
    let by_uuid_dir = PathBuf::from("/dev/disk/by-uuid");
    // udev names the link after the blkid output; vfat UUIDs are uppercase
    // while ext4/btrfs/xfs ones are lowercase, so try both spellings.
    for candidate in [want.to_string(), want.to_lowercase(), want.to_uppercase()] {
        let link = by_uuid_dir.join(&candidate);
        if link.exists() {
            return Ok(link);
        }
    }

    let mut seen = Vec::new();
    for device in block_devices() {
        if let Some(uuid) = fs_uuid(&device) {
            if uuid.eq_ignore_ascii_case(want) {
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
    // LUKS1/LUKS2 containers: magic at 0, UUID string at 0xa8 (40 bytes).
    if buf.len() >= 0xa8 + 40 && &buf[0..6] == b"LUKS\xba\xbe" {
        let raw = &buf[0xa8..0xa8 + 40];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let uuid = std::str::from_utf8(&raw[..end]).ok()?.trim();
        if !uuid.is_empty() {
            return Some(uuid.to_string());
        }
    }

    // FAT12/16/32: boot sector at 0, volume serial shown as XXXX-XXXX.
    // blkid prints the serial (little-endian u32) as high word then low word,
    // e.g. `mkfs.vfat -i 0x12345678` -> `1234-5678`. The FAT label sits at
    // 54 (FAT12/16) or 82 (FAT32); exFAT keeps its serial at offset 100.
    if buf.len() >= 512 && buf[510] == 0x55 && buf[511] == 0xAA {
        let bps = u16::from_le_bytes([buf[11], buf[12]]);
        if (buf[0] == 0xEB || buf[0] == 0xE9) && matches!(bps, 512 | 1024 | 2048 | 4096) {
            let fat32 = &buf[82..85] == b"FAT";
            let fat1216 = &buf[54..57] == b"FAT";
            if fat32 || fat1216 {
                let offset = if fat32 { 67 } else { 39 };
                let id = u32::from_le_bytes(buf[offset..offset + 4].try_into().ok()?);
                return Some(format!("{:04X}-{:04X}", id >> 16, id & 0xFFFF));
            }
            if &buf[3..11] == b"EXFAT   " {
                let id = u32::from_le_bytes(buf[100..104].try_into().ok()?);
                return Some(format!("{:04X}-{:04X}", id >> 16, id & 0xFFFF));
            }
        }
    }

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

/// Logical sector size of a block device; plain files and unreadable
/// devices fall back to 512 (the PC default).
fn logical_sector_size(file: &File) -> u64 {
    // BLKSSZGET from linux/fs.h: _IO(0x12, 104).
    const BLKSSZGET: libc::c_ulong = 0x1268;
    let mut size: libc::c_int = 0;
    // SAFETY: BLKSSZGET writes one c_int into the provided pointer, which is
    // valid for the duration of the call.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), BLKSSZGET, &mut size) };
    if rc == 0 && size > 0 {
        size as u64
    } else {
        512
    }
}

/// Read the GPT header at the second logical sector (LBA 1). LBA addresses
/// in the table are relative to the logical sector size, which is 4096 on
/// 4Kn disks.
fn read_gpt_header(file: &File, sector_size: u64) -> Option<GptHeader> {
    let mut header = [0u8; 92];
    file.read_exact_at(&mut header, sector_size).ok()?;
    if &header[0..8] != b"EFI PART" {
        return None;
    }
    Some(GptHeader {
        entries_lba: u64::from_le_bytes(header[72..80].try_into().ok()?),
        entry_count: u32::from_le_bytes(header[80..84].try_into().ok()?),
        entry_size: u32::from_le_bytes(header[84..88].try_into().ok()?),
    })
}

fn gpt_lookup(file: &File, header: &GptHeader, sector_size: u64, want: &str) -> Option<usize> {
    if header.entry_size < 32 || header.entry_size > 4096 {
        return None;
    }
    let count = header.entry_count.min(128) as usize;
    let mut entries = vec![0u8; count * header.entry_size as usize];
    file.read_exact_at(&mut entries, header.entries_lba * sector_size)
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
    let sector_size = logical_sector_size(&file);
    if let Some(header) = read_gpt_header(&file, sector_size) {
        // A valid GPT means PARTUUIDs are GPT unique GUIDs. The protective
        // MBR has a zero disk signature, so falling back would match
        // "00000000-01" against a bogus entry.
        return gpt_lookup(&file, &header, sector_size, want);
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
    fn fat32_uuid_detection() {
        let mut buf = vec![0u8; 512];
        buf[0] = 0xEB;
        buf[11..13].copy_from_slice(&512u16.to_le_bytes());
        buf[82..85].copy_from_slice(b"FAT");
        buf[67..71].copy_from_slice(&0xABCD1234u32.to_le_bytes());
        buf[510] = 0x55;
        buf[511] = 0xAA;
        assert_eq!(uuid_from_sample(&buf).as_deref(), Some("ABCD-1234"));
    }

    #[test]
    fn fat12_uuid_detection() {
        let mut buf = vec![0u8; 512];
        buf[0] = 0xE9;
        buf[11..13].copy_from_slice(&1024u16.to_le_bytes());
        buf[54..57].copy_from_slice(b"FAT");
        buf[39..43].copy_from_slice(&0x0000ABCDu32.to_le_bytes());
        buf[510] = 0x55;
        buf[511] = 0xAA;
        assert_eq!(uuid_from_sample(&buf).as_deref(), Some("0000-ABCD"));
    }

    #[test]
    fn exfat_uuid_detection() {
        let mut buf = vec![0u8; 512];
        buf[0] = 0xEB;
        buf[3..11].copy_from_slice(b"EXFAT   ");
        buf[11..13].copy_from_slice(&512u16.to_le_bytes());
        buf[100..104].copy_from_slice(&0x11223344u32.to_le_bytes());
        buf[510] = 0x55;
        buf[511] = 0xAA;
        assert_eq!(uuid_from_sample(&buf).as_deref(), Some("1122-3344"));
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

    #[test]
    fn rejects_empty_root_specs() {
        assert!(resolve("").is_err());
        assert!(resolve("  ").is_err());
        assert!(resolve("UUID=").is_err());
        assert!(resolve("UUID=  ").is_err());
        assert!(resolve("PARTUUID=").is_err());
    }

    #[test]
    fn gpt_entries_follow_logical_sector_size() {
        let path = std::env::temp_dir().join(format!("jiminit-gpt-{}", std::process::id()));
        {
            let file = File::create(&path).unwrap();
            let mut header = vec![0u8; 92];
            header[0..8].copy_from_slice(b"EFI PART");
            header[72..80].copy_from_slice(&2u64.to_le_bytes());
            header[80..84].copy_from_slice(&1u32.to_le_bytes());
            header[84..88].copy_from_slice(&128u32.to_le_bytes());
            file.write_all_at(&header, 4096).unwrap();

            let mut entry = vec![0u8; 128];
            let guid = [
                0x67, 0x45, 0x23, 0x01, 0xab, 0x89, 0xef, 0xcd, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef,
            ];
            entry[16..32].copy_from_slice(&guid);
            file.write_all_at(&entry, 2 * 4096).unwrap();
        }

        let file = File::open(&path).unwrap();
        let header = read_gpt_header(&file, 4096).unwrap();
        assert_eq!(
            gpt_lookup(&file, &header, 4096, "01234567-89ab-cdef-0123-456789abcdef"),
            Some(1)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn detects_luks_uuid() {
        let mut sample = vec![0u8; 0x1000];
        sample[..6].copy_from_slice(b"LUKS\xba\xbe");
        let uuid = b"a5737d39-3624-4565-9d36-1d198b748a7d";
        sample[0xa8..0xa8 + uuid.len()].copy_from_slice(uuid);
        assert_eq!(
            uuid_from_sample(&sample).as_deref(),
            Some("a5737d39-3624-4565-9d36-1d198b748a7d")
        );
    }
}
