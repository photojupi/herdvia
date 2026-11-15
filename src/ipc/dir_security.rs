//! Decisions about the Windows security of the folders above the data directory and the
//! integrity labels in it, kept free of Win32 calls so that their tests run on every platform.
//! src/ipc/windows.rs reads the security descriptors and acts on the answers.

pub(super) const ADMINISTRATORS: &str = "S-1-5-32-544";
pub(super) const LOCAL_SYSTEM: &str = "S-1-5-18";
/// NT SERVICE\TrustedInstaller, which owns C:\ and the Windows folders.
pub(super) const TRUSTED_INSTALLER: &str =
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

// Access control entry types and flags, from winnt.h.
pub(super) const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
pub(super) const ACCESS_DENIED_ACE_TYPE: u8 = 1;
pub(super) const ACCESS_DENIED_OBJECT_ACE_TYPE: u8 = 6;
pub(super) const ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 10;
pub(super) const ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE: u8 = 12;
const OBJECT_INHERIT_ACE: u8 = 0x01;
const CONTAINER_INHERIT_ACE: u8 = 0x02;
pub(super) const INHERIT_ONLY_ACE: u8 = 0x08;

// Access rights, from winnt.h.
const DELETE: u32 = 0x0001_0000;
const FILE_DELETE_CHILD: u32 = 0x0040;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
/// FILE_ADD_FILE on a folder.
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
const GENERIC_WRITE: u32 = 0x4000_0000;

// Mandatory label policies and the Medium integrity level, from winnt.h.
const NO_WRITE_UP: u32 = 0x1;
const NO_READ_UP: u32 = 0x2;
const MEDIUM: u32 = 0x2000;

/// The rights that let an account swap a folder's entries for others: DELETE (rename the
/// folder), FILE_DELETE_CHILD (rename what is inside), FILE_WRITE_DATA and
/// FILE_WRITE_ATTRIBUTES (either is enough to set a reparse point, a junction, on the folder
/// while it is empty), WRITE_DAC and WRITE_OWNER (give itself those), and GENERIC_ALL and
/// GENERIC_WRITE, which hold some of them. Reading, traversing, and only adding folders
/// (FILE_ADD_SUBDIRECTORY, which C:\ gives Authenticated Users) are not among them.
pub(super) const PATH_CHANGING: u32 = DELETE
    | FILE_DELETE_CHILD
    | FILE_WRITE_DATA
    | FILE_WRITE_ATTRIBUTES
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_ALL
    | GENERIC_WRITE;

/// Whether an account may change a folder above the data directory: this user, SYSTEM,
/// Administrators, and TrustedInstaller.
pub(super) fn trusted_ancestor(account: &str, sid: &str) -> bool {
    [sid, LOCAL_SYSTEM, ADMINISTRATORS, TRUSTED_INSTALLER].contains(&account)
}

/// An entry of a DACL, as `ancestor_refusal` looks at it.
pub(super) struct Ace {
    pub(super) kind: u8,
    pub(super) flags: u8,
    /// The access mask and SID of an ACCESS_ALLOWED_ACE; not read for other types.
    pub(super) allowed: Option<(u32, String)>,
}

/// Why a folder above the data directory is refused.
#[derive(Debug, PartialEq)]
pub(super) enum Refusal {
    /// Owned by an account that `trusted_ancestor` does not accept.
    Owner(String),
    /// A NULL DACL, which allows everyone everything.
    NullDacl,
    /// An entry that applies to the folder gives this account a right in PATH_CHANGING.
    Grants(String),
    /// An allow entry of a type that is not examined (object, callback, compound).
    Unexamined(u8),
}

/// Whether a folder above the data directory, with this owner and DACL (None when NULL), lets
/// an account other than `trusted_ancestor`'s swap what is inside it. Entries that only pass
/// on to new entries (INHERIT_ONLY) and deny entries do not count. This is not the rule for
/// the data directory and its files (`read_dacl`), which lets no other account in at all.
pub(super) fn ancestor_refusal(owner: &str, dacl: Option<&[Ace]>, sid: &str) -> Option<Refusal> {
    if !trusted_ancestor(owner, sid) {
        return Some(Refusal::Owner(owner.to_owned()));
    }
    let Some(dacl) = dacl else {
        return Some(Refusal::NullDacl);
    };
    for ace in dacl {
        if ace.flags & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        match (ace.kind, &ace.allowed) {
            (
                ACCESS_DENIED_ACE_TYPE
                | ACCESS_DENIED_OBJECT_ACE_TYPE
                | ACCESS_DENIED_CALLBACK_ACE_TYPE
                | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE,
                _,
            ) => {}
            (ACCESS_ALLOWED_ACE_TYPE, Some((mask, trustee))) => {
                if mask & PATH_CHANGING != 0 && !trusted_ancestor(trustee, sid) {
                    return Some(Refusal::Grants(trustee.clone()));
                }
            }
            (kind, _) => return Some(Refusal::Unexamined(kind)),
        }
    }
    None
}

/// A mandatory label entry of a SACL.
pub(super) struct Label {
    pub(super) flags: u8,
    /// NO_WRITE_UP, NO_READ_UP, and NO_EXECUTE_UP.
    pub(super) policy: u32,
    /// The label's SID, S-1-16-<integrity level>.
    pub(super) sid: String,
}

/// The integrity level of a label SID, S-1-16-<level>.
fn integrity_level(sid: &str) -> Option<u32> {
    sid.strip_prefix("S-1-16-")?.parse().ok()
}

/// Whether `prepare_dir` gives a directory or file with these label entries (in SACL order)
/// the label Medium with NO_WRITE_UP and NO_READ_UP, passed on to the files and folders
/// created in a directory. An entry without a label counts as Medium with NO_WRITE_UP only,
/// which lets a Low integrity process read it. A label below Medium is raised; one above
/// Medium is never lowered; one that is not an integrity level is left alone.
pub(super) fn needs_label(labels: &[Label], directory: bool) -> bool {
    // The first entry that applies to the entry itself is its label.
    let Some(label) = labels
        .iter()
        .find(|label| label.flags & INHERIT_ONLY_ACE == 0)
    else {
        return true;
    };
    let policy = NO_WRITE_UP | NO_READ_UP;
    let inherit = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
    match integrity_level(&label.sid) {
        Some(level) if level < MEDIUM => true,
        Some(MEDIUM) => {
            label.policy & policy != policy || (directory && label.flags & inherit != inherit)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "S-1-5-21-1-2-3-1001";
    const USERS: &str = "S-1-5-32-545";
    const AUTHENTICATED_USERS: &str = "S-1-5-11";
    const EVERYONE: &str = "S-1-1-0";
    const CREATOR_OWNER: &str = "S-1-3-0";
    const OBJECT_INHERIT: u8 = 0x01;
    const CONTAINER_INHERIT: u8 = 0x02;
    const INHERITED: u8 = 0x10;
    // Masks as icacls writes them.
    const FULL: u32 = 0x001f_01ff;
    const MODIFY: u32 = 0x0013_01bf;
    const READ_EXECUTE: u32 = 0x0012_00a9;
    const ADD_SUBDIRECTORY: u32 = 0x0004;
    const ADD_FILE: u32 = 0x0002;
    const WRITE_ATTRIBUTES: u32 = 0x0100;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const GENERIC_READ_EXECUTE: u32 = 0xa000_0000;

    fn allow(flags: u8, mask: u32, trustee: &str) -> Ace {
        Ace {
            kind: ACCESS_ALLOWED_ACE_TYPE,
            flags,
            allowed: Some((mask, trustee.to_owned())),
        }
    }

    fn deny(flags: u8) -> Ace {
        Ace {
            kind: ACCESS_DENIED_ACE_TYPE,
            flags,
            allowed: None,
        }
    }

    const OICI: u8 = OBJECT_INHERIT | CONTAINER_INHERIT;
    const OICI_IO: u8 = OICI | INHERIT_ONLY_ACE;

    #[test]
    fn accepts_the_usual_drive_root_and_profile() {
        // C:\ on Windows 11, owned by TrustedInstaller.
        let root = [
            allow(OICI, FULL, ADMINISTRATORS),
            allow(OICI, FULL, LOCAL_SYSTEM),
            allow(OICI, READ_EXECUTE, USERS),
            allow(OICI_IO, MODIFY, AUTHENTICATED_USERS),
            allow(0, ADD_SUBDIRECTORY, AUTHENTICATED_USERS),
            allow(OICI_IO, 0x1000_0000, CREATOR_OWNER),
        ];
        assert_eq!(ancestor_refusal(TRUSTED_INSTALLER, Some(&root), SID), None);
        // C:\Users and a profile folder below it.
        let users = [
            allow(OICI | INHERITED, FULL, LOCAL_SYSTEM),
            allow(OICI | INHERITED, FULL, ADMINISTRATORS),
            allow(OICI, READ_EXECUTE, USERS),
            allow(OICI_IO, GENERIC_READ_EXECUTE, EVERYONE),
            allow(0, READ_EXECUTE, EVERYONE),
        ];
        assert_eq!(ancestor_refusal(LOCAL_SYSTEM, Some(&users), SID), None);
        let profile = [
            allow(OICI, FULL, LOCAL_SYSTEM),
            allow(OICI, FULL, ADMINISTRATORS),
            allow(OICI, FULL, SID),
        ];
        for owner in [SID, LOCAL_SYSTEM, ADMINISTRATORS] {
            assert_eq!(ancestor_refusal(owner, Some(&profile), SID), None);
        }
    }

    #[test]
    fn accepts_reading_adding_folders_and_deny_entries() {
        for mask in [
            READ_EXECUTE,
            GENERIC_READ_EXECUTE,
            ADD_SUBDIRECTORY,
            ADD_SUBDIRECTORY | READ_EXECUTE,
        ] {
            assert_eq!(
                ancestor_refusal(SID, Some(&[allow(0, mask, USERS)]), SID),
                None,
                "{mask:#x}"
            );
        }
        // Deny entries are not examined, whatever they hold.
        assert_eq!(
            ancestor_refusal(SID, Some(&[deny(0), deny(OICI)]), SID),
            None
        );
    }

    #[test]
    fn refuses_a_right_that_changes_the_path() {
        for mask in [
            DELETE,
            FILE_DELETE_CHILD,
            WRITE_DAC,
            WRITE_OWNER,
            GENERIC_ALL,
            MODIFY,
            FULL,
            // Enough to set a reparse point on the folder while it is empty.
            ADD_FILE,
            WRITE_ATTRIBUTES,
            GENERIC_WRITE,
            ADD_FILE | READ_EXECUTE,
        ] {
            for flags in [0, OICI, OICI | INHERITED] {
                assert_eq!(
                    ancestor_refusal(SID, Some(&[allow(flags, mask, USERS)]), SID),
                    Some(Refusal::Grants(USERS.into())),
                    "{mask:#x} {flags:#x}"
                );
            }
            // To the trusted accounts only.
            for account in [SID, LOCAL_SYSTEM, ADMINISTRATORS, TRUSTED_INSTALLER] {
                assert_eq!(
                    ancestor_refusal(SID, Some(&[allow(0, mask, account)]), SID),
                    None
                );
            }
        }
        // The first account that is let in is named.
        let dacl = [
            allow(0, READ_EXECUTE, EVERYONE),
            allow(0, MODIFY, AUTHENTICATED_USERS),
            allow(0, MODIFY, USERS),
        ];
        assert_eq!(
            ancestor_refusal(SID, Some(&dacl), SID),
            Some(Refusal::Grants(AUTHENTICATED_USERS.into()))
        );
        // A deny entry before it does not take it back.
        let dacl = [deny(0), allow(0, DELETE, USERS)];
        assert_eq!(
            ancestor_refusal(SID, Some(&dacl), SID),
            Some(Refusal::Grants(USERS.into()))
        );
    }

    #[test]
    fn refuses_another_owner_a_null_dacl_and_unexamined_entries() {
        let profile = [allow(0, FULL, SID)];
        for owner in ["S-1-5-21-1-2-3-1002", USERS, EVERYONE, CREATOR_OWNER] {
            assert_eq!(
                ancestor_refusal(owner, Some(&profile), SID),
                Some(Refusal::Owner(owner.into()))
            );
        }
        assert_eq!(ancestor_refusal(SID, None, SID), Some(Refusal::NullDacl));
        // Compound (4), object (5), callback (9), and callback object (11) allow entries.
        for kind in [4, 5, 9, 11] {
            let ace = Ace {
                kind,
                flags: 0,
                allowed: None,
            };
            assert_eq!(
                ancestor_refusal(SID, Some(&[ace]), SID),
                Some(Refusal::Unexamined(kind))
            );
            // Unless they pass on only.
            let ace = Ace {
                kind,
                flags: OICI_IO,
                allowed: None,
            };
            assert_eq!(ancestor_refusal(SID, Some(&[ace]), SID), None);
        }
    }

    fn label(flags: u8, policy: u32, sid: &str) -> Label {
        Label {
            flags,
            policy,
            sid: sid.to_owned(),
        }
    }

    const LOW: &str = "S-1-16-4096";
    const ME: &str = "S-1-16-8192";
    const HIGH: &str = "S-1-16-12288";
    const SYSTEM_LEVEL: &str = "S-1-16-16384";
    const NWNR: u32 = NO_WRITE_UP | NO_READ_UP;
    const NO_EXECUTE_UP: u32 = 0x4;

    #[test]
    fn labels_an_entry_without_a_label_or_without_no_read_up() {
        for directory in [false, true] {
            assert!(needs_label(&[], directory));
            // Medium, as the system treats an entry without a label, or without NO_READ_UP.
            for policy in [0, NO_WRITE_UP, NO_READ_UP, NO_WRITE_UP | NO_EXECUTE_UP] {
                assert!(
                    needs_label(&[label(OICI, policy, ME)], directory),
                    "{policy}"
                );
            }
            // Low is raised.
            assert!(needs_label(&[label(OICI, NWNR, LOW)], directory));
            // An entry that only passes on is not the entry's own label.
            assert!(needs_label(&[label(OICI_IO, NWNR, ME)], directory));
        }
        // A directory's label must also pass on to files and folders.
        for flags in [0, OBJECT_INHERIT, CONTAINER_INHERIT] {
            assert!(needs_label(&[label(flags, NWNR, ME)], true), "{flags}");
            assert!(!needs_label(&[label(flags, NWNR, ME)], false), "{flags}");
        }
    }

    #[test]
    fn leaves_a_label_that_is_enough_or_higher_alone() {
        for directory in [false, true] {
            for policy in [NWNR, NWNR | NO_EXECUTE_UP] {
                assert!(!needs_label(&[label(OICI, policy, ME)], directory));
                // Inherited from the directory.
                assert!(!needs_label(&[label(INHERITED, policy, ME)], false));
            }
            // Never lowered to Medium, whatever its policy.
            for level in [HIGH, SYSTEM_LEVEL] {
                for policy in [0, NO_WRITE_UP, NWNR] {
                    assert!(!needs_label(&[label(0, policy, level)], directory));
                }
            }
            // Not an integrity level.
            assert!(!needs_label(&[label(0, 0, "S-1-1-0")], directory));
            // The first entry that applies is the label.
            assert!(!needs_label(
                &[
                    label(OICI_IO, 0, LOW),
                    label(OICI, NWNR, ME),
                    label(0, 0, LOW)
                ],
                directory
            ));
            assert!(needs_label(
                &[label(0, 0, LOW), label(OICI, NWNR, ME)],
                directory
            ));
        }
        assert_eq!(integrity_level(ME), Some(MEDIUM));
        assert_eq!(integrity_level("S-1-16-x"), None);
    }
}
