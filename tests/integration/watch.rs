//! Integration tests for the transcript watcher.

#[cfg(feature = "embedded-aletheiadb")]
#[test]
#[allow(clippy::too_many_lines)]
fn test_watcher_polling_and_incremental_ingestion() {
    use aletheia_egregore::adapters::EmbeddedAletheiaSink;
    use aletheia_egregore::watch::watch;
    use std::fs;
    use std::time::Duration;

    let temp_store = tempfile::tempdir().expect("temp store dir");
    let temp_watch_root = tempfile::tempdir().expect("temp watch root");

    let antigravity_dir = temp_watch_root.path().join("gemini/antigravity/brain");
    fs::create_dir_all(&antigravity_dir).expect("create brain dir");

    // 1. Write initial transcript containing Turn 0
    let conv_dir = antigravity_dir.join("test-conversation-123");
    let logs_dir = conv_dir.join(".system_generated/logs");
    fs::create_dir_all(&logs_dir).expect("create logs dir");
    let transcript_path = logs_dir.join("transcript.jsonl");

    let turn_0_jsonl = concat!(
        "{\"step_index\":0,\"source\":\"user\",\"type\":\"USER_INPUT\",\"status\":\"success\",\"created_at\":\"2026-06-23T16:00:00-05:00\",\"content\":\"hello\",\"thinking\":null,\"tool_calls\":null}\n",
        "{\"step_index\":1,\"source\":\"assistant\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"success\",\"created_at\":\"2026-06-23T16:00:02-05:00\",\"content\":\"hi there\",\"thinking\":\"thought\",\"tool_calls\":null}\n"
    );
    fs::write(&transcript_path, turn_0_jsonl).expect("write initial transcript");

    // 2. Run watch for exactly one iteration (by returning false on callback)
    let ran_first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran_first_clone = ran_first.clone();

    watch(
        temp_store.path(),
        Some(&antigravity_dir),
        None,
        None,
        Duration::from_millis(10),
        false,
        Some(
            &(move || {
                ran_first_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                false // Exit loop after first scan
            }),
        ),
    )
    .expect("watch should run successfully");

    assert!(ran_first.load(std::sync::atomic::Ordering::SeqCst));

    // 3. Verify that Turn 0 records are in the database
    let reopened = EmbeddedAletheiaSink::open(temp_store.path()).expect("open store");

    // Antigravity tag is antigravity-jsonl. We derive stable session ID by hashing path
    let abs_path = fs::canonicalize(&transcript_path).unwrap_or_else(|_| transcript_path.clone());
    let path_str = abs_path.to_string_lossy();
    let path_hash = blake3::hash(path_str.as_bytes()).to_hex().to_string();
    let expected_session_id = aletheia_egregore::ir::agent_memory_stable_id(&[
        "node",
        "agent_session",
        "antigravity-jsonl",
        &path_hash,
    ]);

    let session_node = reopened
        .read_back(&expected_session_id)
        .expect("read session node")
        .expect("session node exists");

    assert_eq!(session_node.id(), &expected_session_id);

    // Let's verify Turn 0 node exists
    let expected_turn_0_id = aletheia_egregore::ir::agent_memory_stable_id(&[
        "node",
        "agent_turn",
        &aletheia_egregore::ir::agent_memory_stable_id(&[
            "node",
            "agent_run",
            &expected_session_id,
            "run-0",
        ]),
        "0",
    ]);
    let turn_0_node = reopened
        .read_back(&expected_turn_0_id)
        .expect("read turn 0 node")
        .expect("turn 0 node exists");

    assert_eq!(turn_0_node.id(), &expected_turn_0_id);

    drop(reopened);

    // 4. Append Turn 1 to the transcript file
    let turn_1_jsonl = concat!(
        "{\"step_index\":2,\"source\":\"user\",\"type\":\"USER_INPUT\",\"status\":\"success\",\"created_at\":\"2026-06-23T16:01:00-05:00\",\"content\":\"run test\",\"thinking\":null,\"tool_calls\":null}\n",
        "{\"step_index\":3,\"source\":\"assistant\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"success\",\"created_at\":\"2026-06-23T16:01:02-05:00\",\"content\":\"ok\",\"thinking\":null,\"tool_calls\":[{\"name\":\"run_command\",\"args\":{\"CommandLine\":\"cargo test\"}}]}\n",
        "{\"step_index\":4,\"source\":\"user\",\"type\":\"COMMAND_RUN\",\"status\":\"success\",\"created_at\":\"2026-06-23T16:01:05-05:00\",\"content\":\"pass\",\"thinking\":null,\"tool_calls\":null}\n"
    );

    // We must sleep briefly to ensure modification time registers as different
    std::thread::sleep(Duration::from_millis(100));
    fs::write(&transcript_path, format!("{turn_0_jsonl}{turn_1_jsonl}")).expect("append turn 1");

    // 5. Run watch again for exactly one iteration
    let ran_second = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran_second_clone = ran_second.clone();

    watch(
        temp_store.path(),
        Some(&antigravity_dir),
        None,
        None,
        Duration::from_millis(10),
        false,
        Some(
            &(move || {
                ran_second_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                false
            }),
        ),
    )
    .expect("watch should run successfully");

    assert!(ran_second.load(std::sync::atomic::Ordering::SeqCst));

    // 6. Verify that Turn 1 is now also in the database with the same session ID
    let reopened2 = EmbeddedAletheiaSink::open(temp_store.path()).expect("reopen store");

    let expected_turn_1_id = aletheia_egregore::ir::agent_memory_stable_id(&[
        "node",
        "agent_turn",
        &aletheia_egregore::ir::agent_memory_stable_id(&[
            "node",
            "agent_run",
            &expected_session_id,
            "run-0",
        ]),
        "1",
    ]);

    let turn_1_node = reopened2
        .read_back(&expected_turn_1_id)
        .expect("read turn 1 node")
        .expect("turn 1 node exists");

    assert_eq!(turn_1_node.id(), &expected_turn_1_id);

    // Turn 0 should still be there, unchanged
    let turn_0_node_check = reopened2
        .read_back(&expected_turn_0_id)
        .expect("read turn 0 node check")
        .expect("turn 0 node still exists");

    assert_eq!(turn_0_node_check.id(), &expected_turn_0_id);
}
