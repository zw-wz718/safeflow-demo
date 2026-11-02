use super::*;
use std::collections::BTreeMap;

fn valid() -> BTreeMap<String, OsString> {
    let values = [
        "http://127.0.0.1:12345/api/audit/outbox/v1",
        "isolated-worker-token",
        "isolated-worker-receiver",
        "250",
        "1",
        "200",
        "2",
        "100",
        "500",
    ];
    let mut map = BTreeMap::new();
    map.insert(ENABLED.into(), "1".into());
    for (key, value) in SETTINGS.iter().zip(values) {
        map.insert((*key).into(), value.into());
    }
    map
}
fn parse(
    map: &BTreeMap<String, OsString>,
) -> Result<Option<AuditWorkerConfig>, AuditWorkerFailure> {
    AuditWorkerConfig::from_values(|k| map.get(k).cloned())
}
#[test]
fn default_off_and_every_residual_setting_rejected() {
    let mut map = BTreeMap::new();
    assert!(parse(&map).unwrap().is_none());
    map.insert(ENABLED.into(), "0".into());
    assert!(parse(&map).unwrap().is_none());
    for switch in [None, Some("0")] {
        for name in SETTINGS {
            let mut map = BTreeMap::new();
            if let Some(s) = switch {
                map.insert(ENABLED.into(), s.into());
            }
            map.insert(name.into(), "".into());
            assert!(matches!(
                parse(&map),
                Err(AuditWorkerFailure::Configuration)
            ));
        }
    }
}
#[test]
fn enabled_requires_every_setting_and_strict_numeric_ranges() {
    assert!(parse(&valid()).unwrap().is_some());
    for name in SETTINGS {
        let mut map = valid();
        map.remove(name);
        assert!(parse(&map).is_err());
    }
    for (name, min, max) in [
        (SETTINGS[3], 100, 60000),
        (SETTINGS[4], 1, 1000),
        (SETTINGS[5], 100, 30000),
        (SETTINGS[6], 1, 5),
        (SETTINGS[7], 100, 30000),
        (SETTINGS[8], 100, 5000),
    ] {
        for value in [min.to_string(), max.to_string()] {
            let mut map = valid();
            map.insert(name.into(), value.into());
            assert!(parse(&map).is_ok());
        }
        for value in [
            (min - 1).to_string(),
            (max + 1).to_string(),
            "+1".into(),
            " 100".into(),
            "100 ".into(),
            "1.0".into(),
            "１２３".into(),
            "18446744073709551616".into(),
            "".into(),
        ] {
            let mut map = valid();
            map.insert(name.into(), value.into());
            assert!(parse(&map).is_err());
        }
    }
    for switch in ["", "true", "yes", "01", " 1"] {
        let mut map = valid();
        map.insert(ENABLED.into(), switch.into());
        assert!(parse(&map).is_err());
    }
}
#[test]
fn sender_validation_and_redacted_errors_preserved() {
    for (name, value) in [
        (SETTINGS[0], "http://example.com/api/audit/outbox/v1"),
        (SETTINGS[0], "http://127.0.0.1:1/other"),
        (
            SETTINGS[0],
            "https://u:secret@example.com/api/audit/outbox/v1",
        ),
        (SETTINGS[1], "secret\n"),
        (SETTINGS[2], "bad receiver\n"),
    ] {
        let mut map = valid();
        map.insert(name.into(), value.into());
        let error = parse(&map).err().unwrap();
        assert_eq!(
            error.to_string(),
            "invalid public audit worker configuration"
        );
        assert_eq!(format!("{error:?}"), "Configuration");
    }
}
#[test]
fn non_unicode_switch_and_each_setting_fail_closed() {
    #[cfg(windows)]
    let bad = {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0xd800])
    };
    #[cfg(unix)]
    let bad = {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![0xff])
    };
    for name in std::iter::once(ENABLED).chain(SETTINGS) {
        let mut map = valid();
        map.insert(name.into(), bad.clone());
        assert!(parse(&map).is_err());
    }
}
