//! Several monorepos splicing the same public repo.
//!
//! Monorepo A is the first publisher and has no `id`: its exports carry the legacy
//! `Monosplice-Source: <sha>`. B joins later with `id = "b"`, so its exports carry
//! `Monosplice-Source: <sha> b`. Each must treat the other's exports as ordinary public commits:
//! pulled like any outside contribution, never mistaken for its own mapping.

mod common;

use common::{
    clone_remote, run_monosplice, run_ok, shared_fixture, write_config_with_id, SharedFixture,
    TestRepo,
};

/// `Monosplice-Source` values in a commit message, in order.
fn source_values(message: &str) -> Vec<String> {
    message
        .lines()
        .filter_map(|line| line.strip_prefix("Monosplice-Source: "))
        .map(str::to_string)
        .collect()
}

fn head_message(repo: &TestRepo) -> String {
    repo.messages("HEAD").pop().expect("a head commit")
}

fn pub_count(fx: &SharedFixture) -> usize {
    fx.pub_repo.subjects("HEAD").len()
}

/// B attached to A's published history with one snapshot commit.
fn b_attached(fx: &SharedFixture) -> TestRepo {
    let b = fx.monorepo("mono-b", Some("b"), false);
    run_ok(&b.dir, &["attach", "core", &fx.pub_dir]);
    b
}

// --- attach ---

#[test]
fn b_attaches_with_import_history_to_a_published_history_unconfigured() {
    let fx = shared_fixture();
    let b = fx.monorepo("mono-b", Some("b"), false);

    let res = run_ok(&b.dir, &["attach", "core", &fx.pub_dir, "--import-history"]);
    assert!(
        res.stdout
            .contains(&format!("replayed {} commit(s)", pub_count(&fx))),
        "every public commit is replayed:\n{}",
        res.stdout
    );
    assert_eq!(
        b.tree_sha("HEAD", Some("core")),
        fx.pub_repo.tree_sha("HEAD", None)
    );
    // The imported commit keeps A's Source trailer, and gains its own Origin.
    let message = head_message(&b);
    assert_eq!(source_values(&message), vec![fx.a.head()], "{message}");
    assert!(message.contains("Monosplice-Origin: "), "{message}");

    let st = run_ok(&b.dir, &["status"]);
    assert!(st.stdout.contains("core: in sync"), "{}", st.stdout);
}

#[test]
fn b_attaches_with_import_history_to_a_published_history_configured() {
    let fx = shared_fixture();
    let b = fx.monorepo("mono-b", Some("b"), true);

    let res = run_ok(&b.dir, &["attach", "core", "--import-history"]);
    assert!(
        res.stdout
            .contains(&format!("{} commit(s)", pub_count(&fx))),
        "every public commit is replayed:\n{}",
        res.stdout
    );
    assert_eq!(
        b.tree_sha("HEAD", Some("core")),
        fx.pub_repo.tree_sha("HEAD", None)
    );
    let st = run_ok(&b.dir, &["status"]);
    assert!(st.stdout.contains("core: in sync"), "{}", st.stdout);
}

#[test]
fn b_attaches_with_a_snapshot_and_doctor_notes_the_other_publisher() {
    let fx = shared_fixture();
    let b = b_attached(&fx);

    let st = run_ok(&b.dir, &["status"]);
    assert!(st.stdout.contains("core: in sync"), "{}", st.stdout);

    let doc = run_monosplice(&b.dir, &["doctor"]);
    assert_eq!(doc.exit_code, 0, "{}\n{}", doc.stdout, doc.stderr);
    assert!(
        doc.stdout.contains(&format!(
            "{} commit(s) published by another monorepo",
            pub_count(&fx)
        )),
        "{}",
        doc.stdout
    );

    let json = run_monosplice(&b.dir, &["doctor", "--json"]);
    assert_eq!(json.exit_code, 0, "{}", json.stdout);
    assert!(
        json.stdout.contains("published by another monorepo"),
        "{}",
        json.stdout
    );
}

// --- the round trip ---

#[test]
fn b_pulls_what_a_publishes_next() {
    let fx = shared_fixture();
    let b = b_attached(&fx);

    fx.a.commit(
        "feat: three from a",
        &[("core/src/three.ts", Some("export const three = 3\n"))],
    );
    run_ok(&fx.a.dir, &["push"]);

    let st = run_ok(&b.dir, &["status"]);
    assert!(st.stdout.contains("1 to pull"), "{}", st.stdout);

    let pull = run_ok(&b.dir, &["pull"]);
    assert!(pull.stdout.contains("imported 1 commit"), "{}", pull.stdout);
    assert_eq!(b.read("core/src/three.ts"), "export const three = 3\n");

    let push = run_ok(&b.dir, &["push"]);
    assert!(push.stdout.contains("core: up to date"), "{}", push.stdout);
}

#[test]
fn exports_flow_both_ways_without_ping_pong() {
    let fx = shared_fixture();
    let b = b_attached(&fx);

    // B publishes: its trailer is tagged with its id.
    let b_sha = b.commit(
        "feat: from b",
        &[("core/src/b.ts", Some("export const b = 1\n"))],
    );
    let push = run_ok(&b.dir, &["push"]);
    assert!(push.stdout.contains("exported 1 commit"), "{}", push.stdout);
    assert_eq!(
        source_values(&head_message(&fx.pub_repo)),
        vec![format!("{b_sha} b")]
    );

    // A pulls it as an outside contribution, and has nothing to send back.
    let pull = run_ok(&fx.a.dir, &["pull"]);
    assert!(pull.stdout.contains("imported 1 commit"), "{}", pull.stdout);
    let push = run_ok(&fx.a.dir, &["push"]);
    assert!(push.stdout.contains("core: up to date"), "{}", push.stdout);
    // ...and B has nothing to pull back.
    let pull = run_ok(&b.dir, &["pull"]);
    assert!(pull.stdout.contains("core: up to date"), "{}", pull.stdout);

    // A second round, the other way first: still exactly one public commit per change.
    let before = pub_count(&fx);
    fx.a.commit(
        "feat: second round from a",
        &[("core/src/a2.ts", Some("export const a2 = 2\n"))],
    );
    run_ok(&fx.a.dir, &["push"]);
    run_ok(&b.dir, &["pull"]);
    let push = run_ok(&b.dir, &["push"]);
    assert!(push.stdout.contains("core: up to date"), "{}", push.stdout);
    b.commit(
        "feat: second round from b",
        &[("core/src/b2.ts", Some("export const b2 = 2\n"))],
    );
    run_ok(&b.dir, &["push"]);
    run_ok(&fx.a.dir, &["pull"]);
    for repo in [&fx.a, &b] {
        let push = run_ok(&repo.dir, &["push"]);
        assert!(push.stdout.contains("core: up to date"), "{}", push.stdout);
        let pull = run_ok(&repo.dir, &["pull"]);
        assert!(pull.stdout.contains("core: up to date"), "{}", pull.stdout);
        let doc = run_monosplice(&repo.dir, &["doctor"]);
        assert_eq!(doc.exit_code, 0, "{}", doc.stdout);
    }
    assert_eq!(pub_count(&fx), before + 2, "no ping-pong");
    assert_eq!(
        fx.a.tree_sha("HEAD", Some("core")),
        b.tree_sha("HEAD", Some("core"))
    );
}

#[test]
fn a_conflicted_import_in_b_is_exported_with_only_b_s_source_and_reaches_a() {
    let fx = shared_fixture();
    let b = b_attached(&fx);

    fx.a.commit(
        "docs: a's readme",
        &[("core/README.md", Some("# core by a\n"))],
    );
    run_ok(&fx.a.dir, &["push"]);
    b.commit(
        "docs: b's readme",
        &[("core/README.md", Some("# core by b\n"))],
    );

    let pull = run_monosplice(&b.dir, &["pull"]);
    assert_ne!(pull.exit_code, 0, "expected a conflict:\n{}", pull.stdout);
    assert!(pull.stderr.contains("conflicted"), "{}", pull.stderr);
    b.write("core/README.md", "# core by a and b\n");
    b.git(&["add", "core/README.md"]);
    run_ok(&b.dir, &["pull", "--continue"]);

    let resolution = b.head();
    run_ok(&b.dir, &["push"]);
    let message = head_message(&fx.pub_repo);
    assert_eq!(
        source_values(&message),
        vec![format!("{resolution} b")],
        "A's Source trailer is dropped, B's appended:\n{message}"
    );
    assert_eq!(
        fx.pub_repo.file_at("HEAD", "README.md"),
        "# core by a and b"
    );

    let pull = run_ok(&fx.a.dir, &["pull"]);
    assert!(pull.stdout.contains("imported 2 commit"), "{}", pull.stdout);
    assert_eq!(fx.a.read("core/README.md"), "# core by a and b\n");
    let push = run_ok(&fx.a.dir, &["push"]);
    assert!(push.stdout.contains("core: up to date"), "{}", push.stdout);
}

// --- refusals ---

#[test]
fn b_must_pull_a_s_newer_export_before_pushing() {
    let fx = shared_fixture();
    let b = b_attached(&fx);

    fx.a.commit(
        "feat: newer from a",
        &[("core/src/newer.ts", Some("export const newer = 1\n"))],
    );
    run_ok(&fx.a.dir, &["push"]);
    b.commit(
        "feat: from b",
        &[("core/src/b.ts", Some("export const b = 1\n"))],
    );
    let pub_head = fx.pub_repo.head();

    let push = run_monosplice(&b.dir, &["push"]);
    assert_ne!(push.exit_code, 0, "{}", push.stdout);
    assert!(
        push.stderr.contains("Run `monosplice pull core` first"),
        "{}",
        push.stderr
    );
    assert_eq!(fx.pub_repo.head(), pub_head, "nothing was pushed");
}

#[test]
fn a_monorepo_without_an_id_may_not_publish_beside_untagged_trailers() {
    let fx = shared_fixture();
    let b = fx.monorepo("mono-b", None, false);
    run_ok(&b.dir, &["attach", "core", &fx.pub_dir]);
    b.commit(
        "feat: from b",
        &[("core/src/b.ts", Some("export const b = 1\n"))],
    );
    let pub_head = fx.pub_repo.head();

    for args in [&["push"][..], &["push", "--dry-run"][..]] {
        let push = run_monosplice(&b.dir, args);
        assert_ne!(push.exit_code, 0, "{}", push.stdout);
        assert!(
            push.stderr.contains("Set `id = ")
                && push.stderr.contains("published by another monorepo"),
            "{}",
            push.stderr
        );
    }
    assert_eq!(fx.pub_repo.head(), pub_head, "nothing was pushed");

    // Pulling is still fine: only publishing is ambiguous.
    run_ok(&b.dir, &["pull"]);
}

#[test]
fn a_duplicate_id_is_a_doctor_problem_and_refuses_the_push() {
    let fx = shared_fixture();
    let b = b_attached(&fx);
    // C picked the same id as B, and attached before B published anything.
    let c = fx.monorepo("mono-c", Some("b"), false);
    run_ok(&c.dir, &["attach", "core", &fx.pub_dir]);

    b.commit(
        "feat: from b",
        &[("core/src/b.ts", Some("export const b = 1\n"))],
    );
    run_ok(&b.dir, &["push"]);
    c.commit(
        "feat: from c",
        &[("core/src/c.ts", Some("export const c = 1\n"))],
    );
    let pub_head = fx.pub_repo.head();

    let doc = run_monosplice(&c.dir, &["doctor"]);
    assert_eq!(doc.exit_code, 1, "{}", doc.stdout);
    assert!(doc.stdout.contains("duplicate id?"), "{}", doc.stdout);

    let push = run_monosplice(&c.dir, &["push"]);
    assert_ne!(push.exit_code, 0, "{}", push.stdout);
    assert!(
        push.stderr.contains("does not exist in this clone")
            && push.stderr.contains("`id = \"b\"`"),
        "{}",
        push.stderr
    );
    assert_eq!(fx.pub_repo.head(), pub_head, "nothing was published");
}

// --- legacy edges ---

#[test]
fn a_shallow_clone_of_a_keeps_its_untagged_trailers_and_doctor_says_so() {
    let fx = shared_fixture();
    // HEAD is not itself an export, so the shallow clone holds none of the exported commits.
    fx.a.commit(
        "chore: a local change",
        &[("private/notes.md", Some("x\n"))],
    );
    let url = format!("file://{}", fx.a.dir.display());
    let shallow = fx.sandbox.path().join("shallow");
    let res = fx.a.git_try(&[
        "clone",
        "--depth",
        "1",
        &url,
        &shallow.display().to_string(),
    ]);
    assert_eq!(res.exit_code, 0, "{}", res.stderr);
    let shallow = TestRepo::new(shallow);

    // Today's behaviour: the trailers are ours and unresolvable, so push refuses.
    let push = run_monosplice(&shallow.dir, &["push"]);
    assert_ne!(push.exit_code, 0, "{}", push.stdout);
    assert!(
        push.stderr.contains("does not exist in this clone"),
        "{}",
        push.stderr
    );

    let doc = run_monosplice(&shallow.dir, &["doctor"]);
    assert_eq!(doc.exit_code, 1, "{}", doc.stdout);
    assert!(
        doc.stdout.contains("shallow clone: untagged"),
        "{}",
        doc.stdout
    );
}

#[test]
fn a_setting_its_id_later_owns_old_and_new_trailers() {
    let fx = shared_fixture();
    let b = b_attached(&fx);

    write_config_with_id(&fx.a, Some("a"), &[&fx.core_block()]);
    fx.a.commit("chore: name monorepo a", &[]);
    let a_sha = fx.a.commit(
        "feat: tagged from a",
        &[("core/src/tagged.ts", Some("export const tagged = 1\n"))],
    );
    run_ok(&fx.a.dir, &["push"]);
    assert_eq!(
        source_values(&head_message(&fx.pub_repo)),
        vec![format!("{a_sha} a")]
    );

    let doc = run_monosplice(&fx.a.dir, &["doctor"]);
    assert_eq!(doc.exit_code, 0, "{}", doc.stdout);
    assert!(
        !doc.stdout.contains("another monorepo"),
        "A owns both forms:\n{}",
        doc.stdout
    );
    let st = run_ok(&fx.a.dir, &["status"]);
    assert!(st.stdout.contains("core: in sync"), "{}", st.stdout);

    // B sees both forms as A's: one more to pull, then in sync.
    let pull = run_ok(&b.dir, &["pull"]);
    assert!(pull.stdout.contains("imported 1 commit"), "{}", pull.stdout);
    let push = run_ok(&b.dir, &["push"]);
    assert!(push.stdout.contains("core: up to date"), "{}", push.stdout);
}

#[test]
fn a_fresh_clone_of_a_still_owns_the_untagged_trailers() {
    let fx = shared_fixture();
    let clone = clone_remote(
        fx.sandbox.path(),
        &format!("file://{}", fx.a.dir.display()),
        "a2",
    );
    let doc = run_monosplice(&clone.dir, &["doctor"]);
    assert_eq!(doc.exit_code, 0, "{}", doc.stdout);
    assert!(!doc.stdout.contains("another monorepo"), "{}", doc.stdout);
    clone.commit(
        "feat: from the clone",
        &[("core/src/clone.ts", Some("export const clone = 1\n"))],
    );
    let push = run_ok(&clone.dir, &["push"]);
    assert!(push.stdout.contains("exported 1 commit"), "{}", push.stdout);
    assert_eq!(
        source_values(&head_message(&fx.pub_repo)),
        vec![clone.head()],
        "still the legacy form: A has no id"
    );
}

/// Review finding 3: a branch of a legacy single monorepo, forked after attach but before the
/// first export. Its own export is not an ancestor of that branch's HEAD, but the commit is right
/// here in the object database — another branch of this monorepo, not another monorepo.
#[test]
fn an_old_branch_of_a_legacy_monorepo_does_not_reimport_its_own_export() {
    let sb = common::sandbox();
    let pub_dir = common::make_bare_remote(sb.path(), "core-pub");
    let up = clone_remote(sb.path(), &pub_dir, "up");
    up.commit("upstream root", &[("core0.txt", Some("0\n"))]);
    up.git(&["push", "-q", "origin", "main"]);

    let a = common::make_repo(sb.path(), "mono-a");
    write_config_with_id(&a, None, &[]);
    a.commit("a init", &[("app.txt", Some("a\n"))]);
    run_ok(&a.dir, &["attach", "core", &pub_dir, "--import-history"]);
    a.git(&["branch", "old"]);
    a.commit("a mine", &[("core/mine.txt", Some("m\n"))]);
    run_ok(&a.dir, &["push"]);

    a.git(&["checkout", "-q", "old"]);
    let st = run_ok(&a.dir, &["status"]);
    assert!(st.stdout.contains("core: in sync"), "{}", st.stdout);
    let pull = run_ok(&a.dir, &["pull"]);
    assert!(pull.stdout.contains("core: up to date"), "{}", pull.stdout);
}

/// Review finding 1: a shallow clone of B cannot see whether A's untagged trailers are its own.
/// Guessing "ours" skips A's commits — the original bug — and after `fetch --unshallow` a push
/// would publish a tree without them. With an `id` set, a shallow clone refuses instead.
#[test]
fn a_shallow_clone_of_a_monorepo_with_an_id_refuses_instead_of_guessing() {
    let fx = shared_fixture();
    let b = fx.monorepo("mono-b", Some("b"), false);
    run_ok(&b.dir, &["attach", "core", &fx.pub_dir, "--import-history"]);
    let b_remote = fx.sandbox.path().join("b-remote.git");
    b.git(&[
        "clone",
        "-q",
        "--bare",
        ".",
        &b_remote.display().to_string(),
    ]);
    let shallow = fx.sandbox.path().join("bs");
    b.git(&[
        "clone",
        "-q",
        "--depth",
        "1",
        &format!("file://{}", b_remote.display()),
        &shallow.display().to_string(),
    ]);
    let shallow = TestRepo::new(shallow);

    fx.a.commit("a two", &[("core/two.txt", Some("2\n"))]);
    run_ok(&fx.a.dir, &["push"]);
    let contrib = clone_remote(fx.sandbox.path(), &fx.pub_dir, "contrib");
    contrib.commit("outside c3", &[("c3.txt", Some("3\n"))]);
    contrib.git(&["push", "-q", "origin", "main"]);
    let pub_head = fx.pub_repo.head();

    for args in [&["pull"][..], &["status"][..], &["push", "--dry-run"][..]] {
        let res = run_monosplice(&shallow.dir, args);
        assert_ne!(res.exit_code, 0, "{args:?}: {}", res.stdout);
        assert!(
            res.stderr.contains("shallow clone") && res.stderr.contains("git fetch --unshallow"),
            "{args:?}: {}",
            res.stderr
        );
    }
    let doc = run_monosplice(&shallow.dir, &["doctor"]);
    assert_eq!(doc.exit_code, 1, "{}", doc.stdout);
    assert!(
        doc.stdout.contains("git fetch --unshallow"),
        "{}",
        doc.stdout
    );
    assert!(!shallow.exists("core/c3.txt"), "nothing was imported");

    // Unshallowed, the history decides: A's trailers are A's, and both commits come in.
    shallow.git(&["fetch", "-q", "--unshallow"]);
    let pull = run_ok(&shallow.dir, &["pull"]);
    assert!(pull.stdout.contains("imported 2 commit"), "{}", pull.stdout);
    assert_eq!(shallow.read("core/two.txt"), "2\n");
    assert_eq!(fx.pub_repo.head(), pub_head);
}

/// Review finding 2: monosplice 1.0 read B's tagged export as its own and skipped it on pull,
/// importing only the commit after it. Upgraded to 1.1, A sees that export as foreign, but
/// reflected by ancestry — and its next push would publish a tree without B's file. A skipped
/// foreign commit must stop every command until it is imported.
#[test]
fn a_foreign_export_an_older_monosplice_skipped_refuses_the_push() {
    let fx = shared_fixture();
    let b = b_attached(&fx);
    b.commit("b feature", &[("core/fromb.txt", Some("b\n"))]);
    run_ok(&b.dir, &["push"]);
    let b_export = fx.pub_repo.head();
    let contrib = clone_remote(fx.sandbox.path(), &fx.pub_dir, "contrib");
    contrib.commit("outside c3", &[("c3.txt", Some("3\n"))]);
    contrib.git(&["push", "-q", "origin", "main"]);
    let c3 = fx.pub_repo.head();

    // What a 1.0 `pull` did: skip B's export as "ours", import c3 alone, diffed against its parent.
    fx.a.git(&["fetch", "-q", &fx.pub_dir, "main"]);
    let diff = fx.a.git(&["diff", "--binary", &b_export, &c3]);
    fx.a.git_with(
        &["apply", "--index", "--directory=core", "-"],
        &[],
        Some(&format!("{diff}\n")),
    );
    fx.a.commit(&format!("outside c3\n\nMonosplice-Origin: {c3}"), &[]);
    assert!(!fx.a.exists("core/fromb.txt"));

    fx.a.commit("a two", &[("core/a2.txt", Some("a\n"))]);
    for args in [
        &["push"][..],
        &["push", "--dry-run"][..],
        &["status"][..],
        &["pull"][..],
    ] {
        let res = run_monosplice(&fx.a.dir, args);
        assert_ne!(res.exit_code, 0, "{args:?}: {}", res.stdout);
        assert!(
            res.stderr.contains(&b_export) && res.stderr.contains("older monosplice"),
            "{args:?}: {}",
            res.stderr
        );
    }
    assert_eq!(fx.pub_repo.head(), c3, "nothing was published");
    let doc = run_monosplice(&fx.a.dir, &["doctor"]);
    assert_eq!(doc.exit_code, 1, "{}", doc.stdout);

    // The recovery the message names, run as printed: import the skipped commit by hand.
    let refusal = run_monosplice(&fx.a.dir, &["push"]).stderr;
    let ran = common::run_advice(&fx.a, &refusal);
    assert_eq!(ran.len(), 2, "{ran:?}");
    let push = run_ok(&fx.a.dir, &["push"]);
    assert!(push.stdout.contains("exported"), "{}", push.stdout);
    assert_eq!(fx.pub_repo.file_at("HEAD", "fromb.txt"), "b");
}

// --- round 2 review: the skipped-foreign guard must not fire on healthy histories ---

/// Clone a monorepo through the pack protocol with `--depth N`.
fn shallow_clone(fx: &SharedFixture, from: &TestRepo, depth: u32, name: &str) -> TestRepo {
    let dest = fx.sandbox.path().join(name);
    from.git(&[
        "clone",
        "-q",
        "--no-local",
        "--depth",
        &depth.to_string(),
        &format!("file://{}", from.dir.display()),
        &dest.display().to_string(),
    ]);
    TestRepo::new(dest)
}

fn assert_healthy(repo: &TestRepo, what: &str) {
    for args in [&["status"][..], &["push", "--dry-run"][..]] {
        let res = run_monosplice(&repo.dir, args);
        assert_eq!(
            res.exit_code, 0,
            "{what}: {args:?}\n{}\n{}",
            res.stdout, res.stderr
        );
    }
}

/// monosplice's own first-parent bypass: a side branch carrying a foreign export merges main in
/// and main fast-forwards to it. The merge's import reproduces its public commit exactly, so the
/// foreign commit came in with it.
#[test]
fn a_foreign_commit_carried_in_by_a_merge_import_is_not_skipped() {
    let fx = shared_fixture();
    let contrib = clone_remote(fx.sandbox.path(), &fx.pub_dir, "contrib");
    contrib.commit("outside X", &[("X.txt", Some("X\n"))]);
    contrib.git(&["push", "-q", "origin", "main"]);
    run_ok(&fx.a.dir, &["pull"]);

    contrib.git(&["checkout", "-q", "-b", "side", "HEAD~1"]);
    contrib.commit(
        &format!("b export\n\nMonosplice-Source: {} b", "e".repeat(40)),
        &[("s1.txt", Some("s1\n"))],
    );
    contrib.merge("main", "merge main", &[]);
    contrib.git(&["push", "-q", "origin", "side:main"]);

    let pull = run_ok(&fx.a.dir, &["pull"]);
    assert!(pull.stdout.contains("imported 1 commit"), "{}", pull.stdout);
    assert_eq!(fx.a.read("core/s1.txt"), "s1\n");
    assert_healthy(&fx.a, "after the merge import");
}

/// B detaches, removes the folder, and attaches again by snapshot: the snapshot reproduces the
/// public head exactly, so every foreign commit below it came in with it.
#[test]
fn a_reattach_by_snapshot_is_not_a_skipped_foreign_commit() {
    let fx = shared_fixture();
    let b = fx.monorepo("mono-b", Some("b"), false);
    run_ok(&b.dir, &["attach", "core", &fx.pub_dir, "--import-history"]);
    b.commit("b feature", &[("core/fromb.txt", Some("b\n"))]);
    run_ok(&b.dir, &["push"]);
    fx.a.commit("a2", &[("core/a2.txt", Some("a\n"))]);
    run_ok(&fx.a.dir, &["pull"]);
    run_ok(&fx.a.dir, &["push"]);

    run_ok(&b.dir, &["detach", "core"]);
    fx.a.commit("a3", &[("core/a3.txt", Some("a\n"))]);
    run_ok(&fx.a.dir, &["push"]);
    b.git(&["rm", "-rq", "core"]);
    b.commit("rm core", &[]);
    run_ok(&b.dir, &["attach", "core", &fx.pub_dir]);
    assert_healthy(&b, "after the re-attach");
}

/// Shallow clones of A, once B has published: A's old exports are not in the clone, so they
/// cannot be the sync point below a gap.
#[test]
fn shallow_clones_of_a_do_not_report_b_s_exports_as_skipped() {
    let fx = shared_fixture();
    let b = fx.monorepo("mono-b", Some("b"), false);
    run_ok(&b.dir, &["attach", "core", &fx.pub_dir, "--import-history"]);
    b.commit("b feature", &[("core/fromb.txt", Some("b\n"))]);
    run_ok(&b.dir, &["push"]);
    run_ok(&fx.a.dir, &["pull"]);
    for i in 1..=4 {
        fx.a.commit(
            &format!("a {i}"),
            &[(&format!("core/a{i}.txt"), Some("a\n"))],
        );
    }
    run_ok(&fx.a.dir, &["push"]);
    run_ok(&b.dir, &["pull"]);
    b.commit("b feature 2", &[("core/fromb2.txt", Some("b\n"))]);
    run_ok(&b.dir, &["push"]);
    run_ok(&fx.a.dir, &["pull"]);
    fx.a.commit("a 5", &[("core/a5.txt", Some("5\n"))]);
    run_ok(&fx.a.dir, &["push"]);

    // Only the guard is under test: a shallow clone holds none of A's import commits, so B's
    // exports read as unimported there (as any outside commit does in a shallow clone).
    for depth in [1, 3] {
        let clone = shallow_clone(&fx, &fx.a, depth, &format!("ad{depth}"));
        let st = run_monosplice(&clone.dir, &["status"]);
        assert_eq!(
            st.exit_code, 0,
            "depth {depth}: {}\n{}",
            st.stdout, st.stderr
        );
        assert!(!st.stderr.contains("never imported"), "{}", st.stderr);
    }
}
