//! An assortment of useful basic functions useful throughout the project.

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirEntry, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, SystemTime};
pub use self::layout::*;

mod layout {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::path::Path;

    pub fn device_layout_hash() -> u64 {
        let hasher = &mut DefaultHasher::new();
        if let Ok(dir) = Path::new("/dev/").read_dir() {
            for entry in dir {
                if let Ok(entry) = entry {
                    entry.path().hash(hasher);

                    if let Ok(md) = entry.metadata() {
                        if let Ok(created) = md.created() {
                            created.hash(hasher);
                        }
                    }
                }
            }
        }

        hasher.finish()
    }
}

pub fn watch_and_set<T, F>(swaps: Arc<RwLock<T>>, file: &'static str, mut create_new: F)
where T: 'static + Send + Sync,
      F: 'static + Send + FnMut() -> Option<T>
{
    thread::spawn(move || {
        let mut modified = get_modified(file).expect("modified time could not be obtained");

        loop {
            thread::sleep(Duration::from_secs(3));
            if let Ok(new_modified) = get_modified(file) {
                if new_modified != modified {
                    modified = new_modified;
                    if let Ok(ref mut swaps) = swaps.write() {
                        if let Some(new_swaps) = create_new() {
                            **swaps = new_swaps;
                        }
                    }
                }
            }
        }
    });
}

pub fn get_modified<P: AsRef<Path>>(path: P) -> io::Result<SystemTime> {
    File::open(path)
        .and_then(|file| file.metadata())
        .and_then(|metadata| metadata.modified())
}

/// Obtains the UUID of the given device path by resolving symlinks in `/dev/disk/by-uuid`
/// until the device is found.
pub fn get_uuid(path: &Path) -> Option<String> {
    let uuid_dir = Path::new("/dev/disk/by-uuid")
        .read_dir()
        .expect("unable to find /dev/disk/by-uuid");

    if let Ok(path) = path.canonicalize() {
        for uuid_entry in uuid_dir.filter_map(|entry| entry.ok()) {
            if let Ok(ref uuid_path) = uuid_entry.path().canonicalize() {
                if uuid_path == &path {
                    if let Some(uuid_entry) = uuid_entry.file_name().to_str() {
                        return Some(uuid_entry.into());
                    }
                }
            }
        }
    }

    None
}

pub fn from_uuid(uuid: &str) -> Option<PathBuf> {
    let uuid_dir = Path::new("/dev/disk/by-uuid")
        .read_dir()
        .expect("unable to find /dev/disk/by-uuid");

    for uuid_entry in uuid_dir.filter_map(|entry| entry.ok()) {
        let uuid_entry = uuid_entry.path();
        if let Some(name) = uuid_entry.file_name() {
            if name == uuid {
                if let Ok(uuid_entry) = uuid_entry.canonicalize() {
                    return Some(uuid_entry);
                }
            }
        }
    }

    None
}

/// Concatenates an array of `&OsStr` into a new `OsString`.
pub(crate) fn concat_osstr(input: &[&OsStr]) -> OsString {
    let mut output = OsString::with_capacity(input.iter().fold(0, |acc, c| acc + c.len()));

    input.iter().for_each(|comp| output.push(comp));
    output
}

pub(crate) fn device_maps<F: FnMut(&Path)>(mut action: F) {
    read_dirs("/dev/mapper", |pv| action(&pv.path())).unwrap()
}

pub(crate) fn read_dirs<P: AsRef<Path>, F: FnMut(DirEntry)>(
    path: P,
    mut action: F,
) -> io::Result<()> {
    for entry in path.as_ref().read_dir()? {
        match entry {
            Ok(entry) => action(entry),
            Err(_) => continue,
        }
    }

    Ok(())
}

pub(crate) fn resolve_slave(name: &str) -> Option<PathBuf> {
    let slaves_dir = PathBuf::from(["/sys/class/block/", name, "/slaves/"].concat());
    if !slaves_dir.exists() {
        return Some(PathBuf::from(["/dev/", name].concat()));
    }

    let mut slaves = Vec::new();

    for entry in slaves_dir.read_dir().ok()? {
        if let Ok(entry) = entry {
            if let Ok(name) = entry.file_name().into_string() {
                slaves.push(name);
            }
        }
    }

    if slaves.len() == 1 {
        return Some(PathBuf::from(["/dev/", &slaves[0]].concat()));
    }

    None
}

pub(crate) fn resolve_to_physical(name: &str) -> Option<PathBuf> {
    let mut physical: Option<PathBuf> = None;

    loop {
        let physical_c = physical.clone();
        let name = physical_c.as_ref()
            .map_or(name, |physical| physical.file_name().unwrap().to_str().unwrap());
        if let Some(slave) = resolve_slave(name) {
            if physical.as_ref().map_or(true, |rec| rec != &slave) {
                physical = Some(slave);
                continue
            }
        }
        break
    }

    physical
}

pub(crate) fn resolve_parent(name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/block").ok()? {
        if let Ok(entry) = entry {
            if let Some(file) = entry.file_name().to_str() {
                if name.starts_with(file) {
                    return Some(PathBuf::from(["/dev/", file].concat()));
                }
            }
        }
    }

    None
}

pub(crate) fn zero<P: AsRef<Path>>(device: P, sectors: u64, offset: u64) -> io::Result<()> {
    let zeroed_sector = [0; 512];
    File::open(device.as_ref())
        .and_then(|mut file| {
            if offset != 0 {
                file.seek(SeekFrom::Start(512 * offset)).map(|_| ())?;
            }

            (0..sectors).map(|_| file.write(&zeroed_sector).map(|_| ())).collect()
        })
}

// TODO: These will be no longer be required once Rust is updated in the repos to 1.26.0

pub fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    File::open(path).and_then(|mut file| {
        let mut buffer = Vec::with_capacity(file.metadata().ok().map_or(0, |x| x.len()) as usize);
        file.read_to_end(&mut buffer).map(|_| buffer)
    })
}

pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
    File::create(path).and_then(|mut file| file.write_all(contents.as_ref()))
}
