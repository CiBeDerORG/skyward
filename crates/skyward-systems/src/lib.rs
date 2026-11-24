//! Guest systems alongside the installed system. Each guest has a fixed
//! root area in the data partition and a saved kernel and ramdisk. Switching
//! writes the selected boot set into the slot's boot and init_boot partitions.

use std::collections::HashMap;

pub mod boot;
pub mod install;
pub mod package;
pub mod state;
pub mod store;
pub mod switch;

pub use state::{State, SystemId, Who};
pub use switch::Held;

/// What the pages show of the systems.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overview {
    /// The system installed in the phone's own partitions, as it names
    /// itself.
    pub installed: Named,
    pub others: Vec<Other>,
    /// The system whose boot images are currently in the partitions.
    pub starting: Who,
    /// What the boot partitions hold, set against what is kept of them.
    pub held: Held,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub name: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Other {
    pub id: SystemId,
    pub name: String,
    pub version: String,
    /// The bytes its root takes in the data partition.
    pub size: u64,
    /// The vendor_boot or dtbo this guest uses changed since installation.
    pub changed: bool,
}

/// What the installed system calls itself, by its properties: HyperOS by
/// the major version Xiaomi gives it, any other by its Android version;
/// with the build it is, where it says.
pub fn installed_name(props: &HashMap<String, String>, version_prop: &str) -> Named {
    let name = match (props.get("ro.mi.os.version.code"), props.get("ro.build.version.release")) {
        (Some(code), _) => format!("HyperOS {code}"),
        (None, Some(release)) => format!("Android {release}"),
        (None, None) => "Android".into(),
    };
    let version = props.get(version_prop).or_else(|| props.get("ro.build.display.id")).cloned();
    Named { name, version }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn the_installed_system_is_named_as_it_names_itself() {
        let hyper = props(&[("ro.mi.os.version.code", "3"), ("ro.build.version.release", "16"), ("ro.mi.os.version.incremental", "OS3.0.308.0.WMACNXM")]);
        assert_eq!(installed_name(&hyper, "ro.mi.os.version.incremental"), Named { name: "HyperOS 3".into(), version: Some("OS3.0.308.0.WMACNXM".into()) });
        let custom = props(&[("ro.build.version.release", "16"), ("ro.build.display.id", "lineage_ishtar-userdebug 16 BP2A")]);
        assert_eq!(
            installed_name(&custom, "ro.mi.os.version.incremental"),
            Named { name: "Android 16".into(), version: Some("lineage_ishtar-userdebug 16 BP2A".into()) }
        );
    }
}
