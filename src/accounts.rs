//! Minimal `/etc/passwd` and `/etc/group` parsing for dropping service
//! privileges. There is deliberately no NSS support: jiminit is PID 1 and
//! must not depend on daemons that are not running yet.

use anyhow::{Context, Result};
use std::path::Path;

pub const PASSWD: &str = "/etc/passwd";
pub const GROUP: &str = "/etc/group";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub shell: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub gid: u32,
    pub members: Vec<String>,
}

/// Credentials resolved for one service, in the order the kernel expects them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups (the primary gid is passed to `setgid`).
    pub groups: Vec<u32>,
    pub home: Option<String>,
}

pub fn load_passwd(path: &Path) -> Result<Vec<User>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(parse_passwd(&text))
}

pub fn load_group(path: &Path) -> Result<Vec<Group>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(parse_group(&text))
}

pub fn parse_passwd(text: &str) -> Vec<User> {
    let mut users = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 7 {
            continue;
        }
        let (Ok(uid), Ok(gid)) = (fields[2].parse(), fields[3].parse()) else {
            continue;
        };
        users.push(User {
            name: fields[0].to_string(),
            uid,
            gid,
            home: fields[5].to_string(),
            shell: fields[6].to_string(),
        });
    }
    users
}

pub fn parse_group(text: &str) -> Vec<Group> {
    let mut groups = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 3 {
            continue;
        }
        let Ok(gid) = fields[2].parse() else {
            continue;
        };
        let members = fields
            .get(3)
            .map(|list| {
                list.split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        groups.push(Group {
            name: fields[0].to_string(),
            gid,
            members,
        });
    }
    groups
}

/// Find a user by name, or by uid when the spec is numeric.
pub fn find_user(users: &[User], spec: &str) -> Result<User> {
    if let Ok(uid) = spec.parse::<u32>() {
        return users
            .iter()
            .find(|user| user.uid == uid)
            .cloned()
            .with_context(|| format!("no user with uid {uid} in /etc/passwd"));
    }
    users
        .iter()
        .find(|user| user.name == spec)
        .cloned()
        .with_context(|| format!("no user named `{spec}` in /etc/passwd"))
}

/// Find a group by name, or by gid when the spec is numeric.
pub fn find_group(groups: &[Group], spec: &str) -> Result<Group> {
    if let Ok(gid) = spec.parse::<u32>() {
        return groups
            .iter()
            .find(|group| group.gid == gid)
            .cloned()
            .with_context(|| format!("no group with gid {gid} in /etc/group"));
    }
    groups
        .iter()
        .find(|group| group.name == spec)
        .cloned()
        .with_context(|| format!("no group named `{spec}` in /etc/group"))
}

/// Resolve the credential set for a service. `user` is required (it is the
/// reason this is called at all); `group` overrides the user's primary group,
/// and `supplementary` overrides the groups read from `/etc/group`.
pub fn resolve(
    user: &str,
    group: Option<&str>,
    supplementary: &[String],
    passwd: &Path,
    groups_file: &Path,
) -> Result<Credentials> {
    let users = load_passwd(passwd)?;
    let all_groups = load_group(groups_file)?;
    let account = find_user(&users, user)?;

    let gid = match group {
        Some(spec) => find_group(&all_groups, spec)?.gid,
        None => account.gid,
    };

    let mut gids: Vec<u32> = Vec::new();
    if supplementary.is_empty() {
        for g in &all_groups {
            if g.members.iter().any(|name| name == &account.name) && g.gid != gid {
                gids.push(g.gid);
            }
        }
    } else {
        for spec in supplementary {
            let gid = find_group(&all_groups, spec)?.gid;
            if !gids.contains(&gid) {
                gids.push(gid);
            }
        }
    }
    gids.retain(|g| *g != gid);
    gids.sort_unstable();
    gids.dedup();

    Ok(Credentials {
        uid: account.uid,
        gid,
        groups: gids,
        home: Some(account.home),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/sh
daemon:x:2:2:daemon:/:/sbin/nologin
# comment
broken line
app:x:1000:1000:App:/srv/app:/bin/sh
";

    const GROUP: &str = "\
root:x:0:
users:x:100:app
wheel:x:10:app
empty:x:20:
";

    #[test]
    fn parses_passwd_and_skips_bad_lines() {
        let users = parse_passwd(PASSWD);
        assert_eq!(users.len(), 3);
        assert_eq!(users[1].name, "daemon");
        assert_eq!(users[1].uid, 2);
        assert_eq!(users[2].home, "/srv/app");
        assert_eq!(users[2].shell, "/bin/sh");
    }

    #[test]
    fn parses_group_members() {
        let groups = parse_group(GROUP);
        assert_eq!(groups.len(), 4);
        assert_eq!(groups[1].members, vec!["app"]);
        assert!(groups[3].members.is_empty());
    }

    #[test]
    fn resolves_by_name_or_number() {
        let users = parse_passwd(PASSWD);
        assert_eq!(find_user(&users, "daemon").unwrap().uid, 2);
        assert_eq!(find_user(&users, "1000").unwrap().name, "app");
        assert!(find_user(&users, "missing").is_err());

        let groups = parse_group(GROUP);
        assert_eq!(find_group(&groups, "users").unwrap().gid, 100);
        assert_eq!(find_group(&groups, "10").unwrap().name, "wheel");
        assert!(find_group(&groups, "missing").is_err());
    }

    #[test]
    fn resolves_credentials_from_group_membership() {
        let dir = std::env::temp_dir().join(format!("jiminit-accounts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let passwd = dir.join("passwd");
        let group = dir.join("group");
        std::fs::write(&passwd, PASSWD).unwrap();
        std::fs::write(&group, GROUP).unwrap();

        // Without an explicit list, groups are taken from /etc/group.
        let creds = resolve("app", None, &[], &passwd, &group).unwrap();
        assert_eq!(creds.uid, 1000);
        assert_eq!(creds.gid, 1000);
        assert_eq!(creds.groups, vec![10, 100]);

        let creds = resolve("app", None, &["wheel".into()], &passwd, &group).unwrap();
        assert_eq!(creds.groups, vec![10]);

        let creds = resolve("app", Some("wheel"), &[], &passwd, &group).unwrap();
        assert_eq!(creds.gid, 10);
    }

    #[test]
    fn rejects_unknown_users_and_groups() {
        let dir = std::env::temp_dir().join(format!("jiminit-accounts-err-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let passwd = dir.join("passwd");
        let group = dir.join("group");
        std::fs::write(&passwd, PASSWD).unwrap();
        std::fs::write(&group, GROUP).unwrap();
        assert!(resolve("nobody", None, &[], &passwd, &group).is_err());
        assert!(resolve("app", Some("missing"), &[], &passwd, &group).is_err());
    }
}
