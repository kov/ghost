//! Font fallback when fontconfig's best match is a font that lacks the character.
//!
//! A user font cache written by a newer fontconfig (Chrome's bundled one) can leave
//! the system fontconfig reading entries with no charset. An entry with nothing to
//! compare is never penalised, so it wins *every* match — `fc-match emoji` answers
//! with a Montserrat web font — and each emoji and symbol fell back to a face that
//! has none of them. Fallback must use a face that actually has the character.
//!
//! The broken entry is reproduced with a scan-time rule that strips Fira Code's
//! charset, in a private font config. fontconfig reads `FONTCONFIG_FILE` once, at
//! its first init, so this file keeps to a single test — it is its own process.

#![cfg(target_os = "linux")]

use ghost_shaper::Fallback;
use ghost_ui::font::SystemFallback;
use std::path::Path;

#[test]
fn fallback_skips_a_best_match_that_lacks_the_character() {
    let dir = tempfile::tempdir().unwrap();
    let fonts = dir.path().join("fonts");
    std::fs::create_dir(&fonts).unwrap();
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ghost-shaper/tests/assets");
    for name in ["FiraCode-Regular.ttf", "DejaVuSansMono.ttf"] {
        std::fs::copy(assets.join(name), fonts.join(name)).unwrap();
    }
    let conf = dir.path().join("fonts.conf");
    std::fs::write(
        &conf,
        format!(
            r#"<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig>
  <dir>{fonts}</dir>
  <cachedir>{cache}</cachedir>
  <match target="scan">
    <test name="family"><string>Fira Code</string></test>
    <edit name="charset" mode="delete_all"/>
  </match>
</fontconfig>
"#,
            fonts = fonts.display(),
            cache = dir.path().join("cache").display(),
        ),
    )
    .unwrap();
    // SAFETY: the only test in this binary, set before anything initialises
    // fontconfig or spawns a thread that could read the environment.
    unsafe { std::env::set_var("FONTCONFIG_FILE", &conf) };

    // ★ is in DejaVu Sans Mono, not in Fira Code; the stripped Fira Code entry is
    // what fontconfig ranks first for it.
    let face = SystemFallback::new()
        .face_for('★')
        .expect("DejaVu Sans Mono covers ★, so fallback must find it");
    assert!(
        ghost_shaper::covers(face, '★'),
        "the fallback face must actually have ★, not be the charset-less best match"
    );
}
