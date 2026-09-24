use super::*;

fn work(label: &str, agent: &str, handle: &str, bytes: Option<u64>, ports: &[u16]) -> Work {
    Work {
        label: label.into(),
        agent: agent.into(),
        handle: handle.into(),
        bytes,
        ports: ports.to_vec(),
    }
}

#[test]
fn label_cleans_collapses_and_truncates() {
    for (input, expected) in [
        ("", "workload"),
        ("   ", "workload"),
        ("npm test", "npm test"),
        ("npm   test\tfoo", "npm test foo"),
        ("npm\u{7}test", "npm test"),
        (
            "1234567890123456789012345678901234567890",
            "1234567890123456789012345678901234567890",
        ),
        (
            "12345678901234567890123456789012345678901",
            "123456789012345678901234567890123456789\u{2026}",
        ),
        (
            "cargo build --release --verbose --all-features --workspace-flag",
            "cargo build --release --verbose\u{2026}",
        ),
    ] {
        assert_eq!(label(input), expected, "input: {input:?}");
    }
}

#[test]
fn label_truncates_by_char_count_not_bytes() {
    // 42 two-byte codepoints with no word boundary: byte-slicing would panic
    // on a non-char boundary, so this only passes if truncation is char-based.
    let input: String = std::iter::repeat_n('\u{e9}', 42).collect();
    let expected: String = std::iter::repeat_n('\u{e9}', 39).collect::<String>() + "\u{2026}";
    assert_eq!(label(&input), expected);
}

#[test]
fn agent_name_maps_known_kinds_and_labels_others() {
    for (kind, expected) in [
        ("claude", "Claude"),
        ("codex", "Codex"),
        ("cursor", "Cursor"),
        ("generic", "agent"),
        ("unknown", "agent"),
        ("agent", "agent"),
        ("gemini", "gemini"),
    ] {
        assert_eq!(agent_name(kind), expected, "kind: {kind}");
    }
}

#[test]
fn paused_names_the_work_and_agent() {
    let w = work("npm test", "Claude", "3f2a", None, &[]);
    assert_eq!(
        paused(&w),
        "Paused `npm test` (Claude) to free memory \u{b7} resumes automatically"
    );
}

#[test]
fn resumed_distinguishes_forced_from_ordinary() {
    let w = work("npm test", "Claude", "3f2a", None, &[]);
    assert_eq!(
        resumed(&w, true),
        "Resumed `npm test` (Claude) after 10 min \u{b7} paused work is never held longer"
    );
    assert_eq!(
        resumed(&w, false),
        "Resumed `npm test` (Claude) \u{b7} protected from pauses for 5 min"
    );
}

#[test]
fn cleanup_reports_a_single_reclaimed_workload_with_memory() {
    let reclaimed = [work("cargo build", "Claude", "", Some(1_288_490_189), &[])];
    assert_eq!(
        cleanup("Claude", true, &reclaimed, &[]),
        "Reclaimed a leftover `cargo build` from an ended Claude session \u{b7} 1.2 GiB"
    );
}

#[test]
fn cleanup_reports_a_single_dev_server_with_stop_hint() {
    let services = [work("npm run dev", "Codex", "3f2a", None, &[3000])];
    assert_eq!(
        cleanup("Codex", true, &[], &services),
        "Dev server `npm run dev` from an ended Codex session still on :3000 \u{b7} ballast stop 3f2a"
    );
}

#[test]
fn cleanup_combines_reclaimed_and_services_in_one_summary() {
    let reclaimed = [work("cargo build", "Claude", "", Some(1_288_490_189), &[])];
    let services = [work("npm run dev", "Claude", "3f2a", None, &[3000])];
    assert_eq!(
        cleanup("Claude", true, &reclaimed, &services),
        "Reclaimed a leftover `cargo build` from an ended Claude session \u{b7} 1.2 GiB \u{b7} \
         Dev server `npm run dev` still on :3000 \u{b7} ballast stop 3f2a"
    );
}

#[test]
fn cleanup_omits_memory_when_any_reclaimed_size_is_unknown() {
    let reclaimed = [
        work("a", "agent", "", Some(1000), &[]),
        work("b", "agent", "", None, &[]),
        work("c", "agent", "", Some(2000), &[]),
    ];
    assert_eq!(
        cleanup("agent", false, &reclaimed, &[]),
        "Reclaimed 3 leftovers (`a`, `b`, 1 more) from an agent session"
    );
}

#[test]
fn cleanup_omits_memory_when_reclaimed_total_is_zero() {
    let reclaimed = [
        work("a", "agent", "", Some(0), &[]),
        work("b", "agent", "", Some(0), &[]),
    ];
    assert_eq!(
        cleanup("agent", false, &reclaimed, &[]),
        "Reclaimed 2 leftovers (`a`, `b`) from an agent session"
    );
}

#[test]
fn cleanup_names_up_to_two_workloads_then_counts_the_rest() {
    let reclaimed: Vec<Work> = (0..5)
        .map(|i| work(&format!("w{i}"), "agent", "", None, &[]))
        .collect();
    assert_eq!(
        cleanup("agent", true, &reclaimed, &[]),
        "Reclaimed 5 leftovers (`w0`, `w1`, 3 more) from an ended agent session"
    );
}

#[test]
fn cleanup_groups_services_ports_deduped_sorted_and_capped() {
    let services = [
        work("svc1", "Claude", "aa", None, &[3000, 3001]),
        work("svc2", "Claude", "bb", None, &[3000, 4000, 5000]),
    ];
    assert_eq!(
        cleanup("Claude", false, &[], &services),
        "2 dev servers (`svc1`, `svc2`) from a Claude session still on :3000, :3001, :4000, \u{2026} \u{b7} ballast ps"
    );
}

#[test]
fn cleanup_falls_back_to_running_when_no_ports_are_known() {
    let services = [work("npm run dev", "Claude", "3f2a", None, &[])];
    assert_eq!(
        cleanup("Claude", false, &[], &services),
        "Dev server `npm run dev` from a Claude session still running \u{b7} ballast stop 3f2a"
    );
}

#[test]
fn cleanup_is_empty_when_nothing_was_reclaimed_or_left_running() {
    assert_eq!(cleanup("Claude", true, &[], &[]), "");
}

#[test]
fn cleanup_picks_an_article_for_the_generic_agent_session() {
    let reclaimed = [work("a", "agent", "", None, &[])];
    assert_eq!(
        cleanup("agent", false, &reclaimed, &[]),
        "Reclaimed a leftover `a` from an agent session"
    );
    let reclaimed = [work("a", "Claude", "", None, &[])];
    assert_eq!(
        cleanup("Claude", false, &reclaimed, &[]),
        "Reclaimed a leftover `a` from a Claude session"
    );
}

#[test]
fn sample_wraps_a_synthetic_paused_notification() {
    assert_eq!(
        sample(),
        "Sample: Paused `npm test` (Claude) to free memory \u{b7} resumes automatically"
    );
}
