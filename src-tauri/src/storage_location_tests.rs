use super::*;

fn location(root: &Path) -> StorageLocation {
    fs::create_dir_all(root.join("install")).unwrap();
    StorageLocation::new(
        &root.join("config"),
        root.join("default"),
        &root.join("install"),
    )
}

fn populate(store: &mut Store, root: &Path) -> String {
    fs::create_dir_all(root).unwrap();
    let config = new_config(root.to_str().unwrap(), &["*.log".into()]);
    let mut state = crate::diff::test_snapshot(
        &Uuid::new_v4().to_string(),
        vec![FileEntry {
            relative_path: "中文/文件.bin".into(),
            kind: EntryKind::File,
            size: 987654,
            modified_time: Utc::now(),
            md5: "1234567890abcdef1234567890abcdef".into(),
        }],
    );
    state.directory_id = config.id.clone();
    state.root_path = config.root_path.clone();
    state.remark = "迁移前记录".into();
    store.commit_scan(&config, &state, true, 3, true).unwrap();
    let mut config = store.config(&config.id).unwrap();
    state.id = Uuid::new_v4().to_string();
    state.files[0].md5 = "abcdef1234567890abcdef1234567890".into();
    store.commit_scan(&config, &state, false, 2, false).unwrap();
    config = store.config(&config.id).unwrap();
    store
        .update_settings(&config.id, vec!["*.log".into()], vec!["*.bin".into()], 2)
        .unwrap();
    store
        .conn
        .execute("INSERT INTO migrated_json(filename) VALUES('old.json')", [])
        .unwrap();
    config.id
}

#[test]
fn relocation_copies_wal_history_current_metadata_activity_and_reopens() {
    let root = tempfile::tempdir().unwrap();
    let location = location(root.path());
    let mut store = Store::open_configured(location.clone()).unwrap();
    let id = populate(&mut store, &root.path().join("watched"));
    assert!(
        location
            .default_dir
            .join(format!("{DATABASE_NAME}-wal"))
            .metadata()
            .unwrap()
            .len()
            > 0
    );
    let config = store.config(&id).unwrap();
    let before = serde_json::to_value(store.overview(&id).unwrap()).unwrap();
    let activity = serde_json::to_value(store.activity(&id).unwrap()).unwrap();
    let saved = config.baseline_id.unwrap();
    let current = config.current_id.unwrap();
    let entries = serde_json::to_value(store.snapshot(&current).unwrap()).unwrap();
    let comparison = store.compare_states(&saved, &current, true, &[]).unwrap();
    let new_dir = root.path().join("自选 数据");
    fs::create_dir_all(new_dir.join("snapshots")).unwrap();
    fs::write(new_dir.join("snapshots/unrelated.json"), "broken").unwrap();
    let result = store.migrate_to(&new_dir).unwrap();
    assert_eq!(result["source_retained"], false);
    assert!(!location.default_dir.join(DATABASE_NAME).exists());
    assert_eq!(
        serde_json::to_value(store.overview(&id).unwrap()).unwrap(),
        before
    );
    assert_eq!(
        serde_json::to_value(store.snapshot(&current).unwrap()).unwrap(),
        entries
    );
    assert_eq!(
        serde_json::to_value(store.activity(&id).unwrap()).unwrap(),
        activity
    );
    assert_eq!(comparison.summary.counts.modified, 1);
    // The independent comparison stays usable after its source connection closes.
    assert_eq!(comparison.query(&DiffQuery::default()).unwrap().total, 1);
    drop(store);
    let store = Store::open_configured(location.clone()).unwrap();
    assert!(store.warnings.is_empty()); // Custom folders do not trigger legacy migration.
    assert_eq!(store.data_dir, fs::canonicalize(new_dir).unwrap());
    assert_eq!(
        serde_json::to_value(store.overview(&id).unwrap()).unwrap(),
        before
    );
    assert_eq!(
        serde_json::to_value(store.snapshot(&current).unwrap()).unwrap(),
        entries
    );
    assert_eq!(
        store
            .conn
            .query_row::<u32, _, _>("SELECT count(*) FROM migrated_json", [], |r| r.get(0))
            .unwrap(),
        1
    );
    drop(store);
    let mut store = Store::open_configured(location.clone()).unwrap();
    store.migrate_to(&location.default_dir).unwrap();
    drop(store);
    assert_eq!(
        Store::open_configured(location)
            .unwrap()
            .history(&id)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn relocation_rejects_existing_database_sidecars_locks_and_scanned_roots() {
    let root = tempfile::tempdir().unwrap();
    let location = location(root.path());
    let mut store = Store::open_configured(location.clone()).unwrap();
    let watched = root.path().join("watched");
    let id = populate(&mut store, &watched);
    let existing = root.path().join("existing");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join(DATABASE_NAME), b"do not overwrite").unwrap();
    assert!(store
        .migrate_to(&existing)
        .unwrap_err()
        .contains("不会覆盖"));
    assert_eq!(
        fs::read(existing.join(DATABASE_NAME)).unwrap(),
        b"do not overwrite"
    );
    let sidecar = root.path().join("sidecar");
    fs::create_dir(&sidecar).unwrap();
    fs::write(sidecar.join(format!("{DATABASE_NAME}-wal")), b"wal").unwrap();
    assert!(store.migrate_to(&sidecar).is_err());
    assert!(!sidecar.join(DATABASE_NAME).exists());
    let locked = root.path().join("locked");
    let _lock = storage_location::lock_directory(&locked).unwrap();
    assert!(store.migrate_to(&locked).is_err());
    assert!(!locked.join(DATABASE_NAME).exists());
    let inside = watched.join("must-not-create");
    assert!(store.migrate_to(&inside).unwrap_err().contains("扫描目录"));
    assert!(!inside.exists());
    assert!(store.migrate_to(Path::new("relative")).is_err());
    assert!(store
        .ensure_database_outside_root(&location.default_dir)
        .is_err());
    assert!(store.ensure_database_outside_root(&watched).is_ok());
    assert_eq!(store.history(&id).unwrap().len(), 1);
    assert!(location.default_dir.join(DATABASE_NAME).exists());
}

#[test]
fn preference_commit_failure_rolls_back_and_allows_a_later_migration() {
    let root = tempfile::tempdir().unwrap();
    let location = location(root.path());
    let mut store = Store::open_configured(location.clone()).unwrap();
    let id = populate(&mut store, &root.path().join("watched"));
    let original_preference = fs::read(&location.config_file).unwrap();
    let bad_file = root.path().join("config/is-a-directory");
    fs::create_dir(&bad_file).unwrap();
    store.location.as_mut().unwrap().config_file = bad_file;
    let target = root.path().join("failed");
    assert!(store
        .migrate_to(&target)
        .unwrap_err()
        .contains("保存文件失败"));
    assert!(location.default_dir.join(DATABASE_NAME).exists());
    for suffix in ["", "-wal", "-shm", "-journal"] {
        assert!(!target.join(format!("{DATABASE_NAME}{suffix}")).exists());
    }
    assert_eq!(
        fs::read(&location.config_file).unwrap(),
        original_preference
    );
    assert_eq!(store.history(&id).unwrap().len(), 1);
    store.location = Some(location.clone());
    store.migrate_to(&target).unwrap();
    drop(store);
    assert_eq!(
        Store::open_configured(location)
            .unwrap()
            .history(&id)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn installer_unicode_bootstrap_and_saved_location_precedence() {
    let root = tempfile::tempdir().unwrap();
    let location = location(root.path());
    let target = root.path().join("数据库 位置");
    let ini = format!(
        "[storage]\r\nmode=custom\r\ndirectory={}\r\n",
        target.display()
    );
    let bytes: Vec<u8> = std::iter::once(0xfeff_u16)
        .chain(ini.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect();
    let bootstrap = root.path().join("install/snapdiff-storage.ini");
    fs::write(&bootstrap, bytes).unwrap();
    let store = Store::open_configured(location.clone()).unwrap();
    assert_eq!(store.data_dir, fs::canonicalize(&target).unwrap());
    assert_eq!(store.storage_info()["mode"], "custom");
    drop(store);
    fs::write(bootstrap, "[storage]\nmode=install\n").unwrap();
    let mut store = Store::open_configured(location.clone()).unwrap();
    assert_eq!(store.data_dir, fs::canonicalize(target).unwrap());
    store.migrate_to(&location.install_dir).unwrap();
    assert_eq!(store.storage_info()["mode"], "install");
}

#[test]
fn failed_installer_selection_keeps_default_and_missing_configured_database_is_not_recreated() {
    let root = tempfile::tempdir().unwrap();
    let location = location(root.path());
    fs::create_dir_all(&location.install_dir).unwrap();
    fs::write(location.install_dir.join(DATABASE_NAME), "existing").unwrap();
    fs::write(
        root.path().join("install/snapdiff-storage.ini"),
        "[storage]\nmode=install\n",
    )
    .unwrap();
    let store = Store::open_configured(location.clone()).unwrap();
    assert_eq!(
        store.data_dir,
        fs::canonicalize(&location.default_dir).unwrap()
    );
    assert_eq!(store.warnings.len(), 1);
    assert!(Store::open_configured(location.clone()).is_err()); // Same user config is locked.
    drop(store);
    fs::remove_file(location.default_dir.join(DATABASE_NAME)).unwrap();
    assert!(Store::open_configured(location.clone()).is_err());
    assert!(!location.default_dir.join(DATABASE_NAME).exists());
}
