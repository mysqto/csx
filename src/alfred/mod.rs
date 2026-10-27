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

const INFO_PLIST: &[u8] = include_bytes!("../../contrib/alfred/csx-tabs/info.plist");

/// The embedded workflow files: name and content.
pub const FILES: &[(&str, &[u8])] = &[
    ("info.plist", INFO_PLIST),
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

/// The preferences folder to install into: `prefs` when given, else the one
/// found for `home`. Without both, an error.
pub fn resolve_prefs(
    prefs: Option<PathBuf>,
    cmd: &dyn CommandRunner,
    home: Option<&Path>,
) -> Result<PathBuf> {
    match (prefs, home) {
        (Some(p), _) => Ok(p),
        (None, Some(h)) => Ok(prefs_dir(cmd, h)),
        (None, None) => Err(Error::other(
            "cannot find the Alfred preferences: HOME is not set (use --prefs)",
        )),
    }
}

/// Uid of the hotkey trigger that opens the "Terminal tabs" script filter.
pub const TABS_UID: &str = "6C5D0A00-7A11-4B2E-9C3D-000000000005";
/// Uid of the hotkey trigger that opens the "AI sessions" script filter.
pub const AGENTS_UID: &str = "6C5D0A00-7A11-4B2E-9C3D-000000000006";

const MOD_SHIFT: u32 = 131_072;
const MOD_CTRL: u32 = 262_144;
const MOD_ALT: u32 = 524_288;
const MOD_CMD: u32 = 1_048_576;

/// macOS virtual keycodes (kVK_ANSI) for the keys a spec may name.
const KEYS: &[(&str, u32)] = &[
    ("a", 0),
    ("s", 1),
    ("d", 2),
    ("f", 3),
    ("h", 4),
    ("g", 5),
    ("z", 6),
    ("x", 7),
    ("c", 8),
    ("v", 9),
    ("b", 11),
    ("q", 12),
    ("w", 13),
    ("e", 14),
    ("r", 15),
    ("y", 16),
    ("t", 17),
    ("1", 18),
    ("2", 19),
    ("3", 20),
    ("4", 21),
    ("6", 22),
    ("5", 23),
    ("9", 25),
    ("7", 26),
    ("8", 28),
    ("0", 29),
    ("o", 31),
    ("u", 32),
    ("i", 34),
    ("p", 35),
    ("l", 37),
    ("j", 38),
    ("k", 40),
    ("n", 45),
    ("m", 46),
    ("space", 49),
];

/// A hotkey as Alfred stores it in a hotkey trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotkey {
    /// macOS virtual keycode (`hotkey`).
    pub keycode: u32,
    /// Modifier flags (`hotmod`).
    pub mods: u32,
    /// Display string (`hotstring`): an uppercase letter or digit, or `Space`.
    pub string: String,
}

impl Hotkey {
    /// Parse a spec such as `alt+a` or `cmd+shift+space`. At least one
    /// modifier is required.
    pub fn parse(spec: &str) -> Result<Hotkey> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(Error::other("empty hotkey"));
        }
        let parts: Vec<String> = spec.split('+').map(|p| p.trim().to_lowercase()).collect();
        if parts.iter().any(String::is_empty) {
            return Err(Error::Other(format!("empty part in hotkey '{spec}'")));
        }
        let (key, modifiers) = parts.split_last().expect("split of a non-empty string");
        let mut mods = 0;
        for m in modifiers {
            mods |= match m.as_str() {
                "shift" => MOD_SHIFT,
                "ctrl" | "control" => MOD_CTRL,
                "alt" | "opt" | "option" => MOD_ALT,
                "cmd" | "command" => MOD_CMD,
                other => {
                    return Err(Error::Other(format!(
                        "unknown modifier '{other}' in hotkey '{spec}' \
(use shift, ctrl, alt, cmd)"
                    )))
                }
            };
        }
        let Some(&(_, keycode)) = KEYS.iter().find(|(k, _)| k == key) else {
            return Err(Error::Other(format!(
                "unknown key '{key}' in hotkey '{spec}'"
            )));
        };
        if mods == 0 {
            return Err(Error::Other(format!(
                "hotkey '{spec}' needs a modifier (shift, ctrl, alt or cmd)"
            )));
        }
        let string = if key == "space" {
            "Space".to_string()
        } else {
            key.to_uppercase()
        };
        Ok(Hotkey {
            keycode,
            mods,
            string,
        })
    }
}

impl std::fmt::Display for Hotkey {
    /// Mac style, for example `\u{2325}A`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (flag, symbol) in [
            (MOD_CTRL, "\u{2303}"),
            (MOD_ALT, "\u{2325}"),
            (MOD_SHIFT, "\u{21e7}"),
            (MOD_CMD, "\u{2318}"),
        ] {
            if self.mods & flag != 0 {
                f.write_str(symbol)?;
            }
        }
        f.write_str(&self.string)
    }
}

/// Hotkeys to set at install time; `None` leaves a trigger as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hotkeys {
    /// Hotkey for "Terminal tabs".
    pub tabs: Option<Hotkey>,
    /// Hotkey for "AI sessions".
    pub agents: Option<Hotkey>,
}

/// The state of one hotkey trigger after an install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyOutcome {
    /// No hotkey is assigned.
    Unset,
    /// Set by a flag.
    Set(Hotkey),
    /// Kept from the previous install.
    Kept(Hotkey),
}

impl HotkeyOutcome {
    fn hotkey(&self) -> Option<&Hotkey> {
        match self {
            HotkeyOutcome::Unset => None,
            HotkeyOutcome::Set(h) | HotkeyOutcome::Kept(h) => Some(h),
        }
    }
}

/// The text of the trigger object that has `uid`: from its `config` key to
/// the `uid` value. Connections name a uid as `destinationuid`, so only a
/// `<key>uid</key>` followed by the value matches.
fn trigger_config(plist: &str, uid: &str) -> Option<(usize, usize)> {
    let value = format!("<string>{uid}</string>");
    let mut from = 0;
    while let Some(i) = plist[from..].find(&value) {
        let at = from + i;
        from = at + value.len();
        if plist[..at].trim_end().ends_with("<key>uid</key>") {
            let start = plist[..at].rfind("<key>config</key>")?;
            return Some((start, at));
        }
    }
    None
}

fn int_after(text: &str, key: &str) -> Option<u32> {
    let rest = text.split(&format!("<key>{key}</key>")).nth(1)?;
    let rest = rest.trim_start().strip_prefix("<integer>")?;
    rest.split("</integer>").next()?.trim().parse().ok()
}

fn string_after(text: &str, key: &str) -> Option<String> {
    let rest = text.split(&format!("<key>{key}</key>")).nth(1)?;
    let rest = rest.trim_start().strip_prefix("<string>")?;
    Some(xml_unescape(rest.split("</string>").next()?))
}

/// The hotkey Alfred stored in the trigger `uid`, when it assigns a key.
pub fn read_hotkey(plist: &str, uid: &str) -> Option<Hotkey> {
    let (start, end) = trigger_config(plist, uid)?;
    let config = &plist[start..end];
    let keycode = int_after(config, "hotkey").unwrap_or(0);
    let mods = int_after(config, "hotmod").unwrap_or(0);
    let string = string_after(config, "hotstring").unwrap_or_default();
    (mods != 0 || (keycode != 0 && !string.is_empty())).then_some(Hotkey {
        keycode,
        mods,
        string,
    })
}

/// Insert `hotkey`, `hotmod` and `hotstring` into the config dict of the
/// trigger `uid`.
pub fn set_hotkey(plist: &str, uid: &str, hotkey: &Hotkey) -> Result<String> {
    let missing = || Error::Other(format!("hotkey trigger {uid} not found in info.plist"));
    let (start, _) = trigger_config(plist, uid).ok_or_else(missing)?;
    let close = start + plist[start..].find("</dict>").ok_or_else(missing)?;
    let line = plist[..close].rfind('\n').map_or(0, |n| n + 1);
    let indent = &plist[line..close];
    let key_indent = format!("{indent}\t");
    let mut out = String::with_capacity(plist.len() + 256);
    out.push_str(&plist[..line]);
    for (key, kind, value) in [
        ("hotkey", "integer", hotkey.keycode.to_string()),
        ("hotmod", "integer", hotkey.mods.to_string()),
        ("hotstring", "string", xml_escape(&hotkey.string)),
    ] {
        out.push_str(&format!(
            "{key_indent}<key>{key}</key>\n{key_indent}<{kind}>{value}</{kind}>\n"
        ));
    }
    out.push_str(&plist[line..]);
    Ok(out)
}

/// Decide the hotkey of one trigger: a flag wins, then the previous install.
fn resolve_hotkey(flag: &Option<Hotkey>, old: Option<&str>, uid: &str) -> HotkeyOutcome {
    match (flag, old.and_then(|p| read_hotkey(p, uid))) {
        (Some(h), _) => HotkeyOutcome::Set(h.clone()),
        (None, Some(h)) => HotkeyOutcome::Kept(h),
        (None, None) => HotkeyOutcome::Unset,
    }
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
    /// The hotkey of "Terminal tabs" after the install.
    pub tabs_hotkey: HotkeyOutcome,
    /// The hotkey of "AI sessions" after the install.
    pub agents_hotkey: HotkeyOutcome,
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

/// Write the workflow into `prefs` and migrate the older workflow. Hotkeys
/// assigned in an earlier install are kept; `hotkeys` overrides them.
pub fn install(prefs: &Path, hotkeys: &Hotkeys) -> Result<InstallReport> {
    let workflows = prefs.join("workflows");
    if !workflows.is_dir() {
        return Err(Error::Other(format!(
            "no workflows folder in {}",
            prefs.display()
        )));
    }
    let dir = workflows.join(WORKFLOW_DIR);
    let old_plist = std::fs::read_to_string(dir.join("info.plist")).ok();
    let tabs_hotkey = resolve_hotkey(&hotkeys.tabs, old_plist.as_deref(), TABS_UID);
    let agents_hotkey = resolve_hotkey(&hotkeys.agents, old_plist.as_deref(), AGENTS_UID);
    let mut info = String::from_utf8_lossy(INFO_PLIST).into_owned();
    for (uid, outcome) in [(TABS_UID, &tabs_hotkey), (AGENTS_UID, &agents_hotkey)] {
        if let Some(h) = outcome.hotkey() {
            info = set_hotkey(&info, uid, h)?;
        }
    }
    std::fs::create_dir_all(&dir)?;
    for (name, bytes) in FILES {
        let path = dir.join(name);
        let tmp = dir.join(format!(".{name}.tmp"));
        if *name == "info.plist" {
            std::fs::write(&tmp, &info)?;
        } else {
            std::fs::write(&tmp, bytes)?;
        }
        if *name == "csx.sh" {
            set_exec(&tmp)?;
        }
        std::fs::rename(&tmp, &path)?;
    }
    let mut report = InstallReport {
        dir: dir.clone(),
        migrated_keyword: None,
        removed_old: false,
        tabs_hotkey,
        agents_hotkey,
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

    #[test]
    fn resolve_prefs_prefers_flag_then_home_then_errors() {
        let c = fake(Err(()));
        let flag = Some(PathBuf::from("/p/Alfred.alfredpreferences"));
        assert_eq!(
            resolve_prefs(flag.clone(), &c, Some(Path::new("/h"))).unwrap(),
            flag.clone().unwrap()
        );
        assert_eq!(
            resolve_prefs(flag, &c, None).unwrap(),
            PathBuf::from("/p/Alfred.alfredpreferences")
        );
        assert!(c.1.borrow().is_empty());
        assert_eq!(
            resolve_prefs(None, &c, Some(Path::new("/h"))).unwrap(),
            PathBuf::from("/h/Library/Application Support/Alfred/Alfred.alfredpreferences")
        );
        let e = resolve_prefs(None, &c, None).unwrap_err();
        assert!(e.to_string().contains("HOME is not set"));
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
        let r = install(&p, &Hotkeys::default()).unwrap();
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
        install(&p, &Hotkeys::default()).unwrap();
    }

    #[test]
    fn install_keeps_existing_prefs_plist() {
        let p = prefs_with_workflows("keep");
        let dir = p.join("workflows").join(WORKFLOW_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("prefs.plist"), "mine").unwrap();
        install(&p, &Hotkeys::default()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("prefs.plist")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn install_without_workflows_errors() {
        let p = tmp("none");
        let e = install(&p, &Hotkeys::default()).unwrap_err().to_string();
        assert_eq!(e, format!("no workflows folder in {}", p.display()));
    }

    #[test]
    fn migration_moves_keyword_and_removes_old() {
        let p = prefs_with_workflows("mig");
        let old = old_workflow(&p, OLD_BUNDLE_ID, Some("x&amp;y"));
        let r = install(&p, &Hotkeys::default()).unwrap();
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
        let r = install(&p, &Hotkeys::default()).unwrap();
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
            let r = install(&p, &Hotkeys::default()).unwrap();
            assert_eq!(r.migrated_keyword, None);
            assert!(r.removed_old);
        }
    }

    #[test]
    fn migration_ignores_other_bundle_ids() {
        let p = prefs_with_workflows("other");
        let old = old_workflow(&p, "someone.else", Some("zz"));
        let r = install(&p, &Hotkeys::default()).unwrap();
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
        let r = install(&p, &Hotkeys::default()).unwrap();
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
            "./csx.sh tabs --alfred -- \"$1\"",
            "./csx.sh tabs --agents --alfred -- \"$1\"",
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
        let uid = |n: u32| format!("6C5D0A00-7A11-4B2E-9C3D-00000000000{n}");
        let expect = |src: u32, want: &[(u32, u32)]| {
            let key = format!("<key>{}</key>", uid(src));
            let at = s
                .match_indices(&key)
                .map(|(i, _)| i + key.len())
                .find(|&i| s[i..].trim_start().starts_with("<array>"))
                .unwrap_or_else(|| panic!("no connections for {src}"));
            let body = &s[at..];
            let body = &body[..body.find("</array>").unwrap()];
            let mut got = vec![];
            for chunk in body.split("<key>destinationuid</key>").skip(1) {
                let dest = chunk
                    .split("<string>")
                    .nth(1)
                    .unwrap()
                    .split("</string>")
                    .next()
                    .unwrap();
                let mods = chunk
                    .split("<integer>")
                    .nth(1)
                    .unwrap()
                    .split("</integer>")
                    .next()
                    .unwrap();
                got.push((dest.to_string(), mods.parse::<u32>().unwrap()));
            }
            let want: Vec<(String, u32)> = want.iter().map(|(d, m)| (uid(*d), *m)).collect();
            assert_eq!(got, want, "connections of {src}");
        };
        // A and B -> R (no modifier), C with Cmd and Option; H1 -> A, H2 -> B.
        expect(1, &[(3, 0), (4, 1048576), (4, 524288)]);
        expect(2, &[(3, 0), (4, 1048576), (4, 524288)]);
        expect(5, &[(1, 0)]);
        expect(6, &[(2, 0)]);
        assert_eq!(s.matches("<key>destinationuid</key>").count(), 8);
        assert!(!s.contains("<key>hotkey</key>"));
        assert!(s.contains("Copy the session id or folder"));
        assert!(s.contains("Copy the resume command"));
        let (_, sh) = FILES.iter().find(|(n, _)| *n == "csx.sh").unwrap();
        assert!(sh.starts_with(b"#!/bin/sh"));
        let (_, png) = FILES.iter().find(|(n, _)| *n == "icon.png").unwrap();
        assert!(png.starts_with(b"\x89PNG"));
    }

    // ---- hotkeys -----------------------------------------------------------

    fn hk(spec: &str) -> Hotkey {
        Hotkey::parse(spec).unwrap()
    }

    #[test]
    fn parse_modifier_aliases_sum_into_hotmod() {
        for (spec, mods) in [
            ("shift+a", 131072),
            ("ctrl+a", 262144),
            ("control+a", 262144),
            ("alt+a", 524288),
            ("opt+a", 524288),
            ("option+a", 524288),
            ("cmd+a", 1048576),
            ("command+a", 1048576),
            ("cmd+shift+a", 1048576 + 131072),
            ("ctrl+alt+cmd+shift+a", 262144 + 524288 + 1048576 + 131072),
            ("alt+alt+a", 524288),
        ] {
            let h = hk(spec);
            assert_eq!((h.keycode, h.mods), (0, mods), "{spec}");
            assert_eq!(h.string, "A");
        }
    }

    #[test]
    fn parse_keys_space_digits_and_case() {
        let h = hk("cmd+shift+space");
        assert_eq!(
            (h.keycode, h.mods, h.string.as_str()),
            (49, 1179648, "Space")
        );
        let h = hk("ctrl+alt+1");
        assert_eq!((h.keycode, h.mods, h.string.as_str()), (18, 786432, "1"));
        assert_eq!(hk(" Opt + T ").keycode, 17);
        assert_eq!(hk("OPT+T"), hk("opt+t"));
        assert_eq!(hk("alt+SPACE").string, "Space");
        let letters = "asdfhgzxcvbqwerytuiopljknm";
        let table: Vec<(char, u32)> = vec![
            ('a', 0),
            ('s', 1),
            ('d', 2),
            ('f', 3),
            ('h', 4),
            ('g', 5),
            ('z', 6),
            ('x', 7),
            ('c', 8),
            ('v', 9),
            ('b', 11),
            ('q', 12),
            ('w', 13),
            ('e', 14),
            ('r', 15),
            ('y', 16),
            ('t', 17),
            ('o', 31),
            ('u', 32),
            ('i', 34),
            ('p', 35),
            ('l', 37),
            ('j', 38),
            ('k', 40),
            ('n', 45),
            ('m', 46),
        ];
        assert_eq!(table.len(), letters.len());
        for (c, code) in table {
            let h = hk(&format!("alt+{c}"));
            assert_eq!(h.keycode, code, "{c}");
            assert_eq!(h.string, c.to_ascii_uppercase().to_string());
        }
        for (d, code) in [
            ('1', 18),
            ('2', 19),
            ('3', 20),
            ('4', 21),
            ('6', 22),
            ('5', 23),
            ('9', 25),
            ('7', 26),
            ('8', 28),
            ('0', 29),
        ] {
            assert_eq!(hk(&format!("alt+{d}")).keycode, code, "{d}");
        }
    }

    #[test]
    fn parse_errors_are_clear() {
        let err = |s: &str| Hotkey::parse(s).unwrap_err().to_string();
        assert!(err("a").contains("modifier"));
        assert!(err("space").contains("modifier"));
        assert!(err("alt+f13").contains("unknown key"));
        assert!(err("hyper+a").contains("unknown modifier"));
        assert!(err("").contains("empty"));
        assert!(err("   ").contains("empty"));
        assert!(err("alt+").contains("empty"));
        assert!(err("+a").contains("empty"));
        assert!(err("alt").contains("unknown key"));
    }

    #[test]
    fn hotkey_display_uses_mac_symbols() {
        assert_eq!(hk("alt+a").to_string(), "\u{2325}A");
        assert_eq!(
            hk("cmd+shift+ctrl+alt+space").to_string(),
            "\u{2303}\u{2325}\u{21e7}\u{2318}Space"
        );
    }

    const TEMPLATE: &str = include_str!("../../contrib/alfred/csx-tabs/info.plist");

    fn installed_plist(prefs: &Path) -> String {
        std::fs::read_to_string(
            prefs
                .join("workflows")
                .join(WORKFLOW_DIR)
                .join("info.plist"),
        )
        .unwrap()
    }
    /// An existing installed info.plist with the given hotkey values on H1 and H2.
    fn seed_installed(prefs: &Path, tabs: Option<&str>, agents: Option<&str>) {
        let mut s = TEMPLATE.to_string();
        if let Some(t) = tabs {
            s = set_hotkey(&s, TABS_UID, &hk(t)).unwrap();
        }
        if let Some(a) = agents {
            s = set_hotkey(&s, AGENTS_UID, &hk(a)).unwrap();
        }
        let dir = prefs.join("workflows").join(WORKFLOW_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("info.plist"), s).unwrap();
    }

    #[test]
    fn fresh_install_without_flags_has_no_hotkeys() {
        let p = prefs_with_workflows("hk-none");
        let r = install(&p, &Hotkeys::default()).unwrap();
        assert_eq!(r.tabs_hotkey, HotkeyOutcome::Unset);
        assert_eq!(r.agents_hotkey, HotkeyOutcome::Unset);
        let s = installed_plist(&p);
        assert!(!s.contains("<key>hotkey</key>"));
        assert_eq!(s, TEMPLATE);
    }

    #[test]
    fn fresh_install_with_flags_writes_hotkeys() {
        let p = prefs_with_workflows("hk-flags");
        let keys = Hotkeys {
            tabs: Some(hk("alt+t")),
            agents: Some(hk("cmd+shift+space")),
        };
        let r = install(&p, &keys).unwrap();
        assert_eq!(r.tabs_hotkey, HotkeyOutcome::Set(hk("alt+t")));
        assert_eq!(r.agents_hotkey, HotkeyOutcome::Set(hk("cmd+shift+space")));
        let s = installed_plist(&p);
        assert_eq!(read_hotkey(&s, TABS_UID), Some(hk("alt+t")));
        assert_eq!(read_hotkey(&s, AGENTS_UID), Some(hk("cmd+shift+space")));
        assert!(s.contains("<key>hotmod</key>\n\t\t\t\t<integer>524288</integer>"));
        assert!(s.contains("<key>hotstring</key>\n\t\t\t\t<string>Space</string>"));
    }

    #[test]
    fn reinstall_keeps_existing_hotkeys() {
        let p = prefs_with_workflows("hk-keep");
        seed_installed(&p, Some("alt+t"), Some("ctrl+1"));
        let r = install(&p, &Hotkeys::default()).unwrap();
        assert_eq!(r.tabs_hotkey, HotkeyOutcome::Kept(hk("alt+t")));
        assert_eq!(r.agents_hotkey, HotkeyOutcome::Kept(hk("ctrl+1")));
        let s = installed_plist(&p);
        assert_eq!(read_hotkey(&s, TABS_UID), Some(hk("alt+t")));
        assert_eq!(read_hotkey(&s, AGENTS_UID), Some(hk("ctrl+1")));
        // A second reinstall is stable.
        install(&p, &Hotkeys::default()).unwrap();
        assert_eq!(installed_plist(&p), s);
    }

    #[test]
    fn keeps_only_the_trigger_that_had_one() {
        let p = prefs_with_workflows("hk-one");
        seed_installed(&p, None, Some("opt+a"));
        let r = install(&p, &Hotkeys::default()).unwrap();
        assert_eq!(r.tabs_hotkey, HotkeyOutcome::Unset);
        assert_eq!(r.agents_hotkey, HotkeyOutcome::Kept(hk("opt+a")));
        assert_eq!(read_hotkey(&installed_plist(&p), TABS_UID), None);
    }

    #[test]
    fn flag_overrides_preserved_hotkey_for_that_trigger_only() {
        let p = prefs_with_workflows("hk-override");
        seed_installed(&p, Some("alt+t"), Some("alt+a"));
        let keys = Hotkeys {
            tabs: None,
            agents: Some(hk("cmd+j")),
        };
        let r = install(&p, &keys).unwrap();
        assert_eq!(r.tabs_hotkey, HotkeyOutcome::Kept(hk("alt+t")));
        assert_eq!(r.agents_hotkey, HotkeyOutcome::Set(hk("cmd+j")));
        let s = installed_plist(&p);
        assert_eq!(read_hotkey(&s, TABS_UID), Some(hk("alt+t")));
        assert_eq!(read_hotkey(&s, AGENTS_UID), Some(hk("cmd+j")));
        assert_eq!(s.matches("<key>hotkey</key>").count(), 2);
    }

    #[test]
    fn unassigned_existing_values_are_not_carried() {
        let p = prefs_with_workflows("hk-zero");
        let mut s = TEMPLATE.to_string();
        for uid in [TABS_UID, AGENTS_UID] {
            s = insert_hotkey_keys(&s, uid, 0, 0, "");
        }
        let dir = p.join("workflows").join(WORKFLOW_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("info.plist"), s).unwrap();
        let r = install(&p, &Hotkeys::default()).unwrap();
        assert_eq!(r.tabs_hotkey, HotkeyOutcome::Unset);
        assert_eq!(r.agents_hotkey, HotkeyOutcome::Unset);
        assert_eq!(installed_plist(&p), TEMPLATE);
    }

    /// Test helper: write raw hotkey, hotmod and hotstring values into a trigger.
    fn insert_hotkey_keys(plist: &str, uid: &str, key: u32, mods: u32, string: &str) -> String {
        let h = Hotkey {
            keycode: key,
            mods,
            string: string.to_string(),
        };
        set_hotkey(plist, uid, &h).unwrap()
    }

    #[test]
    fn read_hotkey_assignment_rules() {
        let with = |key, mods, string: &str| {
            read_hotkey(
                &insert_hotkey_keys(TEMPLATE, TABS_UID, key, mods, string),
                TABS_UID,
            )
        };
        assert_eq!(with(0, 0, ""), None);
        assert_eq!(with(0, 0, "A"), None);
        assert_eq!(with(17, 0, ""), None);
        assert!(with(17, 0, "T").is_some());
        assert!(with(0, 524288, "A").is_some());
        assert!(with(0, 524288, "").is_some());
        // A hotkey that Alfred never saved has no keys at all.
        assert_eq!(read_hotkey(TEMPLATE, TABS_UID), None);
        assert_eq!(read_hotkey(TEMPLATE, "no-such-uid"), None);
    }

    #[test]
    fn read_hotkey_ignores_other_triggers_and_connections() {
        let s = set_hotkey(TEMPLATE, AGENTS_UID, &hk("alt+a")).unwrap();
        assert_eq!(read_hotkey(&s, TABS_UID), None);
        assert_eq!(read_hotkey(&s, AGENTS_UID), Some(hk("alt+a")));
    }

    #[test]
    fn set_hotkey_errors_for_unknown_uid_and_keeps_the_rest() {
        assert!(set_hotkey(TEMPLATE, "no-such-uid", &hk("alt+a")).is_err());
        let s = set_hotkey(TEMPLATE, TABS_UID, &hk("alt+t")).unwrap();
        // Removing the three inserted keys gives back the template.
        let block = "\t\t\t\t<key>hotkey</key>\n\t\t\t\t<integer>17</integer>\n\
\t\t\t\t<key>hotmod</key>\n\t\t\t\t<integer>524288</integer>\n\
\t\t\t\t<key>hotstring</key>\n\t\t\t\t<string>T</string>\n";
        assert_eq!(s.matches(block).count(), 1);
        assert_eq!(s.replacen(block, "", 1), TEMPLATE);
    }

    #[test]
    fn hotkey_strings_are_xml_escaped() {
        let h = Hotkey {
            keycode: 0,
            mods: 524288,
            string: "<&>".into(),
        };
        let s = set_hotkey(TEMPLATE, TABS_UID, &h).unwrap();
        assert!(s.contains("<string>&lt;&amp;&gt;</string>"));
        assert_eq!(read_hotkey(&s, TABS_UID).unwrap().string, "<&>");
    }

    #[test]
    fn written_info_plist_keeps_every_object_and_connection() {
        let p = prefs_with_workflows("hk-intact");
        install(
            &p,
            &Hotkeys {
                tabs: Some(hk("alt+t")),
                agents: Some(hk("alt+a")),
            },
        )
        .unwrap();
        let s = installed_plist(&p);
        assert_eq!(
            s.matches("<key>destinationuid</key>").count(),
            TEMPLATE.matches("<key>destinationuid</key>").count()
        );
        assert_eq!(
            s.matches("<key>uid</key>").count(),
            TEMPLATE.matches("<key>uid</key>").count()
        );
        for n in 1..=6 {
            let uid = format!("6C5D0A00-7A11-4B2E-9C3D-00000000000{n}");
            assert!(s.contains(&format!("<string>{uid}</string>")), "uid {n}");
        }
        assert!(s.contains("alfred.workflow.trigger.hotkey"));
        assert!(s.contains("<string>to.mysq.csx-tabs</string>"));
        assert!(s.contains("./csx.sh tabs focus"));
        assert!(s.trim_end().ends_with("</plist>"));
    }

    #[test]
    fn install_with_unreadable_old_plist_ignores_it() {
        let p = prefs_with_workflows("hk-garbage");
        let dir = p.join("workflows").join(WORKFLOW_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("info.plist"), "not a plist").unwrap();
        let r = install(&p, &Hotkeys::default()).unwrap();
        assert_eq!(r.tabs_hotkey, HotkeyOutcome::Unset);
        assert_eq!(installed_plist(&p), TEMPLATE);
    }
}
