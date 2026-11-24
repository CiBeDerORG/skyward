//! Installing guests from packages, choosing the next system to start,
//! and removing individual guests.

use crate::state::{State, Store};
use crate::{Held, Named, Other, Overview, SystemId};
use skyward_block::area::HEADER;

/// Where the areas' files are, in the data partition.
pub const FOLDER: &str = "/data/skyward/systems";
/// The devices the store and a root are laid out as while they are written.
pub const STORE_DEVICE: &str = "skyward-store";
#[cfg(target_os = "android")]
const GUEST_STORE_DEVICE: &str = "skyward-guest-store";
pub const ROOT_DEVICE: &str = "skyward-root";
/// The gigabyte the pages count sizes in.
const GB: u64 = 1_000_000_000;
/// The sizes a root is offered in, in gigabytes, past what it least takes.
const OFFERED: [u64; 6] = [8, 16, 32, 64, 128, 256];

/// How far an install has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Setting room aside in the data partition: the bytes so far, of all.
    Allocating { done: u64, total: u64 },
    /// Writing the root image, then reading it back.
    Writing { done: u64, total: u64 },
    /// Putting the boot set in the store.
    Finishing,
}

/// The room the data partition has for a system's root, of `free` bytes
/// free: less the store where none is made yet, and less a super's worth,
/// which the installed system's updates keep for themselves, as Android
/// keeps it from its dynamic systems.
pub fn room(free: u64, store: u64, super_size: u64) -> u64 {
    free.saturating_sub(store).saturating_sub(super_size).saturating_sub(HEADER)
}

/// The sizes offered for a root that takes at least `least` bytes, with
/// `room` bytes to spare: what it least takes in whole gigabytes, and the
/// usual sizes past that, as far as there is room. The area made for one
/// is rounded up to its filesystem's blocks.
pub fn sizes(least: u64, room: u64) -> Vec<u64> {
    let first = least.div_ceil(GB).max(1);
    std::iter::once(first).chain(OFFERED.into_iter().filter(|&s| s > first)).filter_map(|s| s.checked_mul(GB)).filter(|&s| s <= room).collect()
}

/// What the pages show of the systems, from the state and the installed
/// system's name; `changed` says whether its vendor_boot or dtbo differ
/// from when the other system was installed, and `held` what the boot
/// partitions hold.
pub fn overview(state: &State, installed: Named, changed: &[bool], held: Held) -> Overview {
    assert_eq!(state.systems.len(), changed.len(), "compatibility is checked for each guest");
    Overview {
        installed,
        others: state.systems.iter().zip(changed).map(|(s, &changed)| Other {
            id: SystemId::new(&s.id).expect("validated system id"),
            name: s.name.clone(),
            version: s.version.clone(),
            size: s.root.size - HEADER,
            changed,
        }).collect(),
        starting: state.starting(),
        held,
    }
}

impl Store {
    /// The next install needs the shared store, a dedicated guest store,
    /// or no extra storage when the shared guest entry is free.
    pub fn needed(state: &State, boot: u64, init_boot: u64) -> u64 {
        if state.store.is_none() {
            Store::size_for(boot, init_boot)
        } else if state.systems.iter().any(|s| s.boot_store.is_none()) {
            Store::guest_size(boot, init_boot)
        } else {
            0
        }
    }
}

#[cfg(any(target_os = "android", test))]
fn is_registered(state: &State, file: &std::path::Path) -> bool {
    state.areas().any(|area| area.file == file)
}

/// Both the legacy layout and current allocations live directly in FOLDER.
#[cfg(any(target_os = "android", test))]
fn managed_path(path: &std::path::Path) -> bool {
    if path.parent() != Some(std::path::Path::new(FOLDER)) {
        return false;
    }
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else { return false };
    if name == "store" || name.strip_suffix(".img").is_some_and(|id| SystemId::new(id).is_ok()) {
        return true;
    }
    let nonce = name.strip_prefix("guest-").and_then(|s| s.strip_suffix(".img").or_else(|| s.strip_suffix(".boot")))
        .or_else(|| name.strip_prefix("main-").and_then(|s| s.strip_suffix(".boot")));
    nonce.is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Detach a guest from the registry and return just the areas it releases.
/// The caller commits the registry and its cleanup journal before unlinking.
#[cfg(any(target_os = "android", test))]
fn detach(state: &mut State, id: SystemId) -> anyhow::Result<Vec<skyward_block::area::Area>> {
    use anyhow::{ensure, Context};
    ensure!(state.switching.is_none() && state.active != Some(id), "the guest is still in use");
    let Some(index) = state.systems.iter().position(|s| s.id == id.as_str()) else { return Ok(Vec::new()) };
    let guest = state.systems.remove(index);
    let mut areas = vec![guest.root];
    if let Some(store) = guest.boot_store {
        areas.push(store.area);
    }
    if state.systems.is_empty() {
        ensure!(state.occupied.is_none(), "a guest still occupies a boot slot");
        areas.push(state.store.take().context("the shared boot store is missing")?.area);
        state.saved = None;
    }
    Ok(areas)
}

#[cfg(target_os = "android")]
pub use sys::*;

#[cfg(target_os = "android")]
mod sys {
    use super::*;
    use crate::boot::{self, Start};
    use crate::package::{Contents, BOOT, INIT_BOOT, ROOT};
    use anyhow::Result;
    use crate::state::{PendingFile, System, Who, PATH};
    use crate::store::{self, OTHER};
    use crate::switch::{self, Places};
    use anyhow::{ensure, Context};
    use sha2::{Digest, Sha256};
    use skyward_block::area::{self, Area};
    use skyward_block::{dm, BY_NAME};
    use skyward_vab::Slot;
    use std::fs::{self, File, OpenOptions};
    use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::DirBuilderExt;
    use std::path::{Path, PathBuf};
    use zip::ZipArchive;

    const CHUNK: usize = 1 << 20;

    fn by_name() -> &'static Path {
        Path::new(BY_NAME)
    }

    /// Where the data partition is mounted.
    const DATA: &str = "/data";

    fn data() -> &'static Path {
        Path::new(DATA)
    }

    /// The bytes free in the data partition's filesystem.
    pub fn free() -> Result<u64> {
        Ok(skyward_block::mount::space(data()).context("look at the room in the data partition")?.available)
    }

    /// The room for a root, in the data partition as it is now.
    pub fn room_now(slot: Slot) -> Result<u64> {
        let _lock = lock()?;
        let mut state = state()?;
        if !state.pending_files.is_empty() {
            writable(|| cleanup(&mut state))?;
        }
        room_for(&state, slot)
    }

    fn room_for(state: &State, slot: Slot) -> Result<u64> {
        let size = |name: &str| skyward_block::size_bytes(store::partition(by_name(), name, slot));
        let needed = Store::needed(state, size(store::BOOT)?, size(store::INIT_BOOT)?);
        Ok(room(free()?, needed, skyward_block::size_bytes(by_name().join("super"))?))
    }

    /// Lay the store out as a device, once it is found where it was, run
    /// `work` on it, and take the device away again.
    fn with_store<T>(store: &Store, name: &str, work: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
        store.area.check_header()?;
        if dm::status(name)?.is_some() {
            dm::remove(name)?;
        }
        let node = store.area.map(name)?;
        let done = work(&node);
        let gone = dm::remove(name);
        let done = done?;
        gone?;
        Ok(done)
    }

    const METADATA: &str = "/metadata";

    /// All registry readers that subsequently mutate state hold the same
    /// directory lock, including across recovery workers or processes.
    fn lock() -> Result<File> {
        metadata()?;
        let dir = File::open(METADATA)?;
        dir.lock().context("lock the systems registry")?;
        Ok(dir)
    }

    /// Mount /metadata, where the state is kept, read-only where it is not
    /// mounted yet, as the recovery keeps it.
    pub fn metadata() -> Result<()> {
        let at = Path::new(METADATA);
        if !skyward_block::mount::is_mount_point(at)? {
            skyward_block::mount::mount_ro(&by_name().join("metadata"), at)?;
        }
        Ok(())
    }

    /// Run `work` with /metadata writable, and leave it read-only after.
    fn writable<T>(work: impl FnOnce() -> Result<T>) -> Result<T> {
        metadata()?;
        let at = Path::new(METADATA);
        skyward_block::mount::remount_writable(at, true)?;
        let done = work();
        let back = skyward_block::mount::remount_writable(at, false);
        let done = done?;
        back?;
        Ok(done)
    }

    /// The state as kept now.
    pub fn state() -> Result<State> {
        metadata()?;
        State::load(Path::new(PATH))
    }

    /// The registry for the recovery page. Once data is open, also reclaim
    /// files left by interrupted installs and removals before showing free space.
    pub fn state_for_display() -> Result<State> {
        let snapshot = state()?;
        if snapshot.pending_files.is_empty() || !skyward_block::mount::is_mount_point(data())? {
            return Ok(snapshot);
        }
        let _lock = lock()?;
        let mut current = state()?;
        writable(|| cleanup(&mut current))?;
        Ok(current)
    }

    /// The program the other system starts with: this recovery's own.
    fn program() -> PathBuf {
        Path::new("/").join(skyward_config::ramdisk::SYSTEM_INIT)
    }

    /// Have the bootloader start `to`, writing its boot set into the boot
    /// partitions; see [`switch::switch`]. Switching to the other system
    /// waits for any update to be merged, and first checks that the other
    /// system's root is still where it was put.
    pub fn switch_to(next: Slot, to: Who) -> Result<()> {
        let _lock = lock()?;
        switch_now(next, to)
    }

    fn switch_now(next: Slot, to: Who) -> Result<()> {
        let state = state()?;
        if let Who::Guest(id) = to {
            let update = skyward_vab::snapshot_state()?;
            ensure!(update == skyward_vab::SnapshotState::None, "an update is in flight ({update:?}), and the installed system has to start once more to merge it");
            state.guest(id).context("the selected guest is not installed")?.root.check_header()?;
        }
        writable(|| with_places(&state, to, |places| switch::switch(places, next, to)))
    }

    fn with_places<T>(state: &State, to: Who, work: impl FnOnce(&Places) -> Result<T>) -> Result<T> {
        let shared = state.store.as_ref().context("no store holds the boot sets")?;
        let guest_store = match to {
            Who::Installed => None,
            Who::Guest(id) => state.guest(id).context("the selected guest is not installed")?.boot_store.as_ref(),
        };
        let program = program();
        with_store(shared, STORE_DEVICE, |store| {
            let run = |guest_store: Option<&Path>| work(&Places { state: Path::new(PATH), by_name: by_name(), store, guest_store, program: &program });
            match guest_store {
                Some(guest) => with_store(guest, GUEST_STORE_DEVICE, |node| run(Some(node))),
                None => run(None),
            }
        })
    }

    /// Finish a switch cut off before, where one was; says whether there
    /// was one.
    pub fn resume() -> Result<bool> {
        let _lock = lock()?;
        resume_now()
    }

    fn resume_now() -> Result<bool> {
        let state = state()?;
        let Some(pending) = &state.switching else { return Ok(false) };
        writable(|| with_places(&state, pending.to, switch::resume))
    }

    /// What the boot partitions hold, set against the state as kept now;
    /// `starts` is the slot the bootloader starts.
    pub fn held(state: &State, starts: Slot) -> Result<Held> {
        switch::held(state, by_name(), starts)
    }

    /// Put the installed system's boot set back where the other system's
    /// is, as everything that writes or erases the installed system first
    /// does; says whether it had to. Where something else was written into
    /// those partitions since, nobody asked for it to be written over, and
    /// nothing is: the one who switches to the installed system on the
    /// systems page, told of it there, asks for that.
    pub fn ensure_installed() -> Result<bool> {
        let _lock = lock()?;
        restore_installed()
    }

    fn restore_installed() -> Result<bool> {
        resume_now()?;
        let state = state()?;
        let Some(slot) = state.occupied.as_deref().map(|s| Slot::from_suffix(s).with_context(|| format!("the other system is in slot {s:?}"))).transpose()? else {
            return Ok(false);
        };
        ensure!(
            held(&state, slot)? != Held::Rewritten,
            "the boot partitions of slot {} changed since Skyward wrote them: switch to the main system in Multi-boot first",
            slot.suffix()
        );
        switch_now(slot, Who::Installed)?;
        Ok(true)
    }

    /// The SHA-256 of all of a partition.
    fn sum_of(path: &Path) -> Result<String> {
        let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
        skyward_block::drop_cache(&f)?;
        let mut sum = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = f.read(&mut buf).with_context(|| format!("read {}", path.display()))?;
            if n == 0 {
                break;
            }
            sum.update(&buf[..n]);
        }
        Ok(hex::encode(sum.finalize()))
    }

    /// Whether the installed system's vendor_boot or dtbo in `slot` differ
    /// from when the other system was installed.
    pub fn changed(state: &State, slot: Slot) -> Result<Vec<bool>> {
        if state.systems.is_empty() {
            return Ok(Vec::new());
        }
        let vendor_boot = sum_of(&store::partition(by_name(), "vendor_boot", slot))?;
        let dtbo = sum_of(&store::partition(by_name(), "dtbo", slot))?;
        Ok(state.systems.iter().map(|s| s.vendor_boot != vendor_boot || s.dtbo != dtbo).collect())
    }

    /// Install the system in the package `zip`, whose contents are
    /// `contents`, with a root of `size` bytes: its root written into a
    /// new area of the data partition, which has to be open, and its boot
    /// set, with `program` to start it, put in the store, made where there
    /// is none yet. `slot` is the slot the phone runs from, whose
    /// init_boot image lends its header.
    pub fn install<R: Read + Seek>(zip: &mut ZipArchive<R>, contents: &Contents, size: u64, program: &[u8], slot: Slot, progress: &mut dyn FnMut(Step)) -> Result<()> {
        let _lock = lock()?;
        ensure!(skyward_block::mount::is_mount_point(data())?, "the data partition is not open");
        writable(|| {
            let mut state = State::load(Path::new(PATH))?;
            cleanup(&mut state)?;
            let result = install_now(&mut state, zip, contents, size, program, slot, progress);
            // A failed rename or sync can leave either complete registry
            // on disk. Only the registry on disk decides which files live.
            if result.is_err() {
                cleanup(&mut State::load(Path::new(PATH))?)?;
            }
            result
        })
    }

    fn allocate(state: &mut State, file: PathBuf, size: u64, progress: &mut dyn FnMut(Step)) -> Result<Area> {
        area::create_tracked(&file, size, |f| {
            let (inode, generation) = area::identity(f)?;
            state.pending_files.push(PendingFile { path: file.clone(), inode, generation });
            state.save(Path::new(PATH))
        }, &mut |done, total| progress(Step::Allocating { done, total }))
    }

    fn storage_folder() -> Result<&'static Path> {
        let folder = Path::new(FOLDER);
        for dir in [folder.parent().expect("Skyward folder"), folder] {
            match fs::DirBuilder::new().mode(0o700).create(dir) {
                Ok(()) => File::open(dir.parent().expect("folder under data"))?.sync_all()?,
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    ensure!(fs::symlink_metadata(dir)?.is_dir(), "unexpected systems directory {}", dir.display());
                }
                Err(e) => return Err(e).with_context(|| format!("make {}", dir.display())),
            }
        }
        Ok(folder)
    }

    fn install_now<R: Read + Seek>(state: &mut State, zip: &mut ZipArchive<R>, contents: &Contents, size: u64, program: &[u8], slot: Slot, progress: &mut dyn FnMut(Step)) -> Result<()> {
        ensure!(state.switching.is_none(), "a switch begun before is not done yet");
        let id = SystemId::new(&contents.manifest.id)?;
        ensure!(state.guest(id).is_none(), "a guest with id {id} is already installed");
        ensure!(size >= contents.least(), "a root of {size} bytes is less than the {} the system takes", contents.least());
        let room = room_for(state, slot)?;
        ensure!(size <= room, "the data partition has room for {room} bytes, less than {size}");
        let folder = storage_folder()?;
        let nonce = area::new_id()?;
        let boot = skyward_block::size_bytes(store::partition(by_name(), store::BOOT, slot))?;
        let init_boot = skyward_block::size_bytes(store::partition(by_name(), store::INIT_BOOT, slot))?;
        let shared = match &state.store {
            Some(store) => {
                ensure!(store.boot == boot && store.init_boot == init_boot, "boot partition sizes differ from the saved store");
                store.clone()
            }
            None => Store {
                area: allocate(state, folder.join(format!("main-{nonce}.boot")), Store::size_for(boot, init_boot), progress)?,
                boot,
                init_boot,
            },
        };
        let boot_store = if state.systems.iter().any(|s| s.boot_store.is_none()) {
            let area = allocate(state, folder.join(format!("guest-{nonce}.boot")), Store::guest_size(boot, init_boot), progress)?;
            Some(Store { area, boot, init_boot })
        } else {
            None
        };
        let root = allocate(state, folder.join(format!("guest-{nonce}.img")), size + HEADER, progress)?;
        write_root(zip, &root, contents.root, progress)?;
        progress(Step::Finishing);
        let start = Start { id: id.to_string(), filesystem: contents.manifest.filesystem.clone(), init: contents.manifest.init.clone(), root: root.clone() };
        let (image, own_ramdisk) = if contents.init_boot {
            let mut own = Vec::new();
            zip.by_name(INIT_BOOT).context("open init_boot.img")?.read_to_end(&mut own).context("read init_boot.img")?;
            let (image, len) = boot::with_start(&own, program, &start)?;
            (image, Some(len))
        } else {
            let template = if state.occupied.as_deref() == Some(slot.suffix()) {
                let saved = state.saved.as_ref().context("the occupied slot has no saved boot set")?;
                with_store(&shared, STORE_DEVICE, |node| store::init_boot(&shared, node, saved))?
            } else {
                fs::read(store::partition(by_name(), store::INIT_BOOT, slot)).context("read the phone's init_boot")?
            };
            (boot::init_boot(&template, program, &start)?, None)
        };
        let (store, entry) = boot_store.as_ref().map_or((&shared, OTHER), |guest| (guest, 0));
        let set = with_store(store, STORE_DEVICE, |device| {
            let mut kernel = zip.by_name(BOOT).context("open boot.img")?;
            store::put(store, device, entry, &mut kernel, contents.boot, &image)
        })?;
        let guest = System {
            id: id.to_string(),
            name: contents.manifest.name.clone(),
            version: contents.manifest.version.clone(),
            filesystem: contents.manifest.filesystem.clone(),
            init: contents.manifest.init.clone(),
            root,
            set,
            boot_store,
            own_ramdisk,
            vendor_boot: sum_of(&store::partition(by_name(), "vendor_boot", slot))?,
            dtbo: sum_of(&store::partition(by_name(), "dtbo", slot))?,
        };
        File::open(folder)?.sync_all()?;
        state.store = Some(shared);
        state.systems.push(guest);
        state.pending_files.clear();
        state.save(Path::new(PATH))
    }

    /// Write the root image from the package into the root area, after its
    /// header, then read it back.
    fn write_root<R: Read + Seek>(zip: &mut ZipArchive<R>, root: &Area, len: u64, progress: &mut dyn FnMut(Step)) -> Result<()> {
        ensure!(len <= root.size - HEADER, "the root image takes {len} bytes, more than its area's {}", root.size - HEADER);
        if dm::status(ROOT_DEVICE)?.is_some() {
            dm::remove(ROOT_DEVICE)?;
        }
        let node = root.map(ROOT_DEVICE)?;
        let written = (|| -> Result<()> {
            let mut out = OpenOptions::new().read(true).write(true).open(&node).with_context(|| format!("open {}", node.display()))?;
            let mut image = zip.by_name(ROOT).context("open rootfs.img")?;
            out.seek(SeekFrom::Start(HEADER))?;
            let mut buf = vec![0u8; CHUNK];
            let (mut sum, mut done) = (Sha256::new(), 0);
            while done < len {
                let n = (len - done).min(CHUNK as u64) as usize;
                image.read_exact(&mut buf[..n]).context("read rootfs.img")?;
                out.write_all(&buf[..n]).context("write the root")?;
                sum.update(&buf[..n]);
                done += n as u64;
                progress(Step::Writing { done, total: len * 2 });
            }
            out.sync_all().context("sync the root")?;
            skyward_block::drop_cache(&out)?;
            out.seek(SeekFrom::Start(HEADER))?;
            let (mut back, mut read) = (Sha256::new(), 0);
            while read < len {
                let n = (len - read).min(CHUNK as u64) as usize;
                out.read_exact(&mut buf[..n]).context("read the root back")?;
                back.update(&buf[..n]);
                read += n as u64;
                progress(Step::Writing { done: len + read, total: len * 2 });
            }
            ensure!(back.finalize() == sum.finalize(), "the root reads back different from what was written");
            Ok(())
        })();
        let gone = dm::remove(ROOT_DEVICE);
        written?;
        gone
    }

    /// Clear abandoned allocations or files released by a committed removal.
    /// A file still named by the registry belongs to a live system.
    fn cleanup(state: &mut State) -> Result<()> {
        if state.pending_files.is_empty() {
            return Ok(());
        }
        ensure!(skyward_block::mount::is_mount_point(data())?, "the data partition is not open");
        let folder = Path::new(FOLDER);
        if folder.try_exists()? {
            ensure!(folder.canonicalize()? == folder, "unexpected systems directory");
        }
        // An earlier I/O error can leave one of our mappings alive. Never
        // return its blocks to the filesystem while the mapping exists.
        for name in [ROOT_DEVICE, STORE_DEVICE, GUEST_STORE_DEVICE, area::NEW_DEVICE] {
            if dm::status(name)?.is_some() {
                dm::remove(name)?;
            }
        }
        for file in &state.pending_files {
            ensure!(managed_path(&file.path) && file.inode != 0, "invalid file in the systems cleanup journal");
            if !is_registered(state, &file.path) {
                area::remove_tracked(&file.path, file.inode, file.generation)?;
            }
        }
        if folder.try_exists()? {
            File::open(folder)?.sync_all()?;
        }
        state.pending_files.clear();
        state.save(Path::new(PATH))
    }

    /// Record the inode of an area after checking that its file still holds
    /// the storage we own. Unlinking follows the registry commit.
    fn retire(files: &mut Vec<PendingFile>, area: &Area) -> Result<()> {
        if area.file.try_exists()? {
            ensure!(managed_path(&area.file), "unexpected area file {}", area.file.display());
            let (inode, generation) = area::identity(&area.verified_file()?)?;
            files.push(PendingFile { path: area.file.clone(), inode, generation });
        }
        Ok(())
    }

    /// Remove just this guest. Restore the primary boot set if the selected
    /// guest is active and keep the shared store while any guest remains.
    pub fn remove(id: SystemId) -> Result<()> {
        let _lock = lock()?;
        ensure!(skyward_block::mount::is_mount_point(data())?, "the data partition is not open");
        resume_now()?;
        if state()?.active == Some(id) {
            restore_installed()?;
        }
        writable(|| {
            let path = Path::new(PATH);
            let mut state = State::load(path)?;
            cleanup(&mut state)?;
            if state.systems.len() == 1 && state.guest(id).is_some() {
                for slot in [Slot::A, Slot::B] {
                    ensure!(held(&state, slot)? == Held::Recorded, "slot {} still holds unrecorded guest boot images; keep the last guest and its boot store", slot.suffix());
                }
            }
            for area in detach(&mut state, id)? {
                retire(&mut state.pending_files, &area)?;
            }
            state.save(path)?;
            cleanup(&mut state)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_room_for_a_root_keeps_a_supers_worth_for_updates() {
        assert_eq!(room(59 * GB, 600 << 20, 9 * GB), 59 * GB - (600 << 20) - 9 * GB - HEADER);
        assert_eq!(room(GB, 0, 9 * GB), 0, "none to spare");
    }

    #[test]
    fn the_sizes_offered_start_at_what_the_root_takes_and_stay_within_room() {
        assert_eq!(sizes(3 << 30, 50 * GB), [4 * GB, 8 * GB, 16 * GB, 32 * GB], "3 GiB is past 3 GB");
        assert_eq!(sizes(100 << 20, 9 * GB), [GB, 8 * GB]);
        assert_eq!(sizes(20 * GB, 300 * GB), [20 * GB, 32 * GB, 64 * GB, 128 * GB, 256 * GB]);
        assert!(sizes(20 * GB, 10 * GB).is_empty(), "no room for it at all");
        assert!(sizes(u64::MAX, 10 * GB).is_empty());
    }

    #[test]
    fn cleanup_only_accepts_legacy_or_generated_area_names_in_the_systems_directory() {
        let dir = std::path::Path::new(FOLDER);
        for path in ["store", "ubuntu.img", "guest-00112233445566778899aabbccddeeff.boot", "main-00112233445566778899aabbccddeeff.boot"] {
            assert!(managed_path(&dir.join(path)));
        }
        for path in ["user.txt", "../ubuntu.img", "nested/guest.img", "main-other.boot", "/data/saved.img"] {
            assert!(!managed_path(&dir.join(path)));
        }
    }

    #[test]
    fn removing_one_guest_keeps_the_others_boot_set_and_the_main_backup() {
        use crate::state::{tests::area, Saved, System, Who};

        let shared = Store { area: area("store", Store::size_for(8192, 4096)), boot: 8192, init_boot: 4096 };
        let dedicated = Store { area: area("guest.boot", Store::guest_size(8192, 4096)), ..shared.clone() };
        let first = System {
            id: "one".into(), name: "Guest".into(), version: "1".into(), filesystem: "ext4".into(), init: "/init".into(),
            root: area("one.img", GB), set: Saved { entry: crate::store::OTHER, boot: "aa".into(), init_boot: "bb".into() },
            boot_store: None, own_ramdisk: None, vendor_boot: String::new(), dtbo: String::new(),
        };
        let second = System { id: "two".into(), root: area("two.img", GB), set: Saved { entry: 0, ..first.set.clone() }, boot_store: Some(dedicated.clone()), ..first.clone() };
        let saved = Saved { entry: 1, ..first.set.clone() };
        let second_id = SystemId::new("two").unwrap();
        let mut state = State {
            store: Some(shared.clone()), systems: vec![first.clone(), second.clone()], saved: Some(saved.clone()),
            occupied: Some("_a".into()), active: Some(second_id), ..State::default()
        };
        let view = overview(&state, Named { name: "HyperOS".into(), version: None }, &[true, false], Held::Recorded);
        assert_eq!(view.starting, Who::Guest(second_id));
        assert_eq!(view.others.iter().map(|s| (s.id.as_str(), s.changed)).collect::<Vec<_>>(), [("one", true), ("two", false)]);
        assert_eq!(Store::needed(&state, 8192, 4096), dedicated.area.size);
        assert!(detach(&mut state, second_id).is_err(), "the active guest stays until the primary is restored");
        assert_eq!(detach(&mut state, SystemId::new("one").unwrap()).unwrap(), std::slice::from_ref(&first.root));
        assert!(!is_registered(&state, &first.root.file));
        assert!(is_registered(&state, &second.root.file) && is_registered(&state, &dedicated.area.file));
        assert!(is_registered(&state, &shared.area.file));
        assert_eq!(state.saved, Some(saved));
        assert_eq!(Store::needed(&state, 8192, 4096), 0, "the shared store now has a free guest entry");
        state.active = None;
        state.occupied = None;
        assert_eq!(detach(&mut state, second_id).unwrap(), [second.root, dedicated.area, shared.area]);
        assert_eq!(state, State::default());
    }
}
