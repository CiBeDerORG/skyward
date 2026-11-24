//! What Skyward keeps of the systems, on /metadata, which the recovery
//! reads before the data partition opens and which no filesystem it holds
//! the areas in can take away without it.

use crate::boot::Start;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use skyward_block::area::Area;
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Where the state is kept.
pub const PATH: &str = "/metadata/skyward/systems.toml";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Where the boot sets are kept, once a system has been installed.
    pub store: Option<Store>,
    /// The installed system's boot set, as last saved before the other
    /// system's took its place.
    pub saved: Option<Saved>,
    #[serde(default)]
    pub systems: Vec<System>,
    /// Which guest owns the occupied slot.
    pub active: Option<SystemId>,
    /// The slot whose boot partitions hold the other system's boot set;
    /// none while they hold the installed system's.
    pub occupied: Option<String>,
    /// What those partitions held once the other system's boot set was
    /// written into them, which tells whether anything else has written
    /// them since. A state kept by a Skyward that did not keep this has
    /// none.
    #[serde(default)]
    pub written: Option<Written>,
    /// A switch begun and not yet done.
    pub switching: Option<Switching>,
    /// Files claimed before installation or retired by removal, awaiting cleanup on open data.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_files: Vec<PendingFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingFile {
    pub path: std::path::PathBuf,
    pub inode: u64,
    pub generation: u32,
}

/// The SHA-256 of each of a slot's boot partitions, whole, as they read
/// back once written. The init_boot image written for the other system is
/// made with the program of the Skyward that writes it, so its sum is none
/// the store keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Written {
    pub boot: String,
    pub init_boot: String,
}

/// The area the boot sets are kept in, and how large the partitions they
/// fill are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    pub area: Area,
    /// The boot partition's bytes.
    pub boot: u64,
    /// The init_boot partition's bytes.
    pub init_boot: u64,
}

/// A boot set in an entry of the store, by the SHA-256 of each partition's
/// whole bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub entry: u32,
    pub boot: String,
    pub init_boot: String,
}

/// The other system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct System {
    pub id: String,
    pub name: String,
    pub version: String,
    /// The filesystem its root is, as the kernel names it.
    pub filesystem: String,
    /// The program its root starts with.
    pub init: String,
    pub root: Area,
    pub set: Saved,
    /// The guest's own boot store. The first guest may use the shared store.
    pub boot_store: Option<Store>,
    /// The bytes of the system's own ramdisk, which comes first in the
    /// ramdisk of its init_boot image, where it brings one.
    pub own_ramdisk: Option<u64>,
    /// The SHA-256 of the installed system's vendor_boot and dtbo when the
    /// other system was installed, which the other system starts with.
    pub vendor_boot: String,
    pub dtbo: String,
}

impl System {
    /// What Skyward's program reads of the system, to start it.
    pub fn start(&self) -> Start {
        Start { id: self.id.clone(), filesystem: self.filesystem.clone(), init: self.init.clone(), root: self.root.clone() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Switching {
    pub to: Who,
    /// The slot whose boot partitions are being written.
    pub slot: String,
    /// The installed system's boot set has been saved for this switch, so
    /// the partitions are no longer its to save again.
    pub saved: bool,
}

/// A package identifier short enough to carry by value through UI and worker events.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SystemId([u8; 32]);

impl SystemId {
    pub fn new(name: &str) -> Result<SystemId> {
        let allowed = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit() || b".-_".contains(&c);
        ensure!(!name.is_empty() && name.len() <= 32 && name.bytes().all(allowed), "invalid system id {name:?}");
        ensure!(name != "installed", "the id {name:?} names the installed system");
        let mut bytes = [0; 32];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        Ok(SystemId(bytes))
    }

    pub fn as_str(&self) -> &str {
        let end = self.0.iter().position(|&c| c == 0).unwrap_or(self.0.len());
        std::str::from_utf8(&self.0[..end]).expect("a system id is ASCII")
    }
}

impl TryFrom<String> for SystemId {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(&value)
    }
}

impl From<SystemId> for String {
    fn from(id: SystemId) -> Self {
        id.as_str().to_owned()
    }
}

impl fmt::Display for SystemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for SystemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Who {
    /// The system in the phone's own partitions.
    Installed,
    /// The system in the data partition's area.
    Guest(SystemId),
}

impl Who {
    /// How a request in misc names it: the installed system by a word, the
    /// other by its id.
    pub fn named(name: &str, state: &State) -> Option<Who> {
        if name == "installed" {
            return Some(Who::Installed);
        }
        let id = SystemId::new(name).ok()?;
        state.guest(id).map(|_| Who::Guest(id))
    }
}

impl State {
    pub(crate) fn areas(&self) -> impl Iterator<Item = &Area> {
        let roots = self.systems.iter().map(|system| &system.root);
        let stores = self.store.iter().chain(self.systems.iter().filter_map(|system| system.boot_store.as_ref()));
        roots.chain(stores.map(|store| &store.area))
    }

    pub fn guest(&self, id: SystemId) -> Option<&System> {
        self.systems.iter().find(|system| system.id == id.as_str())
    }

    pub fn starting(&self) -> Who {
        self.active.map_or(Who::Installed, Who::Guest)
    }

    fn validate(&self) -> Result<()> {
        let mut files = HashSet::new();
        for area in self.areas() {
            ensure!(files.insert(&area.file), "{} belongs to more than one system area", area.file.display());
            ensure!(area.size > skyward_block::area::HEADER && !area.extents.is_empty(), "{} has no usable system area", area.file.display());
        }
        let mut ids = HashSet::new();
        for system in &self.systems {
            let id = SystemId::new(&system.id)?;
            ensure!(ids.insert(id), "duplicate guest id {id}");
            let entry = if system.boot_store.is_some() { 0 } else { crate::store::OTHER };
            ensure!(system.set.entry == entry, "guest {id} has an invalid boot-store entry");
        }
        ensure!(self.saved.as_ref().is_none_or(|saved| saved.entry < crate::store::OTHER), "invalid main-system backup entry");
        ensure!(self.systems.is_empty() || self.store.is_some(), "the main boot store is missing");
        ensure!(self.systems.iter().filter(|s| s.boot_store.is_none()).count() <= 1, "multiple guests share a boot entry");
        ensure!(self.active.is_some() == self.occupied.is_some(), "the active guest and occupied slot disagree");
        if let Some(id) = self.active {
            ensure!(ids.contains(&id) && self.saved.is_some(), "the active guest {id} or its main-system backup is missing");
        }
        if let Some(slot) = &self.occupied {
            ensure!(skyward_vab::Slot::from_suffix(slot).is_some(), "unknown occupied slot {slot:?}");
        }
        if let Some(switch) = &self.switching {
            ensure!(skyward_vab::Slot::from_suffix(&switch.slot).is_some(), "unknown switch slot {:?}", switch.slot);
            ensure!(self.occupied.as_ref().is_none_or(|slot| *slot == switch.slot), "switch slot differs from the occupied slot");
            ensure!(switch.to != self.starting(), "the pending switch names the current system");
            ensure!(!switch.saved || self.saved.is_some(), "the pending switch has no main-system backup");
            if let Who::Guest(id) = switch.to {
                ensure!(ids.contains(&id), "pending switch names missing guest {id}");
            }
        }
        Ok(())
    }

    fn parse(text: &str) -> Result<State> {
        let mut fields: toml::Table = toml::from_str(text)?;
        match fields.remove("system") {
            Some(toml::Value::Integer(version)) => ensure!(version == 2, "unsupported systems metadata version {version}"),
            legacy => {
                ensure!(!fields.contains_key("active") && !fields.contains_key("systems"), "multi-system metadata is missing its version");
                if let Some(guest) = legacy {
                    let id = SystemId::new(guest.get("id").and_then(toml::Value::as_str).context("missing legacy system id")?)?;
                    if fields.contains_key("occupied") {
                        fields.insert("active".into(), toml::Value::String(id.to_string()));
                    }
                    if let Some(to) = fields.get_mut("switching").and_then(|switch| switch.get_mut("to")).filter(|to| to.as_str() == Some("other")) {
                        *to = toml::Value::try_from(Who::Guest(id))?;
                    }
                    fields.insert("systems".into(), toml::Value::Array(vec![guest]));
                }
            }
        }
        let state: State = toml::Value::Table(fields).try_into()?;
        state.validate()?;
        Ok(state)
    }

    /// The state kept at `path`; none kept is none installed.
    pub fn load(path: &Path) -> Result<State> {
        match fs::read_to_string(path) {
            Ok(text) => State::parse(&text).with_context(|| format!("read {}", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Keep the state at `path` whole or not at all: written beside it,
    /// synced, put in its place, and the folder synced.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("the state is kept in a folder")?;
        fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
        self.validate()?;
        // Old recovery versions require `system` to be a table. A numeric version
        // makes them reject this file before they can overwrite a guest or its store.
        let text = if self == &State::default() {
            // After the last guest and its files are gone either recovery can start fresh.
            String::new()
        } else {
            format!("system = 2\n{}", toml::to_string(self).context("write the systems' state")?)
        };
        let tmp = path.with_extension("toml.new");
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).with_context(|| format!("open {}", tmp.display()))?;
        f.write_all(text.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all().with_context(|| format!("sync {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("put {} in place of {}", tmp.display(), path.display()))?;
        File::open(dir).and_then(|d| d.sync_all()).with_context(|| format!("sync {}", dir.display()))
    }

    /// Which system's boot set the boot partitions of `slot` hold.
    pub fn occupant(&self, slot: &str) -> Who {
        match &self.occupied {
            Some(s) if s == slot => self.starting(),
            _ => Who::Installed,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn area(file: &str, size: u64) -> Area {
        Area { partition: "userdata".into(), file: file.into(), id: "00112233445566778899aabbccddeeff".into(), size, extents: vec![(1 << 20, size)] }
    }

    #[test]
    fn a_missing_or_future_layout_cannot_be_read_as_an_empty_phone() {
        for text in ["system = 3", "systems = []", "occupied = \"_a\"", "system = 2\nactive = \"missing\""] {
            assert!(State::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_state_is_kept_whole_and_read_back_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skyward/systems.toml");
        assert_eq!(State::load(&path).unwrap(), State::default(), "nothing kept, nothing installed");
        let state = State {
            store: Some(Store { area: area("/data/skyward/systems/store", 4096 + 3 * 8192), boot: 4096, init_boot: 4096 }),
            saved: Some(Saved { entry: 1, boot: "aa".into(), init_boot: "bb".into() }),
            systems: vec![System {
                id: "test".into(),
                name: "Test".into(),
                version: "1".into(),
                filesystem: "ext4".into(),
                init: "/sbin/init".into(),
                root: area("/data/skyward/systems/test", 1 << 20),
                set: Saved { entry: 2, boot: "cc".into(), init_boot: "dd".into() },
                boot_store: None,
                own_ramdisk: Some(4096),
                vendor_boot: "ee".into(),
                dtbo: "ff".into(),
            }],
            active: Some(SystemId::new("test").unwrap()),
            occupied: Some("_a".into()),
            written: Some(Written { boot: "cc".into(), init_boot: "11".into() }),
            pending_files: Vec::new(),
            switching: Some(Switching { to: Who::Installed, slot: "_a".into(), saved: false }),
        };
        state.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), state);
        let mut malformed = state.clone();
        malformed.systems[0].root.file = state.store.as_ref().unwrap().area.file.clone();
        assert!(malformed.save(&path).is_err(), "a root cannot share the backup's file");
        malformed.systems[0].root = state.systems[0].root.clone();
        malformed.systems[0].root.extents.clear();
        assert!(malformed.save(&path).is_err(), "a missing extent cannot locate the header");
        assert_eq!(State::load(&path).unwrap(), state, "the valid catalog stays intact");
        // Old versions expect the singular system table. Both an old active guest
        // and an interrupted switch targeting the sole guest retain its identity.
        let mut legacy: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let guest = legacy.remove("systems").unwrap().as_array().unwrap()[0].clone();
        legacy.insert("system".into(), guest);
        legacy.remove("active");
        fs::write(&path, toml::to_string(&legacy).unwrap()).unwrap();
        assert_eq!(State::load(&path).unwrap(), state);
        legacy.remove("occupied");
        legacy.remove("written");
        legacy.get_mut("switching").unwrap()["to"] = toml::Value::String("other".into());
        fs::write(&path, toml::to_string(&legacy).unwrap()).unwrap();
        let migrated = State::load(&path).unwrap();
        assert_eq!(migrated.starting(), Who::Installed);
        assert_eq!(migrated.switching.unwrap().to, Who::Guest(SystemId::new("test").unwrap()));
        // A state an earlier Skyward kept says nothing of what was written,
        // and reads as one that has none.
        let earlier = State { written: None, ..state.clone() };
        earlier.save(&path).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("written"));
        assert_eq!(State::load(&path).unwrap(), earlier);
        assert!(!path.with_extension("toml.new").exists(), "nothing left beside it");
        assert_eq!((state.occupant("_a"), state.occupant("_b")), (Who::Guest(SystemId::new("test").unwrap()), Who::Installed));
        assert_eq!(Who::named("installed", &state), Some(Who::Installed));
        assert_eq!(Who::named("test", &state), Some(Who::Guest(SystemId::new("test").unwrap())));
        assert_eq!(Who::named("other", &state), None);
        State::default().save(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
        assert_eq!(State::load(&path).unwrap(), State::default());
    }
}
