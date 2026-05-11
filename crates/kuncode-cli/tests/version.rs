use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn version_flag_prints_package_version() {
    Command::cargo_bin("kuncode")
        .expect("locate kuncode binary")
        .arg("--version")
        .assert()
        .success()
        .stdout(contains(env!("CARGO_PKG_VERSION")));
}
