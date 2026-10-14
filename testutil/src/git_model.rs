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
}
