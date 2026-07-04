use std::process::Command;

fn git(dir: &std::path::Path, args: &[&str]) {
    let s = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(s.success(), "git {:?} failed", args);
}

#[cfg(unix)]
fn write_exe(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[test]
#[cfg(unix)]
fn debate_judge_mode_passes_protocol_judge_to_abe() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".bob")
        .join(format!("judge-protocol-fixture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).unwrap();

    let fake_builder = dir.join("bin/opencode");
    write_exe(
        &fake_builder,
        r#"#!/usr/bin/env bash
set -euo pipefail
dir=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--dir" ]; then
    shift
    dir="$1"
  fi
  shift || true
done
test -n "$dir"
printf new > "$dir/answer.txt"
"#,
    );

    let abe_args = dir.join("abe-args.txt");
    let fake_abe = dir.join("bin/abe");
    write_exe(
        &fake_abe,
        &format!(
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$@" > '{}'
printf '{{"agreements":["ok"],"disagreements":[]}}\n'
"#,
            abe_args.display()
        ),
    );

    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("answer.txt"), "old").unwrap();
    std::fs::write(dir.join(".gitignore"), "/.bob\n").unwrap();
    std::fs::write(
        dir.join("bob.yaml"),
        format!(
            "builder:\n  cmd: {}\n  timeout_secs: 5\n\
             judge:\n  cmd: {}\n  mode: debate\n  timeout_secs: 5\n  policy: blocking\n\
             verify:\n  cmds:\n    - test \"$(cat answer.txt)\" = new\n\
             loop:\n  max_iterations: 1\n  max_walltime_secs: 60\n\
             scope:\n  max_changed_files: 2\n  max_changed_lines: 20\n  allow_paths: []\n\
             apply: false\n\
             artifacts:\n  dir: .bob/runs\n",
            fake_builder.display(),
            fake_abe.display()
        ),
    )
    .unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);

    let status = Command::new(env!("CARGO_BIN_EXE_bob"))
        .args(["build", "change answer"])
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(status.success(), "bob should converge");

    let args = std::fs::read_to_string(&abe_args).unwrap();
    let first = args.lines().take(5).collect::<Vec<_>>();
    assert_eq!(first, ["debate", "--json", "--protocol", "judge", "--"]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[cfg(unix)]
fn propose_out_of_scope_edit_never_leaks_into_main_bug_26() {
    // Regression guard for bug #26: `bob build --propose --jobs 2` (two
    // concurrent, unapplied builds sharing one repo) reportedly leaked a
    // NON-editable file edit outside the run's editable_paths/allow_paths
    // allowlist into the MAIN working tree. Drives the real CLI end-to-end
    // (fake `opencode` plays the builder) with a scope that only allows
    // `src/`, while the fake builder always edits `packages/worldgen/...`
    // (outside the allowlist) — exactly the reported shape. Runs two
    // concurrent `bob build` invocations against one shared repo, looped,
    // since #26 was reported under --jobs 2 concurrency.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".bob")
        .join(format!("bug26-fixture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();

    let fake_builder = dir.join("bin/opencode");
    write_exe(
        &fake_builder,
        r#"#!/usr/bin/env bash
set -euo pipefail
d=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--dir" ]; then
    shift
    d="$1"
  fi
  shift || true
done
test -n "$d"
mkdir -p "$d/packages/worldgen/src"
printf 'export const leaked = true;\n' > "$d/packages/worldgen/src/index.ts"
"#,
    );

    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("src/seed.txt"), "x\n").unwrap();
    std::fs::write(dir.join(".gitignore"), "/.bob\n").unwrap();
    std::fs::write(
        dir.join("bob.yaml"),
        format!(
            "builder:\n  cmd: {}\n  timeout_secs: 5\n\
             judge:\n  cmd: abe\n  mode: validate\n  timeout_secs: 5\n\
             verify:\n  cmds: []\n\
             loop:\n  max_iterations: 1\n  max_walltime_secs: 60\n\
             scope:\n  max_changed_files: 10\n  max_changed_lines: 200\n  allow_paths: [\"src/\"]\n\
             apply: false\n\
             artifacts:\n  dir: .bob/runs\n",
            fake_builder.display(),
        ),
    )
    .unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);

    let leaked = dir.join("packages/worldgen/src/index.ts");
    for i in 0..20 {
        let spawn_one = || {
            Command::new(env!("CARGO_BIN_EXE_bob"))
                .args(["build", "change something", "--allow-path", "src/", "--json"])
                .current_dir(&dir)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        };
        // Two concurrent, unapplied ("propose") builds sharing one repo — the
        // --jobs 2 shape #26 was reported under.
        let a = spawn_one();
        let b = spawn_one();
        let oa = a.wait_with_output().unwrap();
        let ob = b.wait_with_output().unwrap();

        for (who, out) in [("A", &oa), ("B", &ob)] {
            assert!(
                !out.status.success(),
                "iter {i} build {who}: expected non-zero exit (scope-exceeded), got success: {}",
                String::from_utf8_lossy(&out.stdout)
            );
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains("ScopeExceeded"),
                "iter {i} build {who}: expected ScopeExceeded stop_reason: {stdout}"
            );
        }

        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "iter {i}: main tree dirty (bug #26 reproduced):\n{}",
            String::from_utf8_lossy(&status.stdout)
        );
        assert!(
            !leaked.exists(),
            "iter {i}: out-of-scope file leaked into main working tree (bug #26 reproduced)"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builds_and_converges_on_a_trivial_task() {
    if std::env::var("BOB_INTEGRATION").is_err() {
        eprintln!("skipping: set BOB_INTEGRATION=1 with opencode+abe configured");
        return;
    }

    // Place the fixture repo inside the crate's gitignored .bob/ dir: not under
    // /tmp (so opencode's sandbox accepts it) and not external to the crate (so
    // it needs no out-of-tree write permission).
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".bob")
        .join(format!("it-fixture-{}", std::process::id()));

    // Clean up any previous run.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();

    let g = |a: &[&str]| git(&dir, a);
    g(&["init", "-q"]);
    g(&["config", "user.email", "t@t"]);
    g(&["config", "user.name", "t"]);

    // A real failing task: add() returns 0, so the test fails until the builder fixes it.
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { 0 }\n\
         #[test] fn t() { assert_eq!(add(2, 2), 4); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname=\"it\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    // Gitignore build output so opencode's `cargo test` (and bob's worktree) don't
    // pollute the captured diff with target/ artifacts.
    std::fs::write(dir.join(".gitignore"), "/target\n/.bob\n").unwrap();

    // Write bob.yaml so the verify gate and tool config are present.
    std::fs::write(
        dir.join("bob.yaml"),
        "builder:\n  cmd: opencode\n  timeout_secs: 600\n\
         judge:\n  cmd: abe\n  mode: validate\n  timeout_secs: 600\n\
         verify:\n  cmds:\n    - cargo test\n\
         loop:\n  max_iterations: 3\n  max_walltime_secs: 1800\n\
         scope:\n  max_changed_files: 20\n  max_changed_lines: 800\n  allow_paths: []\n\
         apply: false\n\
         artifacts:\n  dir: .bob/runs\n",
    )
    .unwrap();

    g(&["add", "."]);
    g(&["commit", "-qm", "init"]);

    // Act: run bob with a verify gate of `cargo test`. bob exits non-zero unless it converges.
    let bob = env!("CARGO_BIN_EXE_bob");
    let status = Command::new(bob)
        .args([
            "build",
            "Implement add() so the test passes",
            "--max-iters",
            "3",
            "--apply",
        ])
        .current_dir(&dir)
        .status()
        .unwrap();

    // Assert: bob converged (exit 0) and the test now passes in the real tree.
    assert!(status.success(), "bob should converge");
    let test_status = Command::new("cargo")
        .arg("test")
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(test_status.success(), "applied code should pass the test");

    // Clean up the fixture.
    let _ = std::fs::remove_dir_all(&dir);
}
