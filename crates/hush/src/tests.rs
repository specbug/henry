use super::*;

fn keyring() -> (Hush, Arc<Canon>) {
    let db = Arc::new(Canon::open_in_memory().unwrap());
    (Hush::new(db.clone(), &Key::generate().unwrap()), db)
}

#[test]
fn round_trips_a_value() {
    let (c, _) = keyring();
    c.set("draw", "SESSION_KEY", &Secret::new("s3cret")).unwrap();
    assert_eq!(c.get("draw", "SESSION_KEY").unwrap().unwrap().expose(), "s3cret");
    assert_eq!(c.get("draw", "OTHER").unwrap(), None);
    assert_eq!(c.names("draw").unwrap(), ["SESSION_KEY"]);
}

#[test]
fn the_record_holds_only_ciphertext() {
    let (c, db) = keyring();
    c.set("draw", "TOKEN", &Secret::new("plain-text-value")).unwrap();
    let blob = db.get_sealed("draw", "TOKEN").unwrap().unwrap();
    assert!(!blob.windows(16).any(|w| w == b"plain-text-value"));
    for e in db.events(None, 100).unwrap() {
        assert!(!e.message.contains("plain-text-value"));
    }
}

#[test]
fn sealing_twice_gives_different_ciphertexts() {
    let (c, db) = keyring();
    c.set("a", "K", &Secret::new("same")).unwrap();
    let first = db.get_sealed("a", "K").unwrap();
    c.set("a", "K", &Secret::new("same")).unwrap();
    assert_ne!(first, db.get_sealed("a", "K").unwrap());
}

#[test]
fn a_value_moved_to_another_slot_will_not_open() {
    let (c, db) = keyring();
    c.set("draw", "TOKEN", &Secret::new("for draw only")).unwrap();
    let blob = db.get_sealed("draw", "TOKEN").unwrap().unwrap();
    db.put_sealed("thief", "TOKEN", &blob).unwrap();
    assert!(matches!(c.get("thief", "TOKEN"), Err(Error::Unseal(_))));
    db.put_sealed("draw", "OTHER", &blob).unwrap();
    assert!(matches!(c.get("draw", "OTHER"), Err(Error::Unseal(_))));
}

#[test]
fn the_wrong_key_opens_nothing() {
    let db = Arc::new(Canon::open_in_memory().unwrap());
    Hush::new(db.clone(), &Key::generate().unwrap()).set("a", "K", &Secret::new("v")).unwrap();
    let other = Hush::new(db, &Key::generate().unwrap());
    assert!(matches!(other.get("a", "K"), Err(Error::Unseal(_))));
}

#[test]
fn tampering_is_detected() {
    let (c, db) = keyring();
    c.set("a", "K", &Secret::new("value")).unwrap();
    let mut blob = db.get_sealed("a", "K").unwrap().unwrap();
    *blob.last_mut().unwrap() ^= 1;
    db.put_sealed("a", "K", &blob).unwrap();
    assert!(matches!(c.get("a", "K"), Err(Error::Unseal(_))));

    db.put_sealed("a", "K", &[9, 9, 9]).unwrap();
    assert!(matches!(c.get("a", "K"), Err(Error::Format(_))));
}

#[test]
fn secrets_never_print() {
    let s = Secret::new("hunter2");
    assert_eq!(format!("{s:?}"), "Secret(***)");
    let (c, _) = keyring();
    c.set("a", "K", &s).unwrap();
    let p = c.provision("a", &["K".into()]).unwrap();
    assert!(!format!("{p:?}").contains("hunter2"));
}

#[test]
fn provisioning_reports_what_is_missing() {
    let (c, _) = keyring();
    c.set("draw", "A", &Secret::new("1")).unwrap();
    let p = c.provision("draw", &["A".into(), "B".into()]).unwrap();
    assert_eq!(p.env.len(), 1);
    assert_eq!(p.env[0].0, "A");
    assert_eq!(p.missing, ["B"]);
}

#[test]
fn names_are_checked() {
    let (c, _) = keyring();
    assert!(matches!(c.set("a", "not a name", &Secret::new("v")), Err(Error::BadName(_))));
}

#[test]
fn host_secrets_are_separate_from_apps() {
    let (c, _) = keyring();
    c.set(HOST, "ANTHROPIC_API_KEY", &Secret::new("k")).unwrap();
    assert!(c.names("draw").unwrap().is_empty());
    assert_eq!(c.names(HOST).unwrap(), ["ANTHROPIC_API_KEY"]);
    assert!(c.remove(HOST, "ANTHROPIC_API_KEY").unwrap());
    assert!(!c.remove(HOST, "ANTHROPIC_API_KEY").unwrap());
}

#[test]
fn a_file_key_is_created_once_and_private() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileKey { path: dir.path().join("master.key") };
    let a = store.load_or_create().unwrap();
    let b = store.load_or_create().unwrap();
    assert_eq!(a.bytes(), b.bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn a_corrupt_key_file_is_an_error_not_a_new_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("master.key");
    std::fs::write(&path, "not hex").unwrap();
    assert!(matches!(FileKey { path }.load_or_create(), Err(Error::Key(_))));
}

#[test]
fn a_key_moves_to_a_new_mac_and_never_over_another() {
    let dir = tempfile::tempdir().unwrap();
    let old = FileKey { path: dir.path().join("old.key") };
    let new = FileKey { path: dir.path().join("new.key") };
    assert!(new.load().unwrap().is_none(), "loading never creates a key");
    let key = old.load_or_create().unwrap();
    let carried = Key::from_hex(&key.to_hex()).unwrap();
    new.import(&carried).unwrap();
    assert_eq!(new.load().unwrap().unwrap().bytes(), key.bytes());
    let other = Key::generate().unwrap();
    assert!(new.import(&other).unwrap_err().to_string().contains("already holds a key"));
}
