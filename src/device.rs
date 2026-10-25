use anyhow::{anyhow, bail, Context, Result};
use idevice::{
    lockdown::LockdownClient,
    provider::{IdeviceProvider, UsbmuxdProvider},
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
    IdeviceService,
};
use plist::{Dictionary, Value};

pub const LABEL: &str = "ibackup";

#[cfg(windows)]
const MUX_HINT: &str = "Could not reach Apple Mobile Device Service (127.0.0.1:27015).\n\
  Install the \"Apple Devices\" app from the Microsoft Store (or iTunes), then reconnect the iPhone.";
#[cfg(target_os = "macos")]
const MUX_HINT: &str = "Could not reach usbmuxd (/var/run/usbmuxd). It ships with macOS; try reconnecting the cable.";
#[cfg(all(unix, not(target_os = "macos")))]
const MUX_HINT: &str = "Could not reach usbmuxd (/var/run/usbmuxd).\n\
  Install it (Fedora: `sudo dnf install usbmuxd`; Debian/Ubuntu: `sudo apt install usbmuxd`),\n\
  then unplug and replug the iPhone so udev starts it (or `sudo systemctl start usbmuxd`).";

fn mux_addr() -> Result<UsbmuxdAddr> {
    UsbmuxdAddr::from_env_var().context("USBMUXD_SOCKET_ADDRESS is not a valid address")
}

pub async fn mux() -> Result<UsbmuxdConnection> {
    let addr = mux_addr()?;
    addr.connect(0).await.map_err(|e| anyhow!("{MUX_HINT}\n  ({e})"))
}

/// USB-attached devices only (a phone can also show up a second time over Wi-Fi).
pub async fn usb_devices() -> Result<Vec<UsbmuxdDevice>> {
    let mut m = mux().await?;
    let devs = m.get_devices().await.context("listing devices from usbmuxd")?;
    Ok(devs
        .into_iter()
        .filter(|d| d.connection_type == Connection::Usb)
        .collect())
}

pub async fn select(udid: Option<&str>) -> Result<UsbmuxdDevice> {
    let devs = usb_devices().await?;
    match udid {
        Some(u) => devs
            .into_iter()
            .find(|d| d.udid.eq_ignore_ascii_case(u))
            .ok_or_else(|| anyhow!("No USB device with UDID {u}. Run `ibackup devices`.")),
        None => match devs.len() {
            0 => bail!("No iPhone found over USB. Plug it in, unlock it, and run `ibackup devices`."),
            1 => Ok(devs.into_iter().next().unwrap()),
            _ => bail!("Several devices are connected; pick one with --udid (see `ibackup devices`)."),
        },
    }
}

pub fn provider(dev: &UsbmuxdDevice) -> Result<UsbmuxdProvider> {
    Ok(dev.to_provider(mux_addr()?, LABEL))
}

/// Does a working pair record exist for this device?
async fn session_works(p: &UsbmuxdProvider) -> bool {
    let Ok(pf) = p.get_pairing_file().await else { return false };
    let Ok(mut l) = LockdownClient::connect(p).await else { return false };
    l.start_session(&pf).await.is_ok()
}

/// Pair if needed. The phone shows "Trust This Computer?" and asks for the passcode.
pub async fn ensure_paired(dev: &UsbmuxdDevice) -> Result<()> {
    let p = provider(dev)?;
    if session_works(&p).await {
        return Ok(());
    }
    pair(dev).await
}

pub async fn pair(dev: &UsbmuxdDevice) -> Result<()> {
    let p = provider(dev)?;
    let mut lockdown = LockdownClient::connect(&p)
        .await
        .context("connecting to lockdownd")?;
    let mut m = mux().await?;
    let buid = m.get_buid().await.context("reading host BUID from usbmuxd")?;
    let host_id = uuid::Uuid::new_v4().to_string().to_uppercase();
    let host_name = gethostname::gethostname().to_string_lossy().into_owned();

    let sp = crate::util::spinner("Unlock the iPhone and tap \"Trust\", then enter your passcode…");
    let result = lockdown.pair(host_id, buid, Some(host_name.as_str())).await;
    sp.finish_and_clear();
    let mut pf = result.map_err(|e| anyhow!("Pairing failed: {e}. Was \"Trust\" declined?"))?;

    lockdown
        .start_session(&pf)
        .await
        .context("new pair record did not work")?;
    pf.udid = Some(dev.udid.clone());
    let bytes = pf.serialize().context("serializing pair record")?;
    m.save_pair_record(&dev.udid, bytes)
        .await
        .context("saving pair record to usbmuxd")?;
    crate::util::say("Paired with the iPhone. If photos don't copy, keep the screen unlocked.");
    Ok(())
}

/// All lockdown values in the default domain (DeviceName, ProductType, ProductVersion…).
pub async fn info(p: &dyn IdeviceProvider) -> Result<Dictionary> {
    let pf = p.get_pairing_file().await.context("no pair record; run `ibackup pair`")?;
    let mut l = LockdownClient::connect(p).await.context("connecting to lockdownd")?;
    l.start_session(&pf).await.context("starting lockdown session")?;
    let v = l.get_value(None, None).await.context("reading device values")?;
    v.into_dictionary()
        .ok_or_else(|| anyhow!("unexpected lockdown response"))
}

/// Whether the phone currently encrypts its local backups.
pub async fn will_encrypt(p: &dyn IdeviceProvider) -> Result<bool> {
    let pf = p.get_pairing_file().await?;
    let mut l = LockdownClient::connect(p).await?;
    l.start_session(&pf).await?;
    match l
        .get_value(Some("WillEncrypt"), Some("com.apple.mobile.backup"))
        .await
    {
        Ok(Value::Boolean(b)) => Ok(b),
        Ok(_) | Err(_) => Ok(false),
    }
}

pub fn get_str<'a>(d: &'a Dictionary, k: &str) -> &'a str {
    d.get(k).and_then(|v| v.as_string()).unwrap_or("?")
}

/// Folder name for this device, e.g. "Alex-s-iPhone_00008110".
pub fn folder_name(info: &Dictionary, udid: &str) -> String {
    let name = crate::util::sanitize_component(get_str(info, "DeviceName")).replace(' ', "-");
    let short: String = udid.chars().take(8).collect();
    format!("{name}_{short}")
}

pub async fn print_devices() -> Result<()> {
    let devs = usb_devices().await?;
    if devs.is_empty() {
        println!("No devices connected over USB.");
        return Ok(());
    }
    for d in devs {
        let p = provider(&d)?;
        match info(&p).await {
            Ok(i) => println!(
                "{}  {}  {} (iOS {})",
                d.udid,
                get_str(&i, "DeviceName"),
                get_str(&i, "ProductType"),
                get_str(&i, "ProductVersion")
            ),
            Err(_) => println!("{}  (not paired yet; run `ibackup pair`)", d.udid),
        }
    }
    Ok(())
}

/// Connect a lockdown service to the device, with a friendlier error.
pub async fn connect<S: IdeviceService>(p: &UsbmuxdProvider, what: &str) -> Result<S> {
    S::connect(p)
        .await
        .map_err(|e| anyhow!("Could not start {what} on the device: {e}\n  Is the iPhone unlocked?"))
}
