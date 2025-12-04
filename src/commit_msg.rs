use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::{
    cmd,
    util::{self, CommandExt as _, ResultExt as _},
};

pub fn run(msg_file: &Path) {
    if !msg_file.exists() {
        log::error!("file does not exist: {}", msg_file.display());
        std::process::exit(1);
    }

    // 1. Get current branch
    let (repo, branch_name) = match util::get_current_branch() {
        Ok(v) => v,
        Err(_) => return, // If detached or error, we might just exit 0 like the script?
                          // The script checks for rebase state if detached.
                          // For now, let's stick to simple branch detection.
                          // If we can't get the branch, we probably shouldn't enforce gherrit logic unless we are in a rebase of a managed branch.
                          // The script logic for detached HEAD is complex.
                          // "If detached (empty BRANCH), check if we are in a rebase..."
                          // Let's try to support that if possible, or just skip if we can't determine branch.
    };

    // 2. Check if managed
    let key = format!("branch.{branch_name}.gherritManaged");
    let managed = util::get_config_string(&repo, &key)
        .unwrap_or_exit("Failed to get config")
        .as_deref()
        == Some("true");

    if !managed {
        return;
    }

    // 3. Squash check
    let msg_content = fs::read_to_string(msg_file).unwrap_or_exit("Failed to read msg file");
    if msg_content
        .lines()
        .next()
        .map_or(false, |l| l.starts_with("squash! "))
    {
        return;
    }

    // 4. Calculate Change-ID (random)
    // random=$({ git var GIT_COMMITTER_IDENT ; echo "$refhash" ; cat "$1"; } | git hash-object --stdin)

    // Get refhash
    let refhash = if let Ok(head) = repo.head_id() {
        head.to_string()
    } else {
        // Empty tree hash: 4b825dc642cb6eb9a060e54bf8d69288fbee4904
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()
    };

    let committer_ident = cmd!("git var GIT_COMMITTER_IDENT").unwrap_output();
    let committer_ident = String::from_utf8_lossy(&committer_ident.stdout)
        .trim()
        .to_string();

    let mut hash_input = Vec::new();
    hash_input.extend_from_slice(committer_ident.as_bytes());
    hash_input.push(b'\n');
    hash_input.extend_from_slice(refhash.as_bytes());
    hash_input.push(b'\n');
    hash_input.extend_from_slice(msg_content.as_bytes());

    let mut hash_cmd = Command::new("git");
    hash_cmd.arg("hash-object").arg("--stdin");
    hash_cmd.stdin(Stdio::piped());
    hash_cmd.stdout(Stdio::piped());

    let mut child = hash_cmd
        .spawn()
        .unwrap_or_exit("Failed to spawn git hash-object");
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&hash_input)
            .unwrap_or_exit("Failed to write to hash-object stdin");
    }
    let output = child
        .wait_with_output()
        .unwrap_or_exit("Failed to wait for hash-object");
    let random_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // 5. Determine trailer token and value
    let review_url = util::get_config_string(&repo, "gerrit.reviewUrl").unwrap_or(None);
    let (token, value, pattern) = if let Some(url) = review_url {
        let url = url.trim_end_matches('/');
        (
            "Link",
            format!("{}/id/I{}", url, random_hash),
            ".*/id/I[0-9a-f]{40}",
        )
    } else {
        ("gherrit-pr-id", format!("G{}", random_hash), ".*")
    };

    // 6. Check if trailer exists
    // if git interpret-trailers --parse < "$1" | grep -q "^$token: $pattern$" ; then exit 0; fi
    let trailers =
        cmd!("git interpret-trailers --parse", msg_file.to_str().unwrap()).unwrap_output();
    let trailers_str = String::from_utf8_lossy(&trailers.stdout);
    let regex = regex::Regex::new(&format!("^{}: {}$", token, pattern)).unwrap();
    if trailers_str.lines().any(|l| regex.is_match(l)) {
        return;
    }

    // 7. Insert trailer
    // The script uses a complex dance with Signed-off-by SENTINEL to force the trailer before Signed-off-by.
    // We can try to replicate that or just use `git interpret-trailers --where before` if we don't care about the exact `sed` hack.
    // The script says: "Avoid the --where option which only appeared in Git 2.15".
    // Git 2.15 is very old (2017). We can probably assume a newer git.
    // However, the script logic puts it *before* Signed-off-by.
    // `git interpret-trailers --trailer "Token: Value" --where start` might work?
    // Or `--where before` relative to Signed-off-by?

    // Let's try to use `git interpret-trailers` directly with `--if-exists addIfDifferentNeighbor` or similar?
    // The script's goal is: "Make sure the trailer appears before any Signed-off-by trailers".

    // Let's implement the exact logic from the script to be safe, but maybe in a cleaner way if possible.
    // Actually, shelling out to `git` with the exact arguments is fine.

    // Step 7a: Insert SENTINEL
    // git interpret-trailers --trailer "Signed-off-by: SENTINEL" < "$1" > "$dest-2"
    let sentinel_cmd = Command::new("git")
        .args(&["interpret-trailers", "--trailer", "Signed-off-by: SENTINEL"])
        .stdin(Stdio::from(fs::File::open(msg_file).unwrap()))
        .output()
        .unwrap_or_exit("Failed to insert sentinel");

    let dest2 = sentinel_cmd.stdout;

    // Step 7b: Insert actual trailer and cleanup
    // git -c trailer.where=before interpret-trailers --trailer "Signed-off-by: $token: $value" < "$dest-2" | sed ...

    let mut trailer_cmd = Command::new("git");
    trailer_cmd.args(&[
        "-c",
        "trailer.where=before",
        "interpret-trailers",
        "--trailer",
        &format!("Signed-off-by: {}: {}", token, value),
    ]);
    trailer_cmd.stdin(Stdio::piped());
    trailer_cmd.stdout(Stdio::piped());

    let mut child = trailer_cmd
        .spawn()
        .unwrap_or_exit("Failed to spawn trailer cmd");
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&dest2)
            .unwrap_or_exit("Failed to write to trailer cmd");
    }
    let output = child
        .wait_with_output()
        .unwrap_or_exit("Failed to wait for trailer cmd");

    let output_str = String::from_utf8_lossy(&output.stdout);

    // sed -e "s/^Signed-off-by: \($token: \)/\1/" -e "/^Signed-off-by: SENTINEL/d"
    let mut final_content = String::new();
    for line in output_str.lines() {
        if line == "Signed-off-by: SENTINEL" {
            continue;
        }
        if let Some(rest) = line.strip_prefix(&format!("Signed-off-by: {}: ", token)) {
            final_content.push_str(&format!("{}: {}\n", token, rest));
        } else {
            final_content.push_str(line);
            final_content.push('\n');
        }
    }

    fs::write(msg_file, final_content).unwrap_or_exit("Failed to write msg file");
}
