use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Webview};
use tauri_plugin_updater::{Update, UpdaterExt};

const RELEASES_URL: &str = "https://github.com/XxMinor/mykvm/releases";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum UpdateChannel {
    Beta,
    #[serde(other)]
    Stable,
}

impl Default for UpdateChannel {
    fn default() -> Self {
        let version = crate::APP_VERSION
            .get()
            .map(String::as_str)
            .unwrap_or(env!("CARGO_PKG_VERSION"));
        if version.contains("-beta.") {
            Self::Beta
        } else {
            Self::Stable
        }
    }
}

impl UpdateChannel {
    fn endpoint(self) -> &'static str {
        match self {
            Self::Stable => "https://github.com/XxMinor/mykvm/releases/latest/download/latest.json",
            Self::Beta => "https://github.com/XxMinor/mykvm/releases/download/beta/latest.json",
        }
    }

    pub(crate) fn releases_url(self) -> String {
        match self {
            Self::Stable => format!("{RELEASES_URL}/latest"),
            Self::Beta => format!("{RELEASES_URL}/tag/beta"),
        }
    }
}

fn current_version(app: &AppHandle) -> String {
    crate::APP_VERSION
        .get()
        .cloned()
        .unwrap_or_else(|| app.package_info().version.to_string())
}

fn is_available(channel: UpdateChannel, current: &str, latest: &str) -> Result<bool, String> {
    let current = current
        .parse::<semver::Version>()
        .map_err(|error| error.to_string())?;
    let latest = latest
        .parse::<semver::Version>()
        .map_err(|error| error.to_string())?;
    Ok(latest > current && (channel == UpdateChannel::Beta || latest.pre.is_empty()))
}

async fn latest_release(app: &AppHandle, channel: UpdateChannel) -> Result<Option<Update>, String> {
    app.updater_builder()
        .endpoints(vec![channel
            .endpoint()
            .parse::<tauri::Url>()
            .map_err(|error| error.to_string())?])
        .map_err(|error| error.to_string())?
        .timeout(std::time::Duration::from_secs(20))
        // Read metadata even when this local build is newer. Installation still
        // requires is_available, and the normal updater verifies the package.
        .version_comparator(|_, _| true)
        .build()
        .map_err(|error| error.to_string())?
        .check()
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn available_release(
    app: &AppHandle,
    channel: UpdateChannel,
) -> Result<Option<Update>, String> {
    match latest_release(app, channel).await? {
        Some(update) if is_available(channel, &current_version(app), &update.version)? => {
            Ok(Some(update))
        }
        _ => Ok(None),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateCheckResult {
    available: bool,
    latest_version: Option<String>,
    update: Option<UpdateMetadata>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateMetadata {
    rid: tauri::ResourceId,
    version: String,
    current_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    raw_json: serde_json::Value,
}

#[tauri::command]
pub(crate) async fn check_app_update(
    webview: Webview,
    channel: UpdateChannel,
) -> Result<UpdateCheckResult, String> {
    let Some(update) = latest_release(webview.app_handle(), channel).await? else {
        return Ok(UpdateCheckResult {
            available: false,
            latest_version: None,
            update: None,
        });
    };
    let current_version = current_version(webview.app_handle());
    let latest_version = Some(update.version.clone());
    let available = is_available(channel, &current_version, &update.version)?;
    let metadata = if available {
        let mut metadata = UpdateMetadata {
            rid: 0,
            version: update.version.clone(),
            current_version,
            date: update
                .raw_json
                .get("pub_date")
                .and_then(|value| value.as_str())
                .map(str::to_owned),
            body: update.body.clone(),
            raw_json: update.raw_json.clone(),
        };
        // The JS updater downloads from this same webview's resource table.
        metadata.rid = webview.resources_table().add(update);
        Some(metadata)
    } else {
        None
    };
    Ok(UpdateCheckResult {
        available,
        latest_version,
        update: metadata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_use_separate_manifests_and_release_pages() {
        assert!(UpdateChannel::Stable
            .endpoint()
            .contains("/releases/latest/"));
        assert!(UpdateChannel::Beta
            .endpoint()
            .contains("/releases/download/beta/"));
        assert!(UpdateChannel::Beta.releases_url().ends_with("/tag/beta"));
    }

    #[test]
    fn switching_channels_never_downgrades_or_offers_beta_on_stable() {
        assert!(is_available(UpdateChannel::Beta, "0.9.13", "0.9.14-beta.103").unwrap());
        assert!(!is_available(UpdateChannel::Stable, "0.9.13", "0.9.14-beta.103").unwrap());
        assert!(!is_available(UpdateChannel::Stable, "0.9.14-beta.103", "0.9.13").unwrap());
        assert!(is_available(UpdateChannel::Stable, "0.9.14-beta.103", "0.9.14").unwrap());
        assert!(!is_available(UpdateChannel::Beta, "0.9.14-beta.104", "0.9.14-beta.103").unwrap());
        assert!(!is_available(UpdateChannel::Beta, "0.9.14-beta.104", "0.9.14-beta.104").unwrap());
    }

    #[test]
    fn update_channel_survives_config_reload_without_restarting_input() {
        let mut layout = crate::detect_fallback_layout();
        layout.update_channel = UpdateChannel::Beta;
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("layout.json");
        crate::write_layout_to_disk(&file, &layout).unwrap();
        let saved: crate::LayoutState =
            serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        assert_eq!(saved.update_channel, UpdateChannel::Beta);
        layout.update_channel = UpdateChannel::Stable;
        assert!(!crate::runtime_relevant_layout_changed(&layout, &saved));
    }

    #[test]
    fn older_and_future_channel_settings_keep_the_existing_pairing() {
        let mut data = serde_json::to_value(crate::detect_fallback_layout()).unwrap();
        data["pairSecret"] = "preserved-pairing".into();
        data.as_object_mut().unwrap().remove("updateChannel");
        let legacy: crate::LayoutState = serde_json::from_value(data.clone()).unwrap();
        assert_eq!(legacy.update_channel, UpdateChannel::default());
        data["updateChannel"] = "future-channel".into();
        let future: crate::LayoutState = serde_json::from_value(data).unwrap();
        assert_eq!(future.update_channel, UpdateChannel::Stable);
        assert_eq!(future.pair_secret, "preserved-pairing");
    }
}
