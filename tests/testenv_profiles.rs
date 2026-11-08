//! Every `MUMPEER_PROFILE` of the test environment yields a config the relay accepts, with the
//! uses and limits its README row promises.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use mumpeer::config::Config;

fn profile(name: &str) -> Result<Config, String> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("testenv/profile.sh");
    let out = Command::new("bash").arg(script).arg(name).output().unwrap();
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let sections = String::from_utf8(out.stdout).unwrap();
    Ok(Config::parse(&format!("public_host = \"127.0.0.1\"\n{sections}")).unwrap())
}

#[test]
fn every_profile_parses_with_its_uses_and_limits() {
    let all = profile("all").unwrap();
    assert_eq!(
        all.uses.enabled(),
        ["images", "files", "screen_share", "camera", "plugin_data"]
    );
    assert_eq!(
        (all.limits.max_broadcasts, all.limits.max_camera_broadcasts),
        (4, 18)
    );

    let no_camera = profile("no-camera").unwrap();
    assert_eq!(
        no_camera.uses.enabled(),
        ["images", "files", "screen_share", "plugin_data"]
    );
    assert!(no_camera.uses.all(), "pairs without `use` keep working");

    let no_broadcast = profile("no-broadcast").unwrap();
    assert_eq!(
        (
            no_broadcast.limits.max_broadcasts,
            no_broadcast.limits.max_camera_broadcasts
        ),
        (0, 0)
    );

    for (name, off) in [
        ("no-files", "files"),
        ("no-screen", "screen_share"),
        ("no-images", "images"),
    ] {
        let config = profile(name).unwrap();
        assert!(!config.uses.allows(off), "{name}");
        assert!(config.uses.camera, "{name}");
    }
    assert_eq!(profile("plugin-data").unwrap().uses, all.uses);
    let no_plugin_data = profile("no-plugin-data").unwrap();
    assert_eq!(
        no_plugin_data.uses.enabled(),
        ["images", "files", "screen_share", "camera"]
    );
    assert!(no_plugin_data.uses.all());
    let old = profile("old").unwrap();
    assert!(
        !old.uses.plugin_data,
        "a previous release has no plugin data"
    );
    assert_eq!(old.plugin_data, no_plugin_data.plugin_data);
    assert!(
        profile("video")
            .unwrap_err()
            .contains("unknown MUMPEER_PROFILE")
    );
}
