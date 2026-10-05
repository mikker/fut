//! Saved SSH machine profiles: a private, versioned catalog of remote endpoints.
//!
//! A profile stores only an opaque identity, a unique label, the SSH target,
//! and whether it is enabled. Credentials, keys, agent and control sockets
//! stay with OpenSSH. Nothing here contacts a daemon or SSH.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const FILE_NAME: &str = "machines.toml";
const CATALOG_VERSION: u8 = 1;
const MAX_CATALOG_BYTES: u64 = 256 * 1024;
pub(crate) const MAX_MACHINES: usize = 256;
pub(crate) const MAX_LABEL_BYTES: usize = 64;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Machine {
    pub(crate) id: Uuid,
    pub(crate) label: String,
    pub(crate) target: String,
    pub(crate) enabled: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CatalogFile {
    version: u8,
    #[serde(default)]
    machines: Vec<Machine>,
}

#[derive(Debug)]
pub(crate) struct Change {
    pub(crate) machine: Machine,
    pub(crate) changed: bool,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("no saved machine matches {0:?}")]
    NotFound(String),
    #[error("a saved machine is already labeled {0:?}")]
    LabelTaken(String),
    #[error("the machine catalog cannot contain more than {MAX_MACHINES} machines")]
    Full,
    #[error("{0:#}")]
    Invalid(anyhow::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub(crate) struct Catalog {
    path: PathBuf,
}

impl Catalog {
    pub(crate) fn resolve() -> Result<Self> {
        Ok(Self::at(crate::state_file::path(FILE_NAME)?))
    }

    pub(crate) fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn list(&self) -> Result<Vec<Machine>> {
        let Some(text) = crate::state_file::read(&self.path, MAX_CATALOG_BYTES)
            .with_context(|| format!("read machine catalog {}", self.path.display()))?
        else {
            return Ok(Vec::new());
        };
        let file = toml::from_str::<CatalogFile>(&text)
            .with_context(|| format!("parse machine catalog {}", self.path.display()))?;
        validate_catalog(&file)
            .with_context(|| format!("validate machine catalog {}", self.path.display()))?;
        Ok(file.machines)
    }

    /// Explicit attachment accepts saved profiles (even disabled ones), or a raw SSH host.
    pub(crate) fn attachment_target(&self, selector: &str) -> Result<String> {
        match self.find(selector) {
            Ok(machine) => Ok(machine.target),
            Err(Error::NotFound(_)) => {
                crate::ssh_bridge::validate_destination(selector)?;
                Ok(selector.to_owned())
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn find(&self, selector: &str) -> Result<Machine, Error> {
        find(&self.list()?, selector).cloned()
    }

    /// Rejects an addition early, before any slow remote validation. `add`
    /// repeats the check under the lock because the catalog may change meanwhile.
    pub(crate) fn check_new(&self, label: &str, target: &str) -> Result<(), Error> {
        validate_profile(label, target)?;
        check_new(&self.list()?, label)
    }

    #[cfg(test)]
    fn add(&self, label: &str, target: &str) -> Result<Machine, Error> {
        validate_profile(label, target)?;
        self.mutate(|machines| add(machines, label, target))
    }

    /// Waits for a concurrent catalog writer without blocking signal handling.
    /// Once the lock is acquired, the short read-modify-write is the commit point.
    pub(crate) async fn add_async(&self, label: &str, target: &str) -> Result<Machine, Error> {
        validate_profile(label, target)?;
        let _lock = crate::state_file::Lock::acquire_async(&self.path)
            .await
            .with_context(|| format!("lock machine catalog {}", self.path.display()))?;
        self.mutate_locked(|machines| add(machines, label, target))
    }

    fn mutate_locked<T>(
        &self,
        operation: impl FnOnce(&mut Vec<Machine>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let before = self.list()?;
        let mut machines = before.clone();
        let result = operation(&mut machines)?;
        if machines != before {
            let file = CatalogFile {
                version: CATALOG_VERSION,
                machines,
            };
            validate_catalog(&file)?;
            let contents = toml::to_string_pretty(&file).context("serialize machine catalog")?;
            crate::state_file::write(&self.path, &contents, MAX_CATALOG_BYTES)
                .with_context(|| format!("write machine catalog {}", self.path.display()))?;
        }
        Ok(result)
    }

    pub(crate) fn rename(&self, selector: &str, label: &str) -> Result<Change, Error> {
        validate_label(label).map_err(Error::Invalid)?;
        self.mutate(|machines| {
            let id = find(machines, selector)?.id;
            if machines
                .iter()
                .any(|machine| machine.id != id && machine.label == label)
            {
                return Err(Error::LabelTaken(label.to_owned()));
            }
            let machine = by_id(machines, id);
            let changed = machine.label != label;
            machine.label = label.to_owned();
            Ok(Change {
                machine: machine.clone(),
                changed,
            })
        })
    }

    pub(crate) fn set_enabled(&self, selector: &str, enabled: bool) -> Result<Change, Error> {
        self.mutate(|machines| {
            let id = find(machines, selector)?.id;
            let machine = by_id(machines, id);
            let changed = machine.enabled != enabled;
            machine.enabled = enabled;
            Ok(Change {
                machine: machine.clone(),
                changed,
            })
        })
    }

    /// Forgets the profile only. The remote daemon and its panes are untouched.
    pub(crate) fn remove(&self, selector: &str) -> Result<Machine, Error> {
        self.mutate(|machines| {
            let id = find(machines, selector)?.id;
            let index = machines
                .iter()
                .position(|machine| machine.id == id)
                .expect("found machine");
            Ok(machines.remove(index))
        })
    }

    fn mutate<T>(
        &self,
        operation: impl FnOnce(&mut Vec<Machine>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let _lock = crate::state_file::Lock::acquire(&self.path)
            .with_context(|| format!("lock machine catalog {}", self.path.display()))?;
        self.mutate_locked(operation)
    }
}

fn add(machines: &mut Vec<Machine>, label: &str, target: &str) -> Result<Machine, Error> {
    check_new(machines, label)?;
    let machine = Machine {
        id: Uuid::new_v4(),
        label: label.to_owned(),
        target: target.to_owned(),
        enabled: true,
    };
    machines.push(machine.clone());
    Ok(machine)
}

fn check_new(machines: &[Machine], label: &str) -> Result<(), Error> {
    if machines.iter().any(|machine| machine.label == label) {
        return Err(Error::LabelTaken(label.to_owned()));
    }
    if machines.len() >= MAX_MACHINES {
        return Err(Error::Full);
    }
    Ok(())
}

/// Selects by UUID when the selector parses as one, otherwise by exact label.
fn find<'a>(machines: &'a [Machine], selector: &str) -> Result<&'a Machine, Error> {
    let id = Uuid::parse_str(selector).ok();
    machines
        .iter()
        .find(|machine| id.is_some_and(|id| machine.id == id) || machine.label == selector)
        .ok_or_else(|| Error::NotFound(selector.to_owned()))
}

fn by_id(machines: &mut [Machine], id: Uuid) -> &mut Machine {
    machines
        .iter_mut()
        .find(|machine| machine.id == id)
        .expect("found machine")
}

fn validate_profile(label: &str, target: &str) -> Result<(), Error> {
    validate_label(label)
        .and_then(|()| crate::ssh_bridge::validate_destination(target))
        .map_err(Error::Invalid)
}

fn validate_label(label: &str) -> Result<()> {
    if label.is_empty() || label.len() > MAX_LABEL_BYTES {
        bail!("machine label must be 1 to {MAX_LABEL_BYTES} bytes");
    }
    if label.starts_with('-') {
        bail!("machine label must not start with a dash");
    }
    if label
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        bail!("machine label must not contain whitespace or control characters");
    }
    if Uuid::parse_str(label).is_ok() {
        bail!("machine label must not be a UUID");
    }
    Ok(())
}

fn validate_catalog(file: &CatalogFile) -> Result<()> {
    if file.version != CATALOG_VERSION {
        bail!(
            "unsupported machine catalog version {}; expected {CATALOG_VERSION}",
            file.version
        );
    }
    if file.machines.len() > MAX_MACHINES {
        bail!("machine catalog contains more than {MAX_MACHINES} machines");
    }
    let mut ids = HashSet::with_capacity(file.machines.len());
    let mut labels = HashSet::with_capacity(file.machines.len());
    for machine in &file.machines {
        validate_label(&machine.label)?;
        crate::ssh_bridge::validate_destination(&machine.target)?;
        if !ids.insert(machine.id) {
            bail!("machine catalog contains duplicate id {}", machine.id);
        }
        if !labels.insert(machine.label.as_str()) {
            bail!(
                "machine catalog contains duplicate label {:?}",
                machine.label
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::*;

    fn catalog() -> (tempfile::TempDir, Catalog) {
        let temporary = tempfile::tempdir().unwrap();
        let catalog = Catalog::at(temporary.path().join("state/fut/machines.toml"));
        (temporary, catalog)
    }

    #[test]
    fn profiles_have_stable_ids_unique_labels_and_shared_targets() {
        let (_temporary, catalog) = catalog();
        assert!(catalog.list().unwrap().is_empty());
        assert!(!catalog.path().exists());

        let work = catalog.add("work", "user@work.example").unwrap();
        let again = catalog.add("work-again", "user@work.example").unwrap();
        assert_ne!(work.id, again.id);
        assert!(work.enabled);
        assert!(matches!(
            catalog.add("work", "other"),
            Err(Error::LabelTaken(label)) if label == "work"
        ));
        assert!(matches!(
            catalog.check_new("work", "other"),
            Err(Error::LabelTaken(_))
        ));
        catalog.check_new("fresh", "other").unwrap();
        assert!(!catalog.list().unwrap().iter().any(|m| m.label == "fresh"));

        let renamed = catalog.rename(&work.id.to_string(), "office").unwrap();
        assert!(renamed.changed);
        assert_eq!(renamed.machine.id, work.id);
        assert!(!catalog.rename("office", "office").unwrap().changed);
        assert!(matches!(
            catalog.rename("office", "work-again"),
            Err(Error::LabelTaken(_))
        ));
        assert_eq!(catalog.find("office").unwrap().target, "user@work.example");
        assert!(matches!(
            catalog.find("work"),
            Err(Error::NotFound(selector)) if selector == "work"
        ));

        assert!(catalog.set_enabled("office", false).unwrap().changed);
        assert!(!catalog.set_enabled("office", false).unwrap().changed);
        assert!(!catalog.find("office").unwrap().enabled);
        assert!(
            catalog
                .set_enabled(&work.id.to_string(), true)
                .unwrap()
                .changed
        );

        assert_eq!(catalog.remove("office").unwrap().id, work.id);
        assert!(matches!(catalog.remove("office"), Err(Error::NotFound(_))));
        let remaining = catalog.list().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, again.id);

        let path = catalog.path();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let text = fs::read_to_string(path).unwrap();
        assert!(text.starts_with("version = 1\n"), "{text}");
        assert!(text.contains("[[machines]]"), "{text}");
        assert!(path.with_extension("lock").is_file());
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 2);
    }

    #[test]
    fn attachment_resolves_profiles_and_falls_back_to_ssh_hosts() {
        let (_temporary, catalog) = catalog();
        assert_eq!(catalog.attachment_target("clonk").unwrap(), "clonk");
        let machine = catalog.add("work", "user@clonk").unwrap();
        assert_eq!(catalog.attachment_target("work").unwrap(), "user@clonk");
        assert_eq!(
            catalog.attachment_target(&machine.id.to_string()).unwrap(),
            "user@clonk"
        );
        catalog.set_enabled("work", false).unwrap();
        assert_eq!(catalog.attachment_target("work").unwrap(), "user@clonk");
        assert_eq!(catalog.attachment_target("other").unwrap(), "other");
        assert!(catalog.attachment_target("-oProxyCommand=evil").is_err());
        fs::write(catalog.path(), "invalid catalog").unwrap();
        assert!(catalog.attachment_target("other").is_err());
    }

    #[test]
    fn labels_and_targets_are_strictly_bounded() {
        for label in ["work", "user@host", "clonk.local", "a"] {
            validate_label(label).unwrap();
        }
        for label in [
            "",
            "-work",
            "two words",
            "tab\there",
            "line\nbreak",
            "nul\0",
            &"x".repeat(MAX_LABEL_BYTES + 1),
            &Uuid::new_v4().to_string(),
        ] {
            assert!(validate_label(label).is_err(), "{label:?}");
        }
        let (_temporary, catalog) = catalog();
        for target in [
            "",
            "-oProxyCommand=evil",
            "ssh://host",
            "user:password@host",
            "host\n",
            "host name",
        ] {
            assert!(catalog.add("label", target).is_err(), "{target:?}");
            assert!(catalog.check_new("label", target).is_err(), "{target:?}");
        }
        catalog.check_new("unicode", "hôte").unwrap();
        assert!(!catalog.path().exists());
    }

    #[test]
    fn catalog_is_bounded() {
        let (_temporary, catalog) = catalog();
        for index in 0..MAX_MACHINES {
            catalog.add(&format!("machine-{index}"), "host").unwrap();
        }
        assert!(matches!(catalog.add("one-more", "host"), Err(Error::Full)));
        assert!(matches!(
            catalog.check_new("one-more", "host"),
            Err(Error::Full)
        ));
        assert_eq!(catalog.list().unwrap().len(), MAX_MACHINES);
    }

    #[test]
    fn malformed_or_unsafe_catalogs_are_rejected_whole() {
        let (_temporary, catalog) = catalog();
        let id = Uuid::new_v4();
        let entry = |label: &str, target: &str| {
            format!(
                "[[machines]]\nid = \"{id}\"\nlabel = \"{label}\"\ntarget = \"{target}\"\nenabled = true\n"
            )
        };
        for (text, message) in [
            ("version = 2\n", "version"),
            ("version = 1\nunknown = true\n", "unknown"),
            (
                &format!("version = 1\n{}", entry("work", "user:secret@host")),
                "password",
            ),
            (
                &format!("version = 1\n{}", entry("work", "ssh://host")),
                "URI",
            ),
            (
                &format!(
                    "version = 1\n{}{}",
                    entry("work", "host"),
                    entry("work", "other")
                ),
                "duplicate",
            ),
            (
                &format!(
                    "version = 1\n{}password = \"secret\"\n",
                    entry("work", "host")
                ),
                "password",
            ),
        ] {
            fs::create_dir_all(catalog.path().parent().unwrap()).unwrap();
            fs::write(catalog.path(), text).unwrap();
            fs::set_permissions(catalog.path(), fs::Permissions::from_mode(0o600)).unwrap();
            let error = format!("{:#}", catalog.list().unwrap_err());
            assert!(error.contains(message), "{text}: {error}");
            assert!(catalog.add("new", "host").is_err());
            assert_eq!(fs::read_to_string(catalog.path()).unwrap(), text);
        }
        fs::set_permissions(catalog.path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(format!("{:#}", catalog.list().unwrap_err()).contains("0600"));
    }
}
