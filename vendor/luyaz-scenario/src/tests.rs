//! The engine on a fake device: scans, the SIM, the Wi-Fi switch, and both clocks are
//! the test's, files live under a temporary root, and every wait advances the fake clock instead
//! of sleeping. The decisions themselves are u60-policy's and tested there.

use super::*;
use crate::testing::*;

/// The device's own three scenarios (home by BSSID, away, abroad with CHILL DIRECT).
fn config() -> Config {
    let mut cfg = sc::template();
    if let Some(home) = cfg.scenarios.iter_mut().find(|s| s.id == "home") {
        home.detect = sc::Detect::Ssid {
            entries: vec![sc::SsidEntry { ssid: "Home".into(), bssid: Some("02:00:00:00:00:01".into()) }],
        };
    }
    cfg
}

fn home_net() -> Network {
    net("Home", "02:00:00:00:00:01", -40.0)
}

/// Step the fake clock past the scan interval and tick once.
fn tick_after(f: &Fake, e: &Engine, secs: i64) {
    f.d().now += secs;
    e.tick();
}

fn write_config(f: &Fake, cfg: &Config) {
    std::fs::create_dir_all(f.file(DIR)).unwrap();
    f.write_config(cfg);
}

#[test]
fn a_trip_home_away_abroad_and_back() {
    let f = Fake::new("trip");
    write_config(&f, &config());
    f.d().gets.insert(sc::CHILL_STATUS.into(), json!({"ok": true, "data": {"region": {"active": "JP"}}}));
    let e = Engine::new(f.clone());

    // boot: Wi-Fi up, nothing to repair, state cleared
    e.bootsafe();
    assert!(f.log().contains("[bootsafe] Wi-Fi already up"));
    assert_eq!(e.run_state().current, "");
    f.calls();

    // home seen twice: the first scan is a candidate, the second switches (fresh boot: no exit
    // threshold), Wi-Fi off and sleep held first
    f.scan_returns(vec![home_net()]);
    tick_after(&f, &e, 60);
    assert_eq!(e.run_state().candidate, "home");
    tick_after(&f, &e, 60);
    assert_eq!(e.run_state().current, "home");
    let calls = f.calls();
    assert!(calls.contains(&"auto-sleep false".to_string()), "{calls:?}");
    assert!(calls.contains(&"wifi false false".to_string()), "{calls:?}");
    assert_eq!(fs::read_to_string(f.file(RTC_WAKEALARM)).unwrap(), (1000 + 60 + 30).to_string());
    let st: RunState = read_json(&f.file(STATE_FILE));
    assert_eq!(st.current, "home");
    assert_eq!(st.applied.get("wireless.ap0.disabled").map(String::as_str), Some(""));
    let recs = f.records();
    assert_eq!(recs.iter().filter(|r| r["kind"] == "scan").count(), 2);
    let applied = recs.iter().find(|r| r["kind"] == "applied").unwrap();
    assert_eq!(applied["to"], "home");
    assert_eq!(applied["ok"], true);
    assert_eq!(recs.last().unwrap()["seq"], 3);

    // not due yet: only the sleep defences are refreshed
    tick_after(&f, &e, 10);
    assert_eq!(f.calls(), vec!["auto-sleep false"]);

    // home gone (past the blip hold since it was last seen on entering): a recheck before
    // believing it, then two misses to leave
    f.scan_returns(vec![net("Cafe", "02:00:00:00:00:09", -50.0)]);
    tick_after(&f, &e, 300);
    let calls = f.calls();
    assert_eq!(calls.iter().filter(|c| *c == "scan").count(), 2, "{calls:?}");
    assert!(f.log().contains("\"home\" not seen: scans saw 1+1 networks; Home 02:00:00:00:00:01 absent"));
    let rec = f.records().pop().unwrap();
    assert_eq!(rec["rechecked"], true);
    assert_eq!(rec["first"], 1);
    assert_eq!(rec["nets"].as_array().unwrap().len(), 2);
    assert_eq!(rec["detected"], "away");
    tick_after(&f, &e, 60);
    assert_eq!(e.run_state().current, "away");
    let calls = f.calls();
    assert!(calls.contains(&"wifi true true".to_string()), "{calls:?}");
    assert_eq!(f.d().auto_sleep, Some(true));

    // a foreign SIM: abroad. Wi-Fi is already fully on (not torn down again); CHILL goes DIRECT
    // best effort, verified, and what it was (JP) is saved to come back
    f.d().mcc = Some("440".into());
    tick_after(&f, &e, 300);
    tick_after(&f, &e, 300);
    assert_eq!(e.run_state().current, "abroad");
    let calls = f.calls();
    assert!(!calls.iter().any(|c| c.starts_with("wifi ")), "{calls:?}");
    assert!(calls.iter().any(|c| c.contains("PUT /api/services/chill/regions") && c.contains("DIRECT")), "{calls:?}");
    let pending: Vec<PendingRestore> = read_json(&f.file(RESTORE_FILE));
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].action.action.body.as_ref().map(Value::from).unwrap()["member"], "JP");
    let flag = fs::read_to_string(f.file(ABROAD_FILE)).ok();
    assert!(flag.is_some(), "the abroad flag file exists");

    // home again (local SIM back, home Wi-Fi): leaving abroad puts JP back before home applies
    f.d().mcc = Some("460".into());
    f.scan_returns(vec![home_net()]);
    tick_after(&f, &e, 300);
    tick_after(&f, &e, 300);
    assert_eq!(e.run_state().current, "home");
    let calls = f.calls();
    let restore = calls.iter().position(|c| c.contains("\"member\":\"JP\"") && c.ends_with("(restore)")).expect("restore ran");
    let off = calls.iter().position(|c| c == "wifi false false").expect("Wi-Fi off");
    assert!(restore < off, "{calls:?}");
    assert!(read_json::<Vec<PendingRestore>>(&f.file(RESTORE_FILE)).is_empty());
    assert!(!Path::new(&f.file("/data/alerts/abroad")).exists() || fs::read_to_string(f.file("/data/alerts/abroad")).is_err());
}

#[test]
fn a_failed_apply_rolls_back_and_lands_on_away() {
    let f = Fake::new("rollback");
    write_config(&f, &config());
    let e = Engine::new(f.clone());
    f.scan_returns(vec![home_net()]);
    f.d().wifi_fails = true;
    tick_after(&f, &e, 60);
    tick_after(&f, &e, 60);
    let st = e.run_state();
    assert_eq!(st.current, "away");
    assert!(st.last_error.as_deref().unwrap_or("").contains("not observable"));
    let calls = f.calls();
    assert!(calls.iter().any(|c| c.starts_with("put-back ")), "{calls:?}");
    assert!(calls.contains(&"wifi-locked true true".to_string()), "{calls:?}");
    let applied = f.records().into_iter().find(|r| r["kind"] == "applied").unwrap();
    assert_eq!(applied["ok"], false);
    assert_eq!(applied["after"]["current"], "away");
}

#[test]
fn scans_that_keep_failing_with_wifi_off_fail_open() {
    let f = Fake::new("failopen");
    write_config(&f, &config());
    let e = Engine::new(f.clone());
    f.scan_returns(vec![home_net()]);
    tick_after(&f, &e, 60);
    tick_after(&f, &e, 60);
    assert_eq!(e.run_state().current, "home");
    f.records();
    f.calls();
    {
        let mut d = f.d();
        d.scans.clear();
        d.last_scan = Some(Err("no wiphy present".into()));
    }
    tick_after(&f, &e, 60);
    tick_after(&f, &e, 60);
    assert_eq!(e.run_state().current, "home");
    assert_eq!(e.run_state().scan_failures, 2);
    let rec = f.records().pop().unwrap();
    assert_eq!(rec["kind"], "scan_failed");
    assert_eq!(rec["beaconing"], Value::Null, "only looked at from the third failure");
    tick_after(&f, &e, 60);
    assert_eq!(e.run_state().current, "away");
    let recs = f.records();
    assert_eq!(recs[0]["beaconing"], false);
    assert_eq!(recs[0]["switch_to"], "away");
    assert!(f.calls().contains(&"wifi true true".to_string()));
    assert!(f.log().contains("failing open to the away scenario"));
}

#[test]
fn failed_fail_open_retries_the_actual_ap_even_after_state_became_away() {
    for (manual_off, scan_recovers) in [(false, false), (true, false), (false, true)] {
        let f = Fake::new("failopen-retry");
        write_config(&f, &config());
        f.d().uci.insert("wireless.ap0.disabled".into(), "0".into());
        let e = Engine::new(f.clone());
        f.scan_returns(vec![home_net()]);
        tick_after(&f, &e, 60);
        tick_after(&f, &e, 60);
        assert_eq!(e.run_state().current, "home");
        {
            let mut d = f.d();
            d.last_scan = Some(Err("wireless firmware unavailable".into()));
            d.wifi_fails = true;
        }
        for _ in 0..3 {
            tick_after(&f, &e, 60);
        }
        assert_eq!(e.run_state().current, "away");
        assert!(!f.beaconing(), "both the apply and rollback failed to restore the AP");
        assert_eq!(e.run_state().applied.get("wireless.ap0.disabled").map(String::as_str), Some("0"));
        {
            let mut d = f.d();
            d.wifi_fails = false;
            if manual_off {
                d.uci.insert("wireless.ap0.disabled".into(), "1".into());
            }
        }
        if scan_recovers {
            f.scan_returns(vec![net("Cafe", "02:00:00:00:00:09", -50.0)]);
        }
        tick_after(&f, &e, 300);
        assert_eq!(f.beaconing(), !manual_off, "repair retries our failed write but preserves a later operator override");
    }
}

#[test]
fn a_guard_takeover_returns_to_away_and_is_released_after_ten_minutes() {
    let f = Fake::new("takeover");
    write_config(&f, &config());
    let e = Engine::new(f.clone());
    f.scan_returns(vec![home_net()]);
    tick_after(&f, &e, 60);
    tick_after(&f, &e, 60);
    assert_eq!(e.run_state().current, "home");
    let _ = fs::create_dir_all(std::path::Path::new(&f.file(TAKEOVER_FILE)).parent().unwrap());
    fs::write(f.file(TAKEOVER_FILE), "1234\n").unwrap();
    f.records();
    e.tick();
    assert_eq!(e.run_state().current, "away");
    let recs = f.records();
    assert_eq!(recs[0]["kind"], "takeover");
    assert_eq!(recs[0]["switch_to"], "away");
    // held: no detection, though home is in sight
    f.d().mono += 300.0;
    tick_after(&f, &e, 120);
    assert_eq!(e.run_state().current, "away");
    assert!(e.takeover_marked());
    // released after the hold: the marker goes and detection resumes
    f.d().mono += 301.0;
    tick_after(&f, &e, 120);
    assert!(!e.takeover_marked());
    assert!(f.log().contains("takeover released"));
}

#[test]
fn guard_wifi_write_is_not_a_manual_override_after_handback() {
    let f = Fake::new("guard-handback");
    write_config(&f, &config());
    // The guard has turned Wi-Fi on while the persisted home state still says off.
    f.d().uci.insert("wireless.ap0.disabled".into(), "0".into());
    let prev = RunState {
        current: "home".into(),
        applied: [("wireless.ap0.disabled".into(), "1".into())].into(),
        ..RunState::default()
    };
    fs::write(f.file(STATE_FILE), serde_json::to_string(&prev).unwrap()).unwrap();
    fs::write(f.file(TAKEOVER_FILE), "1234\n").unwrap();
    let e = Engine::new(f.clone());

    e.tick();
    assert_eq!(e.run_state().current, "away");
    assert_eq!(e.run_state().applied.get("wireless.ap0.disabled").map(String::as_str), Some("0"));
    fs::remove_file(f.file(TAKEOVER_FILE)).unwrap();
    f.scan_returns(vec![home_net()]);
    tick_after(&f, &e, 60);
    tick_after(&f, &e, 60);
    assert_eq!(e.run_state().current, "home");
    assert!(!f.beaconing(), "detection must control Wi-Fi again after guard handback");
}

#[test]
fn a_pin_switches_and_retries_restores_without_scanning() {
    let f = Fake::new("pin");
    write_config(&f, &config());
    let e = Engine::new(f.clone());
    std::fs::write(e.io().path(PIN_FILE), "home").unwrap();
    assert_eq!(e.pin().as_deref(), Some("home"));
    e.tick();
    assert_eq!(e.run_state().current, "home");
    let calls = f.calls();
    assert!(!calls.contains(&"scan".to_string()), "{calls:?}");
    let rec = &f.records()[0];
    assert_eq!(rec["kind"], "pin");
    assert_eq!(rec["switch_to"], "home");
    std::fs::remove_file(e.io().path(PIN_FILE)).unwrap();
    assert!(e.pin().is_none());
}

#[test]
fn disabling_restores_away_and_puts_everything_back() {
    let f = Fake::new("disable");
    write_config(&f, &config());
    f.d().gets.insert(sc::CHILL_STATUS.into(), json!({"ok": true, "data": {"region": {"active": "JP"}}}));
    f.d().mcc = Some("440".into());
    let e = Engine::new(f.clone());
    tick_after(&f, &e, 60);
    tick_after(&f, &e, 60);
    assert_eq!(e.run_state().current, "abroad");
    // Wi-Fi was turned off by hand meanwhile: bootsafe has to repair it
    f.d().uci.insert(luyaz_wifi::AP_2G.into(), "1".into());
    f.d().uci.insert(luyaz_wifi::AP_5G.into(), "1".into());
    f.calls();
    e.set_enabled(false);
    assert_eq!(e.run_state().current, "");
    let calls = f.calls();
    assert!(calls.contains(&"wifi true true".to_string()), "{calls:?}");
    assert!(calls.iter().any(|c| c.contains("\"member\":\"JP\"") && c.ends_with("(restore)")), "{calls:?}");
    // disabled: ticks do nothing but retry restores
    tick_after(&f, &e, 600);
    assert!(!f.calls().contains(&"scan".to_string()));
    e.set_enabled(true);
    assert!(e.enabled());
}

#[test]
fn config_validation_and_persistence() {
    let f = Fake::new("config");
    let e = Engine::new(f.clone());
    assert_eq!(e.config().scenarios, Vec::<Scenario>::new());
    let mut cfg = config();
    cfg.scenarios[0].actions.push(ScenarioAction {
        action: sc::Action { method: "GET".into(), path: "/api/device/reboot".into(), body: None },
        snapshot: vec![],
        verify_job: None,
        verify_field: None,
        best_effort: false,
        restore_on_exit: None,
    });
    assert!(e.put_config(&cfg).unwrap_err().contains("not allowed"));
    assert_eq!(e.config(), Config::default());
    assert!(e.put_config(&config()).is_ok());
    assert_eq!(e.config(), config());
    let on_disk: Config = read_json(&f.file(CONFIG_FILE));
    assert_eq!(on_disk, config());
    assert!(f.log().contains("configuration replaced"));
}

#[test]
fn boot_access_holds_wifi_before_600_and_preserves_the_disable_flag() {
    for (mono, disabled) in [(499.0, false), (500.0, false), (499.0, true), (500.0, true)] {
        let f = Fake::new(&format!("boot-access-{mono}-{disabled}"));
        write_config(&f, &config());
        f.d().mono = mono;
        f.d().stop_at_uptime = Some(610);
        f.d().uci.insert(luyaz_wifi::AP_2G.into(), "1".into());
        f.d().uci.insert(luyaz_wifi::AP_5G.into(), "1".into());
        let prev = RunState {
            current: "home".into(),
            applied: [("wireless.ap0.disabled".into(), "1".into())].into(),
            ..RunState::default()
        };
        fs::write(f.file(STATE_FILE), serde_json::to_string(&prev).unwrap()).unwrap();
        if disabled {
            fs::write(f.file(DISABLED_FLAG), "").unwrap();
        }
        let e = Engine::new(f.clone());

        e.run(Duration::from_secs(600));

        assert_eq!(f.wifi_fully_on(), mono < 500.0 || disabled, "uptime={}, disabled={disabled}", mono + 100.0);
        assert_eq!(e.enabled(), !disabled, "the access window must preserve the operator's flag");
        if mono < 500.0 {
            assert_ne!(e.run_state().current, "home", "discard ownership of Wi-Fi forced on during boot");
        } else if !disabled {
            assert_eq!(e.run_state().current, "home", "a service restart after the window keeps home");
        }
    }
}

#[test]
fn home_detection_resumes_after_the_boot_access_window() {
    let f = Fake::new("boot-detection-resumes");
    write_config(&f, &config());
    f.d().mono = 499.0; // uptime 599
    f.d().stop_at_uptime = Some(800);
    f.d().uci.insert(luyaz_wifi::AP_2G.into(), "1".into());
    f.d().uci.insert(luyaz_wifi::AP_5G.into(), "1".into());
    let prev = RunState {
        current: "home".into(),
        applied: [("wireless.ap0.disabled".into(), "1".into())].into(),
        ..RunState::default()
    };
    fs::write(f.file(STATE_FILE), serde_json::to_string(&prev).unwrap()).unwrap();
    f.scan_returns(vec![home_net()]);
    let window_end = f.d().now + 1;
    let e = Engine::new(f.clone());

    e.run(Duration::from_secs(600));

    assert_eq!(e.run_state().current, "home");
    assert!(!f.beaconing());
    let records = f.records();
    let applied = records.iter().find(|r| r["kind"] == "applied" && r["to"] == "home")
        .expect("home must be freshly detected after the boot access window");
    assert!(applied["at"].as_i64().unwrap() >= window_end);
}

#[test]
fn interrupted_boot_access_does_not_leave_stale_home_ownership() {
    let f = Fake::new("boot-access-interrupted");
    write_config(&f, &config());
    f.d().stop_at_uptime = Some(130);
    f.d().uci.insert(luyaz_wifi::AP_2G.into(), "1".into());
    f.d().uci.insert(luyaz_wifi::AP_5G.into(), "1".into());
    let prev = RunState {
        current: "home".into(),
        applied: [("wireless.ap0.disabled".into(), "1".into())].into(),
        ..RunState::default()
    };
    fs::write(f.file(STATE_FILE), serde_json::to_string(&prev).unwrap()).unwrap();
    let e = Engine::new(f.clone());

    e.run(Duration::from_secs(600));

    assert!(f.wifi_fully_on());
    let saved: RunState = read_json(&f.file(STATE_FILE));
    assert_ne!(saved.current, "home", "a crash or handback cannot resurrect pre-boot ownership");
    f.d().mono = 500.0; // restart at uptime 600
    f.d().stop_at_uptime = Some(610);
    let restarted = Engine::new(f.clone());
    restarted.run(Duration::from_secs(600));
    assert!(f.wifi_fully_on());
    assert_ne!(restarted.run_state().current, "home");
}

#[test]
fn the_run_loop_beats_and_stops_once_taken_back() {
    struct Stopping(Arc<Fake>, std::sync::atomic::AtomicU32);
    impl luyaz_wifi::Host for Stopping {
        fn uci_get(&self, key: &str) -> Option<String> {
            luyaz_wifi::Host::uci_get(&*self.0, key)
        }
        fn write_aps(&self, _: bool, _: bool) -> Result<Option<String>, String> {
            unreachable!()
        }
        fn vendor_on(&self) -> Result<(), String> {
            unreachable!()
        }
        fn reload(&self) -> Result<Value, String> {
            unreachable!()
        }
    }
    impl Io for Stopping {
        fn path(&self, p: &str) -> String {
            self.0.path(p)
        }
        fn sleep(&self, d: Duration) {
            self.0.sleep(d)
        }
        fn active(&self) -> bool {
            self.1.fetch_add(1, Ordering::SeqCst) < 3
        }
        fn uptime_secs(&self) -> Option<u64> {
            self.0.uptime_secs()
        }
        fn wifi(&self) -> &dyn luyaz_wifi::Host {
            self
        }
        fn beaconing(&self) -> bool {
            true
        }
        fn wifi_put_back(&self, _: serde_json::Map<String, Value>) {}
        fn exec(&self, _: &str, _: &str, _: &[u8]) -> Result<(), String> {
            Ok(())
        }
        fn get(&self, _: &str) -> (u16, Value) {
            (404, Value::Null)
        }
        fn set_auto_sleep(&self, _: bool) {}
        fn charging(&self) -> Option<bool> {
            None
        }
        fn sim_mcc(&self) -> Option<String> {
            None
        }
    }
    let f = Fake::new("run");
    let s = Arc::new(Stopping(f.clone(), std::sync::atomic::AtomicU32::new(0)));
    let e = Engine::new(s.clone());
    e.run(Duration::ZERO);
    assert_eq!(s.1.load(Ordering::SeqCst), 4, "three ticks, then the fourth look ends it");
    let hb = fs::read_to_string(f.file(HEARTBEAT_FILE)).unwrap();
    assert!(hb.trim().parse::<u64>().is_ok(), "{hb:?}");
}

#[test]
fn tick_file_keeps_the_last_eight_records_newest_last() {
    let mut ring = TickRing::default();
    let mut text = String::new();
    for i in 1..=10 {
        text = ring.push(json!({"kind": "scan", "i": i}), 100 + i);
    }
    let v: Value = serde_json::from_str(&text).unwrap();
    let a = v.as_array().unwrap();
    assert_eq!(a.len(), 8);
    assert_eq!(a[0]["seq"], 3);
    assert_eq!(a[7]["seq"], 10);
    assert_eq!(a[7]["at"], 110);
}

#[test]
fn uptime_is_whole_seconds_of_the_first_field() {
    assert_eq!(uptime_secs("12345.67 54321.00\n"), Some(12345));
    assert_eq!(uptime_secs("7 1"), Some(7));
    assert_eq!(uptime_secs(""), None);
    assert_eq!(uptime_secs("x.5 1"), None);
}

#[test]
fn chill_switched_off_abroad_comes_back_when_home() {
    let f = Fake::new("chill-hooks");
    write_config(&f, &config());
    f.d().gets.insert(sc::CHILL_STATUS.into(), json!({"ok": true, "data": {"region": {"active": "JP"}}}));
    let e = Engine::new(f.clone());
    // not abroad: nothing is recorded
    e.chill_toggled(false, true);
    assert!(read_json::<Vec<PendingRestore>>(&f.file(RESTORE_FILE)).is_empty());
    f.d().mcc = Some("440".into());
    tick_after(&f, &e, 60);
    tick_after(&f, &e, 60);
    assert_eq!(e.run_state().current, "abroad");
    e.chill_toggled(false, true);
    e.chill_exit_changed();
    let keys: Vec<String> = read_json::<Vec<PendingRestore>>(&f.file(RESTORE_FILE)).into_iter().map(|p| p.key).collect();
    assert!(keys.contains(&sc::CHILL_ON_KEY.to_string()) && keys.contains(&sc::CHILL_EXIT_KEY.to_string()), "{keys:?}");
    // switched on by hand: no longer switched on when home
    e.chill_toggled(true, false);
    let keys: Vec<String> = read_json::<Vec<PendingRestore>>(&f.file(RESTORE_FILE)).into_iter().map(|p| p.key).collect();
    assert!(!keys.contains(&sc::CHILL_ON_KEY.to_string()));
    // a restore's own "on" does not count as by hand
    e.chill_toggled(false, true);
    set_in_restore(true);
    e.chill_toggled(true, false);
    set_in_restore(false);
    let keys: Vec<String> = read_json::<Vec<PendingRestore>>(&f.file(RESTORE_FILE)).into_iter().map(|p| p.key).collect();
    assert!(keys.contains(&sc::CHILL_ON_KEY.to_string()));
}

#[test]
fn chill_back_on_is_waited_for_before_the_main_group() {
    let f = Fake::new("chill-wait");
    write_config(&f, &config());
    f.d().gets.insert(sc::CHILL_STATUS.into(), json!({"ok": true, "data": {"region": {"active": "JP"}}}));
    f.d().gets.insert("/api/services/chill/job".into(), json!({"ok": true, "data": {"status": "done"}}));
    f.d().mcc = Some("440".into());
    let e = Engine::new(f.clone());
    tick_after(&f, &e, 60);
    tick_after(&f, &e, 60);
    e.chill_toggled(false, true);
    // mihomo answers again only some seconds after CHILL is back on
    f.d().exec_fails.push("/api/services/chill/regions".into());
    f.d().mcc = Some("460".into());
    f.calls();
    let t0 = f.d().mono;
    // the regions restore keeps failing; it is retried for 20 s, then left for later
    tick_after(&f, &e, 300);
    tick_after(&f, &e, 300);
    let calls = f.calls();
    let on = calls.iter().position(|c| c.contains("/api/services/chill/enable")).expect("CHILL on");
    let regions = calls.iter().filter(|c| c.contains("/api/services/chill/regions")).count();
    assert!(on < calls.iter().position(|c| c.contains("/api/services/chill/regions")).unwrap());
    assert!(regions > 3, "retried while waiting for mihomo: {calls:?}");
    assert!(f.d().mono - t0 >= 20.0);
    let keys: Vec<String> = read_json::<Vec<PendingRestore>>(&f.file(RESTORE_FILE)).into_iter().map(|p| p.key).collect();
    assert!(!keys.contains(&sc::CHILL_ON_KEY.to_string()));
    assert_eq!(keys.len(), 1, "the main group waits for the next scan: {keys:?}");
}

/// A core restart (up past RESTART_MIN_UPTIME) at home, APs off by the home scenario: bootsafe
/// keeps the state and touches nothing. The same at boot (up < 300 s) still repairs.
#[test]
fn bootsafe_keeps_home_off_after_a_core_restart_not_after_a_boot() {
    for (mono, kept) in [(600.0, true), (0.0, false)] {
        let f = Fake::new(if kept { "restart-home" } else { "boot-home" });
        write_config(&f, &config());
        f.d().mono = mono;
        f.d().uci.insert(luyaz_wifi::AP_2G.into(), "1".into());
        f.d().uci.insert(luyaz_wifi::AP_5G.into(), "1".into());
        let prev = json!({"current": "home", "candidate": "", "hits": 0, "misses": 0,
            "applied": {luyaz_wifi::AP_2G: "1", luyaz_wifi::AP_5G: "1"},
            "last_switch": 1, "last_error": null, "last_scan": 1, "scan_failures": 0});
        fs::write(f.file(STATE_FILE), prev.to_string()).unwrap();
        let e = Engine::new(f.clone());
        f.calls();
        e.bootsafe();
        let calls = f.calls();
        if kept {
            assert!(f.log().contains("[bootsafe] Wi-Fi off by scenario \"home\""), "{}", f.log());
            assert_eq!(e.run_state().current, "home");
            assert!(!calls.iter().any(|c| c.starts_with("wifi ")), "{calls:?}");
        } else {
            assert!(f.log().contains("[bootsafe] Wi-Fi was down"), "{}", f.log());
            assert!(calls.iter().any(|c| c.starts_with("wifi ")), "{calls:?}");
        }
    }
}

#[test]
fn disabled_flag_restores_wifi_even_when_home_would_be_kept_after_restart() {
    let f = Fake::new("disabled-held-home");
    write_config(&f, &config());
    f.d().mono = 600.0;
    f.d().uci.insert(luyaz_wifi::AP_2G.into(), "1".into());
    f.d().uci.insert(luyaz_wifi::AP_5G.into(), "1".into());
    let prev = RunState {
        current: "home".into(),
        applied: [("wireless.ap0.disabled".into(), "1".into())].into(),
        last_scan: 1,
        last_switch: Some(1),
        ..RunState::default()
    };
    fs::write(f.file(STATE_FILE), serde_json::to_string(&prev).unwrap()).unwrap();
    let e = Engine::new(f.clone());
    fs::write(f.file(DISABLED_FLAG), "").unwrap();

    e.tick();

    assert!(f.wifi_fully_on(), "disabled must restore the AP, not retain home");
    assert_ne!(e.run_state().current, "home");
    assert!(!f.calls().iter().any(|call| call == "scan"));
}

/// 10-20: a scenarios.json copied in by hand sat unread until the core restarted. Now the next
/// tick takes it; a broken or missing file keeps the loaded config; the web's own write is not
/// mistaken for a hand edit.
#[test]
fn a_hand_edited_config_is_picked_up() {
    let f = Fake::new("reload");
    let e = Engine::new(f.clone());
    assert!(e.config().scenarios.is_empty());

    write_config(&f, &config());
    e.tick();
    assert_eq!(e.config(), config());
    assert!(f.log().contains("configuration file changed; reloaded"));

    // the same file again: nothing said
    e.tick();
    assert_eq!(f.log().matches("reloaded").count(), 1);

    // broken: kept, said once
    fs::write(f.file(CONFIG_FILE), "{ not json").unwrap();
    e.tick();
    e.tick();
    assert_eq!(e.config(), config());
    assert_eq!(f.log().matches("does not read").count(), 1);

    // gone: kept
    fs::remove_file(f.file(CONFIG_FILE)).unwrap();
    e.tick();
    assert_eq!(e.config(), config());
    assert!(f.log().contains("configuration file is gone; keeping the one loaded"));

    // the web's PUT writes the file itself: not a hand edit afterwards
    let mut c2 = config();
    c2.scenarios.retain(|s| s.id != "abroad");
    e.put_config(&c2).unwrap();
    e.tick();
    assert_eq!(e.config(), c2);
    assert_eq!(f.log().matches("reloaded").count(), 1);
}

#[test]
fn leaving_follows_the_signal_and_says_why() {
    let f = Fake::new("leave-signal");
    write_config(&f, &config());
    let e = Engine::new(f.clone());
    e.bootsafe();
    f.scan_returns(vec![home_net()]);
    tick_after(&f, &e, 60);
    tick_after(&f, &e, 60);
    tick_after(&f, &e, 60);
    assert_eq!(e.run_state().current, "home");
    assert_eq!(e.run_state().last_seen_dbm, Some(-40.0));

    // the router blips: held (counted, no switch)
    f.scan_returns(vec![]);
    tick_after(&f, &e, 60);
    assert!(f.log().contains("\"home\" not seen, held as a blip: last seen -40 dBm 1m"), "{}", f.log());
    assert_eq!((e.run_state().current.as_str(), e.run_state().misses), ("home", 1));

    // back strong: the count starts over
    f.scan_returns(vec![home_net()]);
    tick_after(&f, &e, 20);
    assert_eq!((e.run_state().current.as_str(), e.run_state().misses), ("home", 0));

    // fading: two weak scans leave
    f.scan_returns(vec![net("Home", "02:00:00:00:00:01", -75.0)]);
    tick_after(&f, &e, 60);
    assert!(f.log().contains("\"home\" weak: best -75 dBm (leave below -72)"), "{}", f.log());
    assert_eq!(e.run_state().misses, 1);
    tick_after(&f, &e, 20);
    assert_eq!(e.run_state().current, "away");
}
