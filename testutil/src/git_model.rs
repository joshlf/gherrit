//! Literal Git ref storage. No application history invariant lives here.
//!
//! Advertisement decides which commands the client sends. Atomic reception
//! checks exactly those commands. Keeping the stages separate lets schedules
//! retain several in-flight advertisements across other completed pushes.

use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RefStore(pub BTreeMap<String, String>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Update {
    pub name: String,
    pub expected: Option<String>,
    pub desired: String,
}

impl RefStore {
    /// Git elides already-desired refs before enforcing explicit leases.
    pub fn advertise(&self, requested: &[Update]) -> Result<Vec<Update>, &'static str> {
        let mut commands = Vec::new();
        for update in requested {
            let current = self.0.get(&update.name);
            if current == Some(&update.desired) {
                continue;
            }
            if current != update.expected.as_ref() {
                return Err("stale lease");
            }
            commands.push(update.clone());
        }
        Ok(commands)
    }

    /// The server atomically checks old OIDs of transmitted commands only.
    pub fn receive(&mut self, commands: &[Update]) -> Result<(), &'static str> {
        if commands.iter().any(|u| self.0.get(&u.name) != u.expected.as_ref()) {
            return Err("stale server ref");
        }
        let conflict = |left: &str, right: &str| {
            left.strip_prefix(right).is_some_and(|tail| tail.starts_with('/'))
                || right.strip_prefix(left).is_some_and(|tail| tail.starts_with('/'))
        };
        for (index, command) in commands.iter().enumerate() {
            if commands[index + 1..].iter().any(|other| other.name == command.name) {
                return Err("duplicate ref destination");
            }
            if self.0.keys().any(|name| conflict(name, &command.name))
                || commands[index + 1..].iter().any(|other| conflict(&other.name, &command.name))
            {
                return Err("ref directory/file conflict");
            }
        }
        for command in commands {
            self.0.insert(command.name.clone(), command.desired.clone());
        }
        Ok(())
    }

    pub fn push(&mut self, requested: &[Update]) -> Result<(), &'static str> {
        let commands = self.advertise(requested)?;
        self.receive(&commands)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tuple(old: Option<&str>, desired: &str, version: usize, object: &str) -> Vec<Update> {
        vec![
            Update {
                name: "head".into(),
                expected: old.map(str::to_owned),
                desired: desired.into(),
            },
            Update { name: format!("v{version}"), expected: None, desired: object.into() },
        ]
    }

    #[test]
    fn content_equality_cannot_fence_an_aba_publication() {
        let mut git = RefStore::default();
        git.push(&tuple(None, "A", 1, "A")).unwrap();
        let delayed = tuple(Some("A"), "B", 2, "B");
        git.push(&tuple(Some("A"), "B", 2, "B")).unwrap();
        git.push(&tuple(Some("B"), "A", 3, "A")).unwrap();
        git.push(&delayed).unwrap();
        assert_eq!(git.0["head"], "B");
        assert_eq!(git.0["v3"], "A");
    }

    #[test]
    fn distinct_publication_objects_fence_repeated_revision_content() {
        let mut git = RefStore::default();
        git.push(&tuple(None, "A", 1, "token-1")).unwrap();
        let delayed = tuple(Some("A"), "B", 2, "token-X");
        git.push(&tuple(Some("A"), "B", 2, "token-Y")).unwrap();
        git.push(&tuple(Some("B"), "A", 3, "token-Z")).unwrap();
        let before = git.clone();
        assert!(git.push(&delayed).is_err());
        assert_eq!(git, before);
    }

    #[test]
    fn retained_advertisements_check_only_the_commands_they_transmit() {
        let mut git = RefStore::default();
        git.push(&tuple(None, "A", 1, "one")).unwrap();
        let first = git.advertise(&tuple(Some("A"), "B", 2, "two-X")).unwrap();
        let second = git.advertise(&tuple(Some("A"), "B", 2, "two-Y")).unwrap();
        git.receive(&second).unwrap();
        git.push(&tuple(Some("B"), "A", 3, "three")).unwrap();
        assert!(git.receive(&first).is_err());
        assert_eq!(git.0["head"], "A");
    }
    #[test]
    fn native_git_agrees_on_lightweight_aba_and_unique_annotation_rejection() {
        for guarded in [false, true] {
            let ctx = crate::TestContextBuilder::new("unused")
                .with_remote()
                .with_initial_commit()
                .build();
            let parent = ctx.head_oid();
            ctx.commit("A");
            let a = ctx.head_oid();
            ctx.run_git(&["reset", "--hard", &parent]);
            ctx.commit("B");
            let b = ctx.head_oid();
            let annotation = |name: &str, head: &str| {
                ctx.run_git(&["tag", "--annotate", name, "--message", name, head]);
                String::from_utf8(ctx.git_cmd().args(["rev-parse", name]).output().unwrap().stdout)
                    .unwrap()
                    .trim()
                    .to_owned()
            };
            let first = if guarded { annotation("token-first", &a) } else { a.clone() };
            let delayed = if guarded { annotation("token-delayed", &b) } else { b.clone() };
            let second = if guarded { annotation("token-winner", &b) } else { b.clone() };
            let third = if guarded { annotation("token-third", &a) } else { a.clone() };
            let tuple = |old: Option<&str>, head: &str, version: usize, object: &str| {
                vec![
                    Update {
                        name: "refs/heads/Gexample".into(),
                        expected: old.map(str::to_owned),
                        desired: head.into(),
                    },
                    Update {
                        name: "refs/heads/gherrit-bases/Gexample".into(),
                        expected: old.map(|_| parent.clone()),
                        desired: parent.clone(),
                    },
                    Update {
                        name: format!("refs/tags/gherrit/Gexample/v{version}"),
                        expected: None,
                        desired: object.into(),
                    },
                ]
            };
            let mut model = RefStore::default();
            let delayed = tuple(Some(&a), &b, 2, &delayed);
            for request in [
                tuple(None, &a, 1, &first),
                tuple(Some(&a), &b, 2, &second),
                tuple(Some(&b), &a, 3, &third),
                delayed,
            ] {
                let modeled = model.push(&request).is_ok();
                let mut args = vec!["push".to_owned(), "--atomic".into(), "--porcelain".into()];
                args.extend(request.iter().map(|u| {
                    format!("--force-with-lease={}:{}", u.name, u.expected.as_deref().unwrap_or(""))
                }));
                args.push(ctx.remote_path.to_str().unwrap().to_owned());
                args.extend(request.iter().map(|u| format!("{}:{}", u.desired, u.name)));
                let actual = ctx.git_cmd().args(&args).output().unwrap();
                assert_eq!(
                    actual.status.success(),
                    modeled,
                    "guarded={guarded}: {}",
                    String::from_utf8_lossy(&actual.stderr)
                );
                for (name, oid) in &model.0 {
                    assert_eq!(ctx.remote_ref_oid(name).as_ref(), Some(oid));
                }
            }
            assert_eq!(model.0["refs/heads/Gexample"], if guarded { a } else { b });
        }
    }

    #[test]
    fn native_atomic_receive_agrees_on_raw_ref_directory_file_conflicts() {
        for (raw, target, accepted) in [
            ("refs/heads/Groot/child", "refs/heads/Groot", false),
            ("refs/heads/gherrit-bases", "refs/heads/gherrit-bases/Groot", false),
            ("refs/heads/Groot-other/child", "refs/heads/Groot", true),
        ] {
            let ctx = crate::TestContextBuilder::new("unused").with_initial_commit().build();
            let head = ctx.head_oid();
            ctx.run_git(&["update-ref", raw, &head]);
            let mut model = RefStore(BTreeMap::from([(raw.to_owned(), head.clone())]));
            let before = model.clone();
            let updates = [
                Update {
                    name: "refs/heads/Gother".to_owned(),
                    expected: None,
                    desired: head.clone(),
                },
                Update { name: target.to_owned(), expected: None, desired: head.clone() },
            ];
            assert_eq!(model.push(&updates).is_ok(), accepted);
            let native = ctx
                .git_cmd()
                .args([
                    "push",
                    "--atomic",
                    ".",
                    &format!("{head}:refs/heads/Gother"),
                    &format!("{head}:{target}"),
                ])
                .output()
                .unwrap();
            assert_eq!(
                native.status.success(),
                accepted,
                "{}",
                String::from_utf8_lossy(&native.stderr)
            );
            if !accepted {
                assert_eq!(model, before);
            }
            for name in ["refs/heads/Gother", target] {
                let native = ctx.git_cmd().args(["rev-parse", "--verify", name]).output().unwrap();
                assert_eq!(native.status.success(), accepted);
            }
            assert_eq!(ctx.head_oid(), head);
        }
        let mut empty = RefStore::default();
        let before = empty.clone();
        assert!(empty
            .receive(&[
                Update { name: "refs/heads/Groot".into(), expected: None, desired: "A".into() },
                Update {
                    name: "refs/heads/Groot/child".into(),
                    expected: None,
                    desired: "A".into()
                }
            ])
            .is_err());
        assert_eq!(empty, before);
    }

    #[test]
    fn native_ref_transactions_reject_duplicate_destinations_atomically() {
        let ctx = crate::TestContextBuilder::new("unused").with_initial_commit().build();
        let a = ctx.head_oid();
        ctx.commit("B");
        let b = ctx.head_oid();
        ctx.commit("C");
        let c = ctx.head_oid();
        let target = "refs/heads/Gduplicate";
        let sibling = "refs/heads/Gsibling";
        for old in [None, Some(a.as_str())] {
            if let Some(old) = old {
                ctx.run_git(&["update-ref", target, old]);
            }
            for second in [&b, &c] {
                let mut model = RefStore(
                    old.map(|old| (target.to_owned(), old.to_owned())).into_iter().collect(),
                );
                let before = model.clone();
                let updates = [
                    Update { name: sibling.into(), expected: None, desired: b.clone() },
                    Update {
                        name: target.into(),
                        expected: old.map(str::to_owned),
                        desired: b.clone(),
                    },
                    Update {
                        name: target.into(),
                        expected: old.map(str::to_owned),
                        desired: second.clone(),
                    },
                ];
                assert_eq!(model.receive(&updates), Err("duplicate ref destination"));
                assert_eq!(model, before);
                let old_oid = old.map(str::to_owned).unwrap_or_else(|| "0".repeat(a.len()));
                let input=format!("start\nupdate {sibling} {b} {}\nupdate {target} {b} {old_oid}\nupdate {target} {second} {old_oid}\nprepare\ncommit\n", "0".repeat(a.len()));
                let input_path = ctx.dir.path().join("ref-transaction");
                std::fs::write(&input_path, input).unwrap();
                let output = ctx
                    .git_cmd()
                    .args([
                        "-c",
                        "alias.transaction=!git update-ref --stdin < \"$GHERRIT_REF_INPUT\"",
                        "transaction",
                    ])
                    .env("GHERRIT_REF_INPUT", &input_path)
                    .output()
                    .unwrap();
                assert!(!output.status.success());
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains("multiple updates for ref")
                );
                let actual =
                    ctx.git_cmd().args(["rev-parse", "--verify", target]).output().unwrap();
                assert_eq!(actual.status.success(), old.is_some());
                if let Some(old) = old {
                    assert_eq!(String::from_utf8(actual.stdout).unwrap().trim(), old);
                }
                assert!(!ctx
                    .git_cmd()
                    .args(["rev-parse", "--verify", sibling])
                    .output()
                    .unwrap()
                    .status
                    .success());
            }
        }
    }
    #[test]
    fn native_ref_transactions_agree_after_retaining_only_transmitted_commands() {
        for restore_old in [false, true] {
            let ctx = crate::TestContextBuilder::new("unused").with_initial_commit().build();
            let a = ctx.head_oid();
            ctx.commit("B");
            let b = ctx.head_oid();
            ctx.commit("C");
            let c = ctx.head_oid();
            let target = "refs/heads/Gretained";
            let omitted = "refs/heads/GalreadyDesired";
            for name in [target, omitted] {
                ctx.run_git(&["update-ref", name, &a]);
            }
            let native_refs = || {
                let output = ctx
                    .git_cmd()
                    .args(["for-each-ref", "--format=%(refname) %(objectname)"])
                    .output()
                    .unwrap();
                assert!(output.status.success());
                String::from_utf8(output.stdout)
                    .unwrap()
                    .lines()
                    .map(|line| {
                        let (name, oid) = line.split_once(' ').unwrap();
                        (name.to_owned(), oid.to_owned())
                    })
                    .collect::<BTreeMap<_, _>>()
            };
            let mut model = RefStore(BTreeMap::from([
                ("refs/heads/main".into(), c.clone()),
                (target.into(), a.clone()),
                (omitted.into(), a.clone()),
            ]));
            assert_eq!(native_refs(), model.0);
            let requested = [
                Update { name: target.into(), expected: Some(a.clone()), desired: b.clone() },
                Update { name: omitted.into(), expected: None, desired: a.clone() },
            ];
            let retained = model.advertise(&requested).unwrap();
            assert_eq!(
                retained,
                [requested[0].clone()],
                "equal desired wins before the absence lease"
            );
            // A different writer changes both refs after advertisement. The
            // omitted ref never participates in the retained receive request.
            for (name, desired) in [(target, &c), (omitted, &b)] {
                ctx.run_git(&["update-ref", name, desired]);
                model.0.insert(name.into(), desired.clone());
                assert_eq!(native_refs(), model.0);
            }
            if restore_old {
                ctx.run_git(&["update-ref", target, &a]);
                model.0.insert(target.into(), a.clone());
                assert_eq!(native_refs(), model.0);
            }
            let modeled = model.receive(&retained).is_ok();
            assert_eq!(modeled, restore_old);
            let input_path = ctx.dir.path().join("retained-transaction");
            // This exercises native atomic old-OID transactions, not a fresh
            // push advertisement. The requested-but-omitted ref stays absent.
            std::fs::write(
                &input_path,
                format!("start\nupdate {target} {b} {a}\nprepare\ncommit\n"),
            )
            .unwrap();
            let actual = ctx
                .git_cmd()
                .args([
                    "-c",
                    "alias.transaction=!git update-ref --stdin < \"$GHERRIT_REF_INPUT\"",
                    "transaction",
                ])
                .env("GHERRIT_REF_INPUT", &input_path)
                .output()
                .unwrap();
            assert_eq!(
                actual.status.success(),
                modeled,
                "restore_old={restore_old}: {}",
                String::from_utf8_lossy(&actual.stderr)
            );
            assert_eq!(native_refs(), model.0);
            assert_eq!(model.0[omitted], b);
        }
    }
}
