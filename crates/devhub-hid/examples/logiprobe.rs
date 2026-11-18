//! Logitech HID++ 2.0 の解析ツール (PRO Wireless 等)。
//!
//! ```text
//! cargo run -p devhub-hid --example logiprobe              # 列挙 + 機能一覧 + オンボード情報
//! cargo run -p devhub-hid --example logiprobe -- dump      # ↑ + 現在プロファイル/ディレクトリ/ROM の 16 進ダンプ
//! cargo run -p devhub-hid --example logiprobe -- listen    # 受信を全部表示 (G HUB の通信の応答を傍受)
//! ```
//!
//! `listen` は G HUB を起動したまま実行する: Windows は input report を同じ
//! コレクションを開いた全ハンドルへ配るので、G HUB が送ったコマンドへの
//! **応答** (feature index・関数番号・値) がそのまま見える。G HUB で 1 項目だけ
//! 変えて差分を見る、という使い方をする (docs/PROTOCOL_NOTES.md §6.5)。
//! 解析が終わったら G HUB は scripts/remove-ghub.ps1 で消してよい。

use devhub_hid::hidpp::{crc_ccitt, parse_directory, parse_dpi_list, sector_valid};
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const FEATURES: &[(u16, &str)] = &[
    (0x0000, "Root"),
    (0x0001, "FeatureSet"),
    (0x0003, "DeviceFwVersion"),
    (0x0005, "DeviceName"),
    (0x0020, "ConfigChange"),
    (0x1000, "BatteryStatus"),
    (0x1001, "BatteryVoltage"),
    (0x1004, "UnifiedBattery"),
    (0x1814, "ChangeHost"),
    (0x1B04, "ReprogControlsV4"),
    (0x1D4B, "WirelessStatus"),
    (0x1F20, "AdcMeasurement"),
    (0x2201, "AdjustableDpi"),
    (0x2202, "ExtendedAdjustableDpi"),
    (0x2250, "AnalysisMode"),
    (0x8060, "ReportRate"),
    (0x8061, "ExtendedReportRate"),
    (0x8070, "ColorLedEffects"),
    (0x8071, "RgbEffects"),
    (0x8081, "PerKeyLighting"),
    (0x8090, "ModeStatus"),
    (0x8100, "OnboardProfiles"),
    (0x8110, "MouseButtonSpy"),
    (0x8111, "LatencyMonitoring"),
];

fn name_of(id: u16) -> &'static str {
    FEATURES
        .iter()
        .find(|f| f.0 == id)
        .map(|f| f.1)
        .unwrap_or("?")
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

struct Dev {
    h: HidDevice,
    idx: u8,
}

impl Dev {
    fn call(&self, fi: u8, func: u8, p: &[u8]) -> Result<[u8; 16], String> {
        let fsw = (func << 4) | 0x0A;
        let mut req = [0u8; 20];
        req[0] = 0x11;
        req[1] = self.idx;
        req[2] = fi;
        req[3] = fsw;
        req[4..4 + p.len().min(16)].copy_from_slice(&p[..p.len().min(16)]);
        self.h.write(&req).map_err(|e| e.to_string())?;
        let end = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < end {
            let mut r = [0u8; 64];
            let n = self
                .h
                .read_timeout(&mut r, 200)
                .map_err(|e| e.to_string())?;
            if n < 20 || r[0] != 0x11 || r[1] != self.idx {
                continue;
            }
            if r[2] == fi && r[3] == fsw {
                let mut o = [0u8; 16];
                o.copy_from_slice(&r[4..20]);
                return Ok(o);
            }
            if r[2] == 0xFF && r[3] == fi && r[4] == fsw {
                return Err(format!("hid++ error {:#04x}", r[5]));
            }
        }
        Err("timeout (スリープ中?)".into())
    }

    fn read_sector(&self, ob: u8, sector: u16, size: usize) -> Result<Vec<u8>, String> {
        let mut out = vec![0u8; size];
        let mut off = 0;
        while off < size {
            let at = off.min(size - 16);
            let r = self.call(
                ob,
                5,
                &[(sector >> 8) as u8, sector as u8, (at >> 8) as u8, at as u8],
            )?;
            let end = (at + 16).min(size);
            out[at..end].copy_from_slice(&r[..end - at]);
            off = end;
        }
        Ok(out)
    }
}

fn dump(label: &str, d: &[u8]) {
    let n = d.len() - 2;
    println!(
        "  -- {label} ({} B, crc {} stored={:04x} calc={:04x})",
        d.len(),
        if sector_valid(d) { "OK" } else { "NG" },
        u16::from_be_bytes([d[n], d[n + 1]]),
        crc_ccitt(&d[..n])
    );
    for (i, c) in d.chunks(16).enumerate() {
        println!("  {:04x}: {}", i * 16, hex(c));
    }
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let api = HidApi::new().expect("hidapi");
    let mut targets = Vec::new();
    println!("== Logitech HID インターフェース ==");
    for d in api.device_list().filter(|d| d.vendor_id() == 0x046D) {
        println!(
            "046d:{:04x} if={} up={:04x} u={:04x} {:?}",
            d.product_id(),
            d.interface_number(),
            d.usage_page(),
            d.usage(),
            d.product_string()
        );
        if d.usage_page() == 0xFF00 && d.usage() == 0x0002 {
            targets.push((d.product_id(), d.path().to_owned()));
        }
    }
    if targets.is_empty() {
        println!("HID++ long コレクション (FF00:0002) が見つかりません");
        return;
    }

    for (pid, path) in targets {
        let h = api.open_path(&path).expect("open");
        // C0xx = 有線直結 (0xFF)、C5xx = レシーバー (index 1)。
        let idx = if pid & 0xFF00 == 0xC500 { 0x01 } else { 0xFF };
        let dev = Dev { h, idx };
        println!("\n== 046d:{pid:04x} device index {idx:#04x} ==");

        if mode == "listen" {
            println!("受信を表示します (Ctrl+C で終了)。G HUB で設定を 1 つずつ変えてください。");
            let mut names: HashMap<u8, u16> = HashMap::new();
            // feature index → ID の表を先に作る (G HUB の応答を読むため)。
            if let Ok(r) = dev.call(0, 0, &[0x00, 0x01]) {
                let count = dev.call(r[0], 0, &[]).map(|c| c[0]).unwrap_or(0);
                for i in 0..=count {
                    if let Ok(e) = dev.call(r[0], 1, &[i]) {
                        names.insert(i, u16::from_be_bytes([e[0], e[1]]));
                    }
                }
            }
            loop {
                let mut r = [0u8; 64];
                let Ok(n) = dev.h.read_timeout(&mut r, 1000) else {
                    break;
                };
                if n == 0 {
                    continue;
                }
                let fi = if r[2] == 0xFF { r[3] } else { r[2] };
                let id = names.get(&fi).copied().unwrap_or(0xFFFF);
                println!(
                    "<< {} | feat[{fi:#04x}]={id:04x} {} fn{} sw{:x}",
                    hex(&r[..n]),
                    name_of(id),
                    r[3] >> 4,
                    r[3] & 0x0F
                );
            }
            continue;
        }

        match dev.call(0, 1, &[0, 0, 0x5A]) {
            Ok(r) => println!("HID++ {}.{}", r[0], r[1]),
            Err(e) => {
                println!("ping 失敗: {e}");
                continue;
            }
        }
        let mut feats = HashMap::new();
        if let Ok(r) = dev.call(0, 0, &[0x00, 0x01]) {
            let fs = r[0];
            let count = dev.call(fs, 0, &[]).map(|c| c[0]).unwrap_or(0);
            println!("-- feature 一覧 ({count}) --");
            for i in 0..=count {
                if let Ok(e) = dev.call(fs, 1, &[i]) {
                    let id = u16::from_be_bytes([e[0], e[1]]);
                    feats.insert(id, i);
                    println!(
                        "  [{i:#04x}] {id:04x} {:<22} type={:02x} ver={}",
                        name_of(id),
                        e[2],
                        e[3]
                    );
                }
            }
        }
        if let Some(&fi) = feats.get(&0x0005) {
            if let Ok(r) = dev.call(fi, 0, &[]) {
                let mut s = Vec::new();
                while s.len() < r[0] as usize {
                    match dev.call(fi, 1, &[s.len() as u8]) {
                        Ok(c) => s.extend_from_slice(&c[..(r[0] as usize - s.len()).min(16)]),
                        Err(_) => break,
                    }
                }
                println!("名前: {}", String::from_utf8_lossy(&s));
            }
        }
        if let Some(&fi) = feats.get(&0x2201) {
            if let Ok(r) = dev.call(fi, 1, &[0]) {
                println!("DPI リスト: {} → {:?}", hex(&r), parse_dpi_list(&r[1..]));
            }
            if let Ok(r) = dev.call(fi, 2, &[0]) {
                println!(
                    "現在 DPI: {} (既定 {})",
                    u16::from_be_bytes([r[1], r[2]]),
                    u16::from_be_bytes([r[3], r[4]])
                );
            }
        }
        if let Some(&fi) = feats.get(&0x8060) {
            if let (Ok(l), Ok(c)) = (dev.call(fi, 0, &[]), dev.call(fi, 1, &[])) {
                println!("ポーリング: 対応マスク {:08b} / 現在 {}ms", l[0], c[0]);
            }
        }
        if let Some(&fi) = feats.get(&0x1001) {
            if let Ok(r) = dev.call(fi, 0, &[]) {
                println!(
                    "電池: {}mV flags={:02x}",
                    u16::from_be_bytes([r[0], r[1]]),
                    r[2]
                );
            }
        }
        let Some(&ob) = feats.get(&0x8100) else {
            println!("OnboardProfiles 無し");
            continue;
        };
        let Ok(info) = dev.call(ob, 0, &[]) else {
            continue;
        };
        let sector_size = u16::from_be_bytes([info[7], info[8]]) as usize;
        println!(
            "オンボード: {} → memModel={} format={} macro={} profiles={} oob={} buttons={} sectors={} sectorSize={}",
            hex(&info),
            info[0],
            info[1],
            info[2],
            info[3],
            info[4],
            info[5],
            info[6],
            sector_size
        );
        let mode_r = dev.call(ob, 2, &[]).map(|r| r[0]);
        let cur = dev.call(ob, 4, &[]).map(|r| r[1]);
        let dpi_ix = dev.call(ob, 11, &[]).map(|r| r[0]);
        println!("モード={mode_r:?} (1=オンボード 2=ホスト) 現在プロファイル={cur:?} DPI index={dpi_ix:?}");

        if mode == "dump" && sector_size >= 16 {
            for (label, sec) in [
                ("ユーザーディレクトリ 0x0000", 0x0000u16),
                ("ROM ディレクトリ 0x0100", 0x0100),
            ] {
                match dev.read_sector(ob, sec, sector_size) {
                    Ok(d) => {
                        dump(label, &d);
                        println!("  entries: {:?}", parse_directory(&d, info[3] as usize));
                    }
                    Err(e) => println!("  {label}: {e}"),
                }
            }
            for s in [1u16, 2, 3, 0x0101] {
                match dev.read_sector(ob, s, sector_size) {
                    Ok(d) => dump(&format!("セクタ {s:#06x}"), &d),
                    Err(e) => println!("  セクタ {s:#06x}: {e}"),
                }
            }
        }
    }
}
