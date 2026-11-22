//! U60 Pro transmit-power preferences and driver readback. On B11, `iw set txpower`
//! succeeds without changing the power. The driver's `iwpriv setTxPower` takes whole dBm.

use std::process::Command;
use std::time::{Duration, Instant};

use luyaz_api::wifi::TX_POWER_TIERS;
use luyaz_wifi::{WifiLock, layout, output_within, psm};
use serde::Serialize;

pub(crate) const PREF: [&str; 2] = ["wireless.wifi0.txpowerpercent", "wireless.wifi1.txpowerpercent"];
const CAP: [&str; 2] = ["wireless.wifi0.max_power", "wireless.wifi1.max_power"];
const GUEST: [&str; 2] = ["guest_2g", "guest_5g"];
const DEADLINE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Default, Serialize)]
pub(crate) struct Reading {
    pub percent: Option<u8>,
    pub mbm: Option<i32>,
    pub target_mbm: Option<i32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Link {
    index: u32,
    phy: u32,
    mhz: u16,
    mbm: Option<i32>,
}

fn mbm(dbm: &str) -> Option<i32> {
    let n = dbm.parse::<f64>().ok()?;
    (-100.0..=100.0).contains(&n).then(|| (n * 100.0).round() as i32)
}

fn link(info: &str, band: usize) -> Option<Link> {
    let (mut index, mut phy, mut mhz, mut power, mut ap) = (None, None, None, None, false);
    for line in info.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(key) = fields.next() else { continue };
        match key {
            "ifindex" => index = fields.next().and_then(|n| n.parse().ok()),
            "wiphy" => phy = fields.next().and_then(|n| n.parse().ok()),
            "type" => ap = fields.next() == Some("AP"),
            "channel" => mhz = fields.nth(1).and_then(|n| n.strip_prefix('(')?.parse().ok()),
            "txpower" => power = fields.next().and_then(mbm),
            _ => {}
        }
    }
    let mhz = mhz?;
    let in_band = if band == 0 { (2400..2500).contains(&mhz) } else { (5000..5925).contains(&mhz) };
    (ap && in_band).then_some(Link { index: index?, phy: phy?, mhz, mbm: power })
}

fn channel_cap(phys: &str, link: Link) -> Option<i32> {
    let mut current = None;
    for line in phys.lines() {
        if let Some(name) = line.strip_prefix("Wiphy phy") {
            current = name.trim().parse().ok();
        }
        if current != Some(link.phy) {
            continue;
        }
        let mut words = line.split_ascii_whitespace();
        if words.next() == Some("*")
            && words.next().and_then(|n| n.parse::<u16>().ok()) == Some(link.mhz)
            && words.next() == Some("MHz")
        {
            let cap = words.nth(1)?.strip_prefix('(')?;
            return (words.next() == Some("dBm)")).then(|| mbm(cap)).flatten();
        }
    }
    None
}

fn percent(cfg: &layout::Uci, band: usize) -> Option<u8> {
    cfg.get(PREF[band])?.parse().ok().filter(|n| *n <= 100)
}

fn offset(percent: u8) -> i32 {
    // 10 log10(p/100), rounded to the nearest step the driver accepts.
    // Other persisted values keep the firmware's legacy buckets, including on undo.
    match percent {
        75 | 80..=89 => 1,
        90..=100 => 0,
        60..=79 => 2,
        50..=59 => 3,
        40..=49 => 4,
        30..=39 => 5,
        20..=29 => 6,
        10..=19 => 7,
        _ => 8,
    }
}

fn target(cfg: &layout::Uci, band: usize, link: Link, phys: &str) -> Option<i32> {
    let mut vendor_cap = cfg.get(CAP[band])?.parse::<i32>().ok()?;
    if !(0..=100).contains(&vendor_cap) {
        return None;
    }
    if band == 1 && (5180..=5320).contains(&link.mhz)
        && cfg.get("wireless.readonly.n79_coex").is_some_and(|v| v == "1")
    {
        vendor_cap -= 3;
    }
    let cap = vendor_cap.min(channel_cap(phys, link)?.div_euclid(100));
    let dbm = cap - offset(percent(cfg, band)?);
    (dbm > 0).then_some(dbm * 100)
}

fn iw(args: &[&str]) -> Option<String> {
    let bin = std::env::var_os("LUYAZ_IW_BIN").unwrap_or_else(|| "/usr/sbin/iw".into());
    let out = output_within(Command::new(bin).args(args), DEADLINE).ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub(crate) fn readings(cfg: Option<&layout::Uci>, info: [&str; 2], up: [bool; 2]) -> [Reading; 2] {
    let links = std::array::from_fn::<_, 2, _>(|i| up[i].then(|| link(info[i], i)).flatten());
    let phys = links.iter().any(Option::is_some).then(|| iw(&["phy"])).flatten();
    std::array::from_fn(|i| Reading {
        percent: cfg.and_then(|cfg| percent(cfg, i)),
        mbm: links[i].and_then(|link| link.mbm),
        target_mbm: cfg.zip(links[i]).zip(phys.as_deref()).and_then(|((cfg, link), phy)| target(cfg, i, link, phy)),
    })
}

fn ensure_iface(cfg: &layout::Uci, band: usize, iface: &str, phys: &str) -> Result<(), String> {
    let read = || iw(&["dev", iface, "info"]).as_deref().and_then(|out| link(out, band));
    let before = read().ok_or_else(|| format!("{iface}: no active AP channel"))?;
    let want = target(cfg, band, before, phys)
        .ok_or_else(|| format!("{iface}: safe transmit-power limit is unavailable"))?;
    if before.mbm == Some(want) {
        return Ok(());
    }
    let out = output_within(Command::new("/usr/sbin/iwpriv")
        .args([iface, "setTxPower", &(want / 100).to_string()]), DEADLINE)
        .map_err(|e| format!("{iface}: setTxPower: {e}"))?;
    if !out.status.success() {
        return Err(format!("{iface}: setTxPower returned {}", out.status));
    }
    let expected = Link { mbm: Some(want), ..before };
    let until = Instant::now() + Duration::from_secs(5);
    // Firmware statistics can lag the private ioctl by a few seconds.
    loop {
        if read() == Some(expected) {
            return Ok(());
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(Duration::from_millis(500)));
    }
    Err(format!("{iface}: transmit power did not read back as {} dBm", want / 100))
}

pub(crate) fn apply(cfg: &layout::Uci, bands: [bool; 2], _lock: &WifiLock) -> Result<(), String> {
    let up = psm::main_up();
    let bands = std::array::from_fn::<_, 2, _>(|i| up[i] && bands[i] && percent(cfg, i).is_some());
    if !bands.contains(&true) {
        return Ok(()); // The settings are saved for when the AP comes up.
    }
    let phys = iw(&["phy"]).ok_or("cannot read Wi-Fi channel power limits")?;
    let mut error = Ok(());
    for (i, apply) in bands.into_iter().enumerate() {
        if !apply {
            continue;
        }
        let guest = layout::ifname(cfg, GUEST[i]).filter(|name| {
            std::fs::read_to_string(format!("/sys/class/net/{name}/operstate"))
                .is_ok_and(|s| s.trim() == "up")
        });
        for iface in std::iter::once(psm::MAIN_IFACES[i]).chain(guest) {
            let result = ensure_iface(cfg, i, iface, &phys);
            error = error.and(result);
        }
    }
    error
}

/// The stock Wi-Fi startup and channel changes can reset the power. Reapply saved tiers
/// under the same lock as Wi-Fi transactions, even after a firmware-initiated reload.
pub(crate) async fn maintain() {
    loop {
        if psm::main_up().contains(&true) {
            crate::state::blocking(|| {
                let path = crate::state::host_path(luyaz_wifi::LOCK_FILE);
                if let Ok(lock) = luyaz_wifi::lock_at(&path, Duration::ZERO)
                    && let Some(cfg) = crate::state::wifi_map_sync()
                {
                    let managed = std::array::from_fn(|i| percent(&cfg, i).is_some_and(|p| TX_POWER_TIERS.contains(&p)));
                    let _ = apply(&cfg, managed, &lock);
                }
            }).await;
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "Interface wlan2\n\tifindex 39\n\ttype AP\n\twiphy 7\n\tchannel 36 (5180 MHz), width: 80 MHz, center1: 5210 MHz\n\ttxpower 16.50 dBm\n";
    const PHYS: &str = "Wiphy phy0\n\t* 5180 MHz [36] (30.0 dBm)\nWiphy phy7\n\t* 5180 MHz [36] (17.5 dBm)\n\t* 5200 MHz [40] (disabled)\n";

    #[test]
    fn tiers_use_the_active_phys_cap_and_coexistence_limit() {
        let mut cfg = layout::Uci::from([(CAP[1].into(), "18".into())]);
        let active = link(INFO, 1).unwrap();
        for (tier, expected_mbm) in TX_POWER_TIERS.into_iter().zip([1100, 1400, 1600, 1700]) {
            cfg.insert(PREF[1].into(), tier.to_string());
            assert_eq!(target(&cfg, 1, active, PHYS), Some(expected_mbm));
        }
        cfg.insert("wireless.readonly.n79_coex".into(), "1".into());
        assert_eq!(target(&cfg, 1, active, PHYS), Some(1500));
        assert_eq!(target(&cfg, 1, Link { mhz: 5200, ..active }, PHYS), None);
        assert_eq!(active.mbm, Some(1650));
        assert!(link(INFO, 0).is_none());
    }
}
