//! Checks the built-in pins against the real projects. This touches the
//! network, so it is ignored by default; run it by hand before a release:
//!
//!     cargo test --features testing --test tools_pins -- --ignored

use rustdm::tools::checksum::expected_for;
use rustdm::tools::openpgp::{parse_fingerprint, verify_detached, PinnedKey};
use rustdm::tools::pins::{
    pin_for, Platform, GITHUB_API, GITHUB_DOWNLOADS, YTDLP_FINGERPRINT, YTDLP_KEY, YTDLP_REPO,
};
use rustdm::tools::Tool;

async fn get_text(client: &reqwest::Client, url: &str) -> String {
    let response = client.get(url).send().await.expect(url);
    assert!(
        response.status().is_success(),
        "{url}: {}",
        response.status()
    );
    response.text().await.expect(url)
}

#[tokio::test]
#[ignore = "touches github.com; run by hand before a release"]
async fn pins_still_resolve() {
    let client = reqwest::Client::builder()
        .user_agent("rustdm-pin-check")
        .build()
        .unwrap();

    for platform in [Platform::Windows64, Platform::Linux64] {
        for tool in [Tool::Ffmpeg, Tool::Deno] {
            let pin = pin_for(tool, platform).unwrap();

            // The project's own checksum file lists exactly our hash.
            let checksums = get_text(
                &client,
                &format!("{GITHUB_DOWNLOADS}/{}", pin.published_checksums_path),
            )
            .await;
            assert_eq!(
                expected_for(&checksums, &pin.asset).as_deref(),
                Some(pin.sha256.as_str()),
                "{tool} on {platform:?}"
            );

            // The asset is still downloadable.
            let head = client
                .head(format!("{GITHUB_DOWNLOADS}/{}", pin.path))
                .send()
                .await
                .unwrap();
            assert!(
                head.status().is_success(),
                "{}: {}",
                pin.path,
                head.status()
            );
        }
    }

    // yt-dlp's newest nightly is signed by the pinned key.
    let latest: serde_json::Value = serde_json::from_str(
        &get_text(
            &client,
            &format!("{GITHUB_API}/repos/{YTDLP_REPO}/releases/latest"),
        )
        .await,
    )
    .unwrap();
    let tag = latest["tag_name"].as_str().unwrap();
    let base = format!("{GITHUB_DOWNLOADS}/{YTDLP_REPO}/releases/download/{tag}");
    let sums = client
        .get(format!("{base}/SHA2-256SUMS"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let sig = client
        .get(format!("{base}/SHA2-256SUMS.sig"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let key = PinnedKey::load(YTDLP_KEY, &parse_fingerprint(YTDLP_FINGERPRINT).unwrap()).unwrap();
    verify_detached(&key, &sums, &sig).expect("latest yt-dlp release verifies");
}
