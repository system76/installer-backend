mod builder;

pub use self::builder::PartitionBuilder;
pub use os_detect::OS;
pub use fstypes::{FileSystemType, PartitionType};
use FileSystemType::*;
use libparted::{Partition, PartitionFlag};
use proc_mounts::{swapoff, MountList, SwapList};
use external::{get_label, is_encrypted};
use fstab_generate::BlockInfo;
use os_detect::detect_os;
use disk_usage::sectors_used;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use super::PVS;
use super::super::{LvmEncryption, PartitionError};
use sys_mount::*;
use tempdir::TempDir;

bitflags! {
    pub struct FileSystemSupport: u8 {
        const LVM = 1;
        const LUKS = 2;
        const FAT = 4;
        const XFS = 8;
        const EXT4 = 16;
        const BTRFS = 32;
        const NTFS = 64;
        const F2FS = 128;
    }
}

pub fn get_preferred_options(fs: FileSystemType) -> &'static str {
    match fs {
        FileSystemType::Fat16 | FileSystemType::Fat32 => "umask=0077",
        FileSystemType::Ext4 => "noatime,errors=remount-ro",
        FileSystemType::Swap => "sw",
        _ => "defaults",
    }
}

// Defines that this partition exists in the source.
pub const SOURCE:  u8 = 0b00_0001;
// Defines that this partition will be removed.
pub const REMOVE:  u8 = 0b00_0010;
// Defines that this partition will be formatted.
pub const FORMAT:  u8 = 0b00_0100;
// Defines that this partition is currently active.
pub const ACTIVE:  u8 = 0b00_1000;
// Defines that this partition is currently busy.
pub const BUSY:    u8 = 0b01_0000;
// Defines that this partition is currently swapped.
pub const SWAPPED: u8 = 0b10_0000;

/// Contains relevant information about a certain partition.
#[derive(Debug, Clone, PartialEq)]
pub struct PartitionInfo {
    pub bitflags: u8,
    /// The partition number is the numeric value that follows the disk's device path.
    /// IE: _/dev/sda1_
    pub number: i32,
    /// The physical order of the partition on the disk, as partition numbers may not be in order.
    pub ordering: i32,
    /// The initial sector where the partition currently, or will, reside.
    pub start_sector: u64,
    /// The final sector where the partition currently, or will, reside.
    /// # Note
    /// The length of the partion can be calculated by substracting the `end_sector`
    /// from the `start_sector`, and multiplying that by the value of the disk's
    /// sector size.
    pub end_sector: u64,
    /// Whether this partition is a primary or logical partition.
    pub part_type: PartitionType,
    /// Whether there is a file system currently, or will be, on this partition.
    pub filesystem: Option<FileSystemType>,
    /// Specifies optional flags that should be applied to the partition, if
    /// not already set.
    pub flags: Vec<PartitionFlag>,
    /// Specifies the name of the partition.
    pub name: Option<String>,
    /// Contains the device path of the partition, which is the disk's device path plus
    /// the partition number.
    pub device_path: PathBuf,
    /// Where this partition is mounted in the file system, if at all.
    pub mount_point: Option<PathBuf>,
    /// Where this partition will be mounted in the future
    pub target: Option<PathBuf>,
    /// The pre-existing volume group assigned to this partition.
    pub original_vg: Option<String>,
    /// The volume group & LUKS configuration to associate with this device.
    // TODO: Separate the tuple?
    pub volume_group: Option<(String, Option<LvmEncryption>)>,
    /// If the partition is associated with a keyfile, this will name the key.
    pub key_id: Option<String>,
}

impl PartitionInfo {
    pub fn new_from_ped(partition: &Partition) -> io::Result<Option<PartitionInfo>> {
        let device_path = partition.get_path()
            .expect("unable to get path from ped partition")
            .to_path_buf();
        info!(
            "obtaining partition information from {}",
            device_path.display()
        );

        let filesystem = partition
            .fs_type_name()
            .and_then(|name| FileSystemType::from_str(name).ok());

        Ok(Some(PartitionInfo {
            bitflags: SOURCE | if partition.is_active() { ACTIVE } else { 0 }
                | if partition.is_busy() { BUSY } else { 0 },
            part_type: match partition.type_get_name() {
                "primary" => PartitionType::Primary,
                "logical" => PartitionType::Logical,
                _ => return Ok(None),
            },
            mount_point: None,
            target: None,
            filesystem,
            flags: get_flags(partition),
            number: partition.num(),
            ordering: -1,
            name: filesystem.and_then(|fs| get_label(&device_path, fs)),
            device_path,
            start_sector: partition.geom_start() as u64,
            end_sector: partition.geom_end() as u64,
            original_vg: None,
            volume_group: None,
            key_id: None,
        }))
    }

    pub fn collect_extended_information(&mut self, mounts: &MountList, swaps: &SwapList) {
        let device_path = &self.device_path;
        let original_vg = unsafe {
            PVS.as_ref()
                .unwrap()
                .get(device_path)
                .and_then(|vg| vg.as_ref().cloned())
        };

        if let Some(ref vg) = original_vg.as_ref() {
            info!("partition belongs to volume group '{}'", vg);
        }

        if self.filesystem.is_none() {
            self.filesystem = if is_encrypted(device_path) {
                Some(FileSystemType::Luks)
            } else if original_vg.is_some() {
                Some(FileSystemType::Lvm)
            } else {
                None
            };
        }

        self.mount_point = mounts.get_mount_point(device_path);
        self.bitflags |= if swaps.get_swapped(device_path) { SWAPPED } else { 0 };
        self.original_vg = original_vg;
    }

    pub fn deactivate_if_swap(&mut self, swaps: &SwapList) -> Result<(), (PathBuf, io::Error)> {
        {
            let path = &self.get_device_path();
            if swaps.get_swapped(path) {
                swapoff(path).map_err(|why| { (path.to_path_buf(), why) })?;
            }
        }
        self.mount_point = None;
        self.flag_disable(SWAPPED);
        Ok(())
    }

    pub fn flag_is_enabled(&self, flag: u8) -> bool { self.bitflags & flag != 0 }

    pub fn flag_disable(&mut self, flag: u8) { self.bitflags &= 255 ^ flag; }

    /// Assigns the partition to a keyfile ID.
    pub fn associate_keyfile(&mut self, id: String) {
        self.key_id = Some(id);
        self.target = None;
    }

    /// Returns the length of the partition in sectors.
    pub fn sectors(&self) -> u64 { self.end_sector - self.start_sector }

    // True if the partition contains an encrypted partition
    pub fn is_encrypted(&self) -> bool { is_encrypted(self.get_device_path()) }

    // True if the partition is an ESP partition.
    pub fn is_esp_partition(&self) -> bool {
        (self.filesystem == Some(Fat16) || self.filesystem == Some(Fat32))
            && self.flags.contains(&PartitionFlag::PED_PARTITION_ESP)
    }

    /// True if the partition is a swap partition.
    pub fn is_swap(&self) -> bool {
        self.filesystem
            .as_ref()
            .map_or(false, |&fs| fs == FileSystemType::Swap)
    }

    /// True if the partition is compatible for Linux to be installed on it.
    pub fn is_linux_compatible(&self) -> bool {
        self.filesystem
            .as_ref()
            .map_or(false, |&fs| match fs {
                Exfat | Ntfs | Fat16 | Fat32 | Lvm | Luks | Swap => false,
                Btrfs | Xfs | Ext2 | Ext3 | Ext4 | F2fs => true
            })
    }

    pub fn get_current_lvm_volume_group(&self) -> Option<&str> {
        self.original_vg.as_ref().map(|x| x.as_str())
    }

    /// Returns the path to this device in the system.
    pub fn get_device_path(&self) -> &Path { &self.device_path }

    /// True if the compared partition has differing parameters from the source.
    pub fn requires_changes(&self, other: &PartitionInfo) -> bool {
        self.sectors_differ_from(other) || self.filesystem != other.filesystem
            || self.flags != other.flags || other.flag_is_enabled(FORMAT)
    }

    /// True if the sectors in the compared partition differs from the source.
    pub fn sectors_differ_from(&self, other: &PartitionInfo) -> bool {
        self.start_sector != other.start_sector || self.end_sector != other.end_sector
    }

    /// Ture if the compared partition is the same as the source.
    pub fn is_same_partition_as(&self, other: &PartitionInfo) -> bool {
        self.flag_is_enabled(SOURCE) && other.flag_is_enabled(SOURCE) && self.number == other.number
    }

    /// Defines a mount target for this partition.
    pub fn set_mount(&mut self, target: PathBuf) { self.target = Some(target); }

    /// Defines that the partition belongs to a given volume group.
    ///
    /// Optionally, this partition may be encrypted, in which you will also need to
    /// specify a new physical volume name as well. In the event of encryption, an LVM
    /// device will be assigned to the encrypted partition.
    pub fn set_volume_group(&mut self, group: String, encryption: Option<LvmEncryption>) {
        self.volume_group = Some((group, encryption));
    }

    /// Shrinks the partition, if possible.
    pub fn shrink_to(&mut self, sectors: u64) -> Result<(), PartitionError> {
        if self.end_sector - self.start_sector < sectors {
            Err(PartitionError::ShrinkValueTooHigh)
        } else {
            self.end_sector -= sectors;
            Ok(())
        }
    }

    /// Defines that a new file system will be applied to this partition.
    /// NOTE: this will also unset the partition's name.
    pub fn format_with(&mut self, fs: FileSystemType) {
        self.bitflags |= FORMAT;
        self.filesystem = Some(fs);
        self.name = None;
    }

    /// Defines that a new file system will be applied to this partition.
    /// Unlike `format_with`, this will not remove the name.
    pub fn format_and_keep_name(&mut self, fs: FileSystemType) {
        self.bitflags |= FORMAT;
        self.filesystem = Some(fs);
    }

    /// Returns true if this partition will be formatted.
    pub fn will_format(&self) -> bool {
        self.bitflags & FORMAT != 0
    }

    /// Returns the number of used sectors on the file system that belongs to
    /// this partition.
    pub fn sectors_used(&self) -> Option<io::Result<u64>> {
        self.filesystem.and_then(|fs| match sectors_used(self.get_device_path(), fs) {
            Ok(sectors) => Some(Ok(sectors)),
            Err(why) => if why.kind() == io::ErrorKind::NotFound { None } else { Some(Err(why)) }
        })
    }

    /// Mount the file system temporarily, if possible.
    pub fn probe<T, F>(&self, mut func: F) -> T
        where F: FnMut(Option<(&Path, UnmountDrop<Mount>)>) -> T
    {
        let mount = self.filesystem
            .and_then(|fs| TempDir::new("distinst").ok().map(|t| (fs, t)));

        if let Some((fs, tempdir)) = mount {
            let fs = match fs {
                FileSystemType::Fat16 | FileSystemType::Fat32 => "vfat",
                fs => fs.into(),
            };

            // Mount the FS to the temporary directory
            let base = tempdir.path();
            match Mount::new(&self.device_path, base, fs, MountFlags::empty(), None).ok() {
                Some(m) => return func(Some((base, m.into_unmount_drop(UnmountFlags::DETACH)))),
                None => ()
            }
        }

        func(None)
    }

    /// Detects if an OS is installed to this partition, and if so, what the OS
    /// is named.
    pub fn probe_os(&self) -> Option<OS> {
        self.filesystem
            .and_then(|fs| detect_os(self.get_device_path(), fs))
    }

    /// Specifies to delete this partition from the partition table.
    pub fn remove(&mut self) { self.bitflags |= REMOVE; }

    /// Obtains bock information for the partition, if possible, for use with
    /// generating entries in "/etc/fstab".
    pub fn get_block_info(&self) -> Option<BlockInfo> {
        info!(
            "getting block information for partition at {}",
            self.device_path.display()
        );

        if self.filesystem != Some(FileSystemType::Swap)
            && (self.target.is_none() || self.filesystem.is_none())
        {
            return None;
        }

        let fs = self.filesystem.expect("unable to get block info due to lack of file system");
        Some(BlockInfo::new(
            BlockInfo::get_partition_id(&self.device_path, fs)?,
            fs,
            self.target.as_ref().map(|p| p.as_path()),
            get_preferred_options(fs)
        ))
    }
}

const FLAGS: &[PartitionFlag] = &[
    PartitionFlag::PED_PARTITION_BOOT,
    PartitionFlag::PED_PARTITION_ROOT,
    PartitionFlag::PED_PARTITION_SWAP,
    PartitionFlag::PED_PARTITION_HIDDEN,
    PartitionFlag::PED_PARTITION_RAID,
    PartitionFlag::PED_PARTITION_LVM,
    PartitionFlag::PED_PARTITION_LBA,
    PartitionFlag::PED_PARTITION_HPSERVICE,
    PartitionFlag::PED_PARTITION_PALO,
    PartitionFlag::PED_PARTITION_PREP,
    PartitionFlag::PED_PARTITION_MSFT_RESERVED,
    PartitionFlag::PED_PARTITION_BIOS_GRUB,
    PartitionFlag::PED_PARTITION_APPLE_TV_RECOVERY,
    PartitionFlag::PED_PARTITION_DIAG,
    PartitionFlag::PED_PARTITION_LEGACY_BOOT,
    PartitionFlag::PED_PARTITION_MSFT_DATA,
    PartitionFlag::PED_PARTITION_IRST,
    PartitionFlag::PED_PARTITION_ESP,
];

fn get_flags(partition: &Partition) -> Vec<PartitionFlag> {
    FLAGS
        .into_iter()
        .filter(|&&f| partition.is_flag_available(f) && partition.get_flag(f))
        .cloned()
        .collect::<Vec<PartitionFlag>>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn efi_partition() -> PartitionInfo {
        PartitionInfo {
            bitflags:     ACTIVE | BUSY | SOURCE,
            device_path:  Path::new("/dev/sdz1").to_path_buf(),
            flags:        vec![PartitionFlag::PED_PARTITION_ESP],
            mount_point:  Some(Path::new("/boot/efi").to_path_buf()),
            target:       Some(Path::new("/boot/efi").to_path_buf()),
            start_sector: 2048,
            end_sector:   1026047,
            filesystem:   Some(FileSystemType::Fat16),
            name:         None,
            number:       1,
            ordering:     1,
            part_type:    PartitionType::Primary,
            key_id:       None,
            original_vg:  None,
            volume_group: None,
        }
    }

    fn root_partition() -> PartitionInfo {
        PartitionInfo {
            bitflags:     ACTIVE | BUSY | SOURCE,
            device_path:  Path::new("/dev/sdz2").to_path_buf(),
            flags:        vec![],
            mount_point:  Some(Path::new("/").to_path_buf()),
            target:       Some(Path::new("/").to_path_buf()),
            start_sector: 1026048,
            end_sector:   420456447,
            filesystem:   Some(FileSystemType::Btrfs),
            name:         Some("Pop!_OS".into()),
            number:       2,
            ordering:     2,
            part_type:    PartitionType::Primary,
            key_id:       None,
            original_vg:  None,
            volume_group: None,
        }
    }

    fn luks_on_lvm_partition() -> PartitionInfo {
        PartitionInfo {
            bitflags:     ACTIVE | SOURCE,
            device_path:  Path::new("/dev/sdz3").to_path_buf(),
            flags:        vec![],
            mount_point:  None,
            target:       None,
            start_sector: 420456448,
            end_sector:   1936738303,
            filesystem:   Some(FileSystemType::Luks),
            name:         None,
            number:       4,
            ordering:     4,
            part_type:    PartitionType::Primary,
            key_id:       None,
            original_vg:  None,
            volume_group: Some((
                "LVM_GROUP".into(),
                Some(LvmEncryption {
                    physical_volume: "LUKS_PV".into(),
                    password:        Some("password".into()),
                    keydata:         None,
                })
            )),
        }
    }

    fn lvm_partition() -> PartitionInfo {
        PartitionInfo {
            bitflags:     ACTIVE | SOURCE,
            device_path:  Path::new("/dev/sdz3").to_path_buf(),
            flags:        vec![],
            mount_point:  None,
            target:       None,
            start_sector: 420456448,
            end_sector:   1936738303,
            filesystem:   Some(FileSystemType::Lvm),
            name:         None,
            number:       4,
            ordering:     4,
            part_type:    PartitionType::Primary,
            key_id:       None,
            original_vg:  None,
            volume_group: Some(("LVM_GROUP".into(), None)),
        }
    }

    fn swap_partition() -> PartitionInfo {
        PartitionInfo {
            bitflags:     ACTIVE | SOURCE,
            device_path:  Path::new("/dev/sdz4").to_path_buf(),
            flags:        vec![],
            mount_point:  None,
            target:       None,
            start_sector: 1936738304,
            end_sector:   1953523711,
            filesystem:   Some(FileSystemType::Swap),
            name:         None,
            number:       4,
            ordering:     4,
            part_type:    PartitionType::Primary,
            key_id:       None,
            original_vg:  None,
            volume_group: None,
        }
    }

    #[test]
    fn partition_sectors() {
        assert_eq!(swap_partition().sectors(), 16785407);
        assert_eq!(root_partition().sectors(), 419430399);
        assert_eq!(efi_partition().sectors(), 1023999);
    }

    #[test]
    fn partition_is_esp_partition() {
        assert!(!root_partition().is_esp_partition());
        assert!(efi_partition().is_esp_partition());
    }

    #[test]
    fn partition_is_linux_compatible() {
        assert!(root_partition().is_linux_compatible());
        assert!(!swap_partition().is_linux_compatible());
        assert!(!efi_partition().is_linux_compatible());
        assert!(!luks_on_lvm_partition().is_linux_compatible());
        assert!(!lvm_partition().is_linux_compatible());
    }

    #[test]
    fn partition_requires_changes() {
        let root = root_partition();

        {
            let mut other = root_partition();
            assert!(!root.requires_changes(&other));
            other.start_sector = 0;
            assert!(root.requires_changes(&other));
        }

        {
            let mut other = root_partition();
            other.format_with(FileSystemType::Btrfs);
            assert!(root.requires_changes(&other));
        }
    }

    #[test]
    fn partition_sectors_differ_from() {
        assert!(root_partition().sectors_differ_from(&efi_partition()));
        assert!(!root_partition().sectors_differ_from(&root_partition()));
    }

    #[test]
    fn partition_is_same_as() {
        let root = root_partition();
        let root_dup = root.clone();
        let efi = efi_partition();

        assert!(root.is_same_partition_as(&root_dup));
        assert!(!root.is_same_partition_as(&efi));
    }
}