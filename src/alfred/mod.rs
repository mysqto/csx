//! Alfred workflow integration: the embedded csx Tabs workflow and the logic
//! that installs it into the Alfred preferences folder.

use crate::error::{Error, Result};
use crate::tabs::CommandRunner;
use std::path::{Path, PathBuf};

/// Folder name of the csx Tabs workflow.
pub const WORKFLOW_DIR: &str = "user.workflow.csx-tabs";
/// Folder name of the older Claude Tabs workflow.
pub const OLD_WORKFLOW_DIR: &str = "user.workflow.claude-tabs";
/// Bundle id of the older Claude Tabs workflow.
pub const OLD_BUNDLE_ID: &str = "to.mysq.claude-tabs";

/// The embedded workflow files: name and content.
pub const FILES: &[(&str, &[u8])] = &[
    (
        "info.plist",
        include_bytes!("../../contrib/alfred/csx-tabs/info.plist"),
    ),
    (
        "icon.png",
        include_bytes!("../../contrib/alfred/csx-tabs/icon.png"),
    ),
    (
        "csx.sh",
        include_bytes!("../../contrib/alfred/csx-tabs/csx.sh"),
    ),
];

const DEFAULTS_DOMAIN: &str = "com.runningwithcrayons.Alfred-Preferences";
const PREFS_NAME: &str = "Alfred.alfredpreferences";

/// Find the Alfred preferences folder: the sync folder when one is set,
/// otherwise the default folder under `home`.
pub fn prefs_dir(cmd: &dyn CommandRunner, home: &Path) -> PathBuf {
    if let Ok(out) = cmd.run("defaults", &["read", DEFAULTS_DOMAIN, "syncfolder"]) {
        let folder = out.trim();
        if !folder.is_empty() {
            let folder = match folder.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None if folder == "~" => home.to_path_buf(),
                None => PathBuf::from(folder),
            };
            return folder.join(PREFS_NAME);
        }
    }
    home.join("Library/Application Support/Alfred")
        .join(PREFS_NAME)
}

/// What `install` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// The folder the workflow was written to.
    pub dir: PathBuf,
    /// The keyword carried over from the old workflow, if any.
    pub migrated_keyword: Option<String>,
    /// Whether the old workflow folder was removed.
    pub removed_old: bool,
}

/// Find the string after `<key>keyword</key>` in a plist.
fn old_keyword(plist: &str) -> Option<String> {
    let rest = plist.split("<key>keyword</key>").nth(1)?;
    let rest = rest.trim_start().strip_prefix("<string>")?;
    let value = rest.split("</string>").next()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Write the workflow into `prefs` and migrate the older workflow.
pub fn install(prefs: &Path) -> Result<InstallReport> {
    let workflows = prefs.join("workflows");
    if !workflows.is_dir() {
        return Err(Error::Other(format!(
            "no workflows folder in {}",
            prefs.display()
        )));
    }
    let dir = workflows.join(WORKFLOW_DIR);
    std::fs::create_dir_all(&dir)?;
    for (name, bytes) in FILES {
        let path = dir.join(name);
        std::fs::write(&path, bytes)?;
        if *name == "csx.sh" {
            set_exec(&path)?;
        }
    }
    let mut report = InstallReport {
        dir: dir.clone(),
        migrated_keyword: None,
        removed_old: false,
    };
    let old = workflows.join(OLD_WORKFLOW_DIR);
    let marker = format!("<string>{OLD_BUNDLE_ID}</string>");
    let is_old = std::fs::read_to_string(old.join("info.plist"))
        .map(|s| s.contains(&marker))
        .unwrap_or(false);
    if !is_old {
        return Ok(report);
    }
    let new_prefs = dir.join("prefs.plist");
    if !new_prefs.exists() {
        let keyword = std::fs::read_to_string(old.join("prefs.plist"))
            .ok()
            .and_then(|s| old_keyword(&s))
            .map(|k| xml_unescape(&k));
        if let Some(k) = keyword {
            let plist = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\t<key>agents_keyword</key>\n\t<string>{}</string>\n</dict>\n</plist>\n",
                xml_escape(&k)
            );
            std::fs::write(&new_prefs, plist)?;
            report.migrated_keyword = Some(k);
        }
    }
    std::fs::remove_dir_all(&old)?;
    report.removed_old = true;
    Ok(report)
}

fn set_exec(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;

    struct Fake(std::result::Result<String, ()>, RefCell<Vec<String>>);
    impl CommandRunner for Fake {
        fn run(&self, program: &str, args: &[&str]) -> Result<String> {
            self.1
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            self.0.clone().map_err(|_| Error::Other("no".into()))
        }
    }
    fn fake(r: std::result::Result<&str, ()>) -> Fake {
        Fake(r.map(String::from), RefCell::new(vec![]))
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("csx-alfred-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn prefs_with_workflows(name: &str) -> PathBuf {
        let d = tmp(name);
        std::fs::create_dir_all(d.join("workflows")).unwrap();
        d
    }
    fn old_workflow(prefs: &Path, bundle: &str, keyword: Option<&str>) -> PathBuf {
        let old = prefs.join("workflows").join(OLD_WORKFLOW_DIR);
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(
            old.join("info.plist"),
            format!("<plist><dict><key>bundleid</key><string>{bundle}</string></dict></plist>"),
        )
        .unwrap();
        if let Some(k) = keyword {
            std::fs::write(
                old.join("prefs.plist"),
                format!("<plist><dict><key>keyword</key><string>{k}</string></dict></plist>"),
            )
            .unwrap();
        }
        old
    }

    #[test]
    fn prefs_dir_uses_syncfolder_with_tilde() {
        let c = fake(Ok("~/Sync/Alfred\n"));
        let home = Path::new("/h");
        assert_eq!(
            prefs_dir(&c, home),
            PathBuf::from("/h/Sync/Alfred/Alfred.alfredpreferences")
        );
        assert_eq!(
            c.1.borrow()[0],
            "defaults read com.runningwithcrayons.Alfred-Preferences syncfolder"
        );
        let c = fake(Ok("/abs/Alfred"));
        assert_eq!(
            prefs_dir(&c, home),
            PathBuf::from("/abs/Alfred/Alfred.alfredpreferences")
        );
        let c = fake(Ok("~"));
        assert_eq!(
            prefs_dir(&c, home),
            PathBuf::from("/h/Alfred.alfredpreferences")
        );
    }

    #[test]
    fn prefs_dir_falls_back_to_default() {
        let want = PathBuf::from("/h/Library/Application Support/Alfred/Alfred.alfredpreferences");
        assert_eq!(prefs_dir(&fake(Err(())), Path::new("/h")), want);
        assert_eq!(prefs_dir(&fake(Ok("  \n")), Path::new("/h")), want);
    }

    #[test]
    fn install_writes_all_files_and_exec_bit() {
        let p = prefs_with_workflows("write");
        let r = install(&p).unwrap();
        assert_eq!(r.dir, p.join("workflows").join(WORKFLOW_DIR));
        assert_eq!(r.migrated_keyword, None);
        assert!(!r.removed_old);
        for (name, bytes) in FILES {
            assert_eq!(&std::fs::read(r.dir.join(name)).unwrap(), bytes);
        }
        let mode = std::fs::metadata(r.dir.join("csx.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        // Re-install is fine.
        install(&p).unwrap();
    }

    #[test]
    fn install_keeps_existing_prefs_plist() {
        let p = prefs_with_workflows("keep");
        let dir = p.join("workflows").join(WORKFLOW_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("prefs.plist"), "mine").unwrap();
        install(&p).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("prefs.plist")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn install_without_workflows_errors() {
        let p = tmp("none");
        let e = install(&p).unwrap_err().to_string();
        assert_eq!(e, format!("no workflows folder in {}", p.display()));
    }

    #[test]
    fn migration_moves_keyword_and_removes_old() {
        let p = prefs_with_workflows("mig");
        let old = old_workflow(&p, OLD_BUNDLE_ID, Some("x&amp;y"));
        let r = install(&p).unwrap();
        assert_eq!(r.migrated_keyword.as_deref(), Some("x&y"));
        assert!(r.removed_old);
        assert!(!old.exists());
        let s = std::fs::read_to_string(r.dir.join("prefs.plist")).unwrap();
        assert!(s.contains("<key>agents_keyword</key>"));
        assert!(s.contains("<string>x&amp;y</string>"));
    }

    #[test]
    fn migration_without_old_keyword_still_removes_old() {
        let p = prefs_with_workflows("nokw");
        let old = old_workflow(&p, OLD_BUNDLE_ID, None);
        let r = install(&p).unwrap();
        assert_eq!(r.migrated_keyword, None);
        assert!(r.removed_old);
        assert!(!old.exists());
        assert!(!r.dir.join("prefs.plist").exists());
        // Empty and malformed keyword values are ignored.
        for body in [
            "<key>keyword</key><string></string>",
            "<key>keyword</key><integer>1</integer>",
        ] {
            let p = prefs_with_workflows("badkw");
            let old = old_workflow(&p, OLD_BUNDLE_ID, None);
            std::fs::write(old.join("prefs.plist"), body).unwrap();
            let r = install(&p).unwrap();
            assert_eq!(r.migrated_keyword, None);
            assert!(r.removed_old);
        }
    }

    #[test]
    fn migration_ignores_other_bundle_ids() {
        let p = prefs_with_workflows("other");
        let old = old_workflow(&p, "someone.else", Some("zz"));
        let r = install(&p).unwrap();
        assert_eq!(r.migrated_keyword, None);
        assert!(!r.removed_old);
        assert!(old.exists());
        assert!(!r.dir.join("prefs.plist").exists());
    }

    #[test]
    fn migration_does_not_override_existing_new_prefs() {
        let p = prefs_with_workflows("noover");
        let old = old_workflow(&p, OLD_BUNDLE_ID, Some("zz"));
        let dir = p.join("workflows").join(WORKFLOW_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("prefs.plist"), "mine").unwrap();
        let r = install(&p).unwrap();
        assert_eq!(r.migrated_keyword, None);
        assert!(r.removed_old);
        assert!(!old.exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("prefs.plist")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn files_embed_valid_info_plist() {
        let (_, bytes) = FILES.iter().find(|(n, _)| *n == "info.plist").unwrap();
        let s = std::str::from_utf8(bytes).unwrap();
        for want in [
            "to.mysq.csx-tabs",
            "<string>{var:tabs_keyword}</string>",
            "<string>{var:agents_keyword}</string>",
            "<string>tabs_keyword</string>",
            "<string>agents_keyword</string>",
            "<string>csx_path</string>",
            "./csx.sh tabs --alfred",
            "./csx.sh tabs --agents --alfred",
            "./csx.sh tabs focus",
            "alfred.workflow.output.clipboard",
            "alfred.workflow.trigger.hotkey",
        ] {
            assert!(s.contains(want), "missing {want}");
        }
        // Destination uids of the four connections into R and C, plus H1/H2 -> A/B.
        for n in 1..=6 {
            let uid = format!("6C5D0A00-7A11-4B2E-9C3D-00000000000{n}");
            assert!(s.contains(&format!("<string>{uid}</string>")), "uid {n}");
        }
        assert_eq!(s.matches("<key>destinationuid</key>").count(), 8);
        let (_, sh) = FILES.iter().find(|(n, _)| *n == "csx.sh").unwrap();
        assert!(sh.starts_with(b"#!/bin/sh"));
        let (_, png) = FILES.iter().find(|(n, _)| *n == "icon.png").unwrap();
        assert!(png.starts_with(b"\x89PNG"));
    }
}
