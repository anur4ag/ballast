use std::{
    fs,
    process::{Command, Output},
};

#[test]
fn report_offline_json_empty_corrupt_and_observe() {
    let home = std::env::temp_dir().join(format!("ballast-report-cli-{}", std::process::id()));
    fs::create_dir_all(home.join("state")).unwrap();
    let run = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_ballast"))
            .args(args)
            .env("BALLAST_HOME", &home)
            .output()
            .unwrap()
    };
    let empty = run(&["report", "--json"]);
    assert!(empty.status.success());
    let empty: serde_json::Value = serde_json::from_slice(&empty.stdout).unwrap();
    assert_eq!(empty["since_days"], 7);
    assert_eq!(empty["schema_version"], 1);
    assert_eq!(empty["days"], serde_json::json!({}));
    let today = empty["through_day"].as_str().unwrap();
    fs::write(
        home.join("state/stats.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema_version":1,"days":{today:{"observe":{"holds":3,"kills_blocked":2}}}
        }))
        .unwrap(),
    )
    .unwrap();
    let json = run(&["report", "--since", "1d", "--json"]);
    assert!(json.status.success());
    let json: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(json["totals"]["observe"]["holds"], 3);
    assert_eq!(json["totals"]["enforce"]["holds"], 0);
    assert!(
        String::from_utf8(run(&["report"]).stdout)
            .unwrap()
            .contains("would have")
    );
    assert!(!run(&["report", "--since", "2d"]).status.success());
    fs::write(home.join("state/stats.json"), b"{broken").unwrap();
    let corrupt = run(&["report"]);
    assert!(corrupt.status.success());
    assert!(
        String::from_utf8(corrupt.stdout)
            .unwrap()
            .contains("No recorded activity")
    );
    assert!(
        String::from_utf8(corrupt.stderr)
            .unwrap()
            .contains("stats unavailable")
    );
    assert_eq!(fs::read(home.join("state/stats.json")).unwrap(), b"{broken");
    fs::remove_dir_all(home).unwrap();
}
