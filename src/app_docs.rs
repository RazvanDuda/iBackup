//! Readable copy of each app's shared "Documents" folder (the "On My iPhone" files for
//! apps that enable File Sharing), via installation_proxy + house_arrest.

use std::path::Path;

use anyhow::Result;
use idevice::afc::AfcClient;
use idevice::house_arrest::HouseArrestClient;
use idevice::installation_proxy::InstallationProxyClient;
use idevice::usbmuxd::UsbmuxdDevice;
use plist::Value;

use crate::afc_copy;
use crate::device;
use crate::util::{bytes_bar, sanitize_component, Stats};

struct App {
    bundle_id: String,
    name: String,
}

async fn shared_apps(p: &idevice::provider::UsbmuxdProvider) -> Result<Vec<App>> {
    let mut ip: InstallationProxyClient = device::connect(p, "the app list service").await?;
    let apps = ip.get_apps(Some("User"), None).await?;
    let mut out: Vec<App> = apps
        .into_iter()
        .filter_map(|(id, v)| {
            let d = v.as_dictionary()?;
            let shared = matches!(d.get("UIFileSharingEnabled"), Some(Value::Boolean(true)));
            if !shared {
                return None;
            }
            let name = d
                .get("CFBundleDisplayName")
                .or_else(|| d.get("CFBundleName"))
                .and_then(|v| v.as_string())
                .unwrap_or(&id)
                .to_string();
            Some(App { bundle_id: id, name })
        })
        .collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

async fn vend(p: &idevice::provider::UsbmuxdProvider, bundle: &str) -> Result<AfcClient> {
    let ha: HouseArrestClient = device::connect(p, "house_arrest").await?;
    Ok(ha.vend_documents(bundle.to_string()).await?)
}

pub async fn run(dev: &UsbmuxdDevice, out: &Path, only: &[String]) -> Result<Stats> {
    let p = device::provider(dev)?;
    let mut stats = Stats::default();
    let mut apps = shared_apps(&p).await?;
    if !only.is_empty() {
        apps.retain(|a| only.iter().any(|o| o.eq_ignore_ascii_case(&a.bundle_id)));
    }
    if apps.is_empty() {
        stats.notes.push("no installed apps expose a Documents folder".into());
        return Ok(stats);
    }
    crate::util::say(&format!("app documents: {} apps", apps.len()));

    for app in apps {
        let dest = out.join(sanitize_component(&format!("{} ({})", app.name, app.bundle_id)));
        let mut done = false;
        for attempt in 1..=2 {
            let mut afc = match vend(&p, &app.bundle_id).await {
                Ok(a) => a,
                Err(e) => {
                    stats.errors.push(format!("{}: {e}", app.bundle_id));
                    break;
                }
            };
            let mut s = Stats::default();
            let files = match afc_copy::scan(&mut afc, "/Documents", &[], &mut s).await {
                Ok(f) => f,
                Err(e) if attempt == 1 => {
                    eprintln!("  {}: connection lost ({e}), retrying", app.name);
                    afc_copy::retry_pause().await;
                    continue;
                }
                Err(e) => {
                    stats.errors.push(format!("{}: {e}", app.bundle_id));
                    break;
                }
            };
            if files.is_empty() {
                done = true;
                break;
            }
            let bar = bytes_bar(afc_copy::pending_bytes(&files), "app docs");
            bar.println(format!("  {}", app.name));
            let r = afc_copy::copy_all(&mut afc, &files, "/Documents", &dest, &bar, &mut s).await;
            bar.finish_and_clear();
            stats.merge(s);
            match r {
                Ok(()) => {
                    done = true;
                    break;
                }
                Err(e) if attempt == 1 => {
                    eprintln!("  {}: connection lost ({e}), retrying", app.name);
                    afc_copy::retry_pause().await;
                }
                Err(e) => stats.errors.push(format!("{}: {e}", app.bundle_id)),
            }
        }
        if !done {
            eprintln!("  skipped {} (see report)", app.name);
        }
    }
    Ok(stats)
}
