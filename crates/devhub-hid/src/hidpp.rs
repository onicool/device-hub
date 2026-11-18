//! Logitech HID++ 2.0 系マウス (PRO Wireless ほか) — Logicool G HUB の置き換え。
//!
//! G HUB を常駐させなくても、設定を**マウス本体のオンボードメモリ**
//! (feature 0x8100 Onboard Profiles) に直接書き込むので、アプリを閉じても
//! (G HUB をアンインストールしても) DPI・ポーリング・ボタン・LED は保持される。
//!
//! プロトコルの要点 (docs/PROTOCOL_NOTES.md §6 に詳細):
//!
//! - 設定チャネル: `FF00:0002` (HID++ long report, report ID 0x11, 20B)。
//!   Windows では short (0x10, `FF00:0001`) と別コレクションになるので long だけ開く。
//! - 送信 `[0x11, devIdx, featIdx, func<<4 | swId, params[16]]`。
//!   devIdx = 0xFF (有線直結) / 0x01 (LIGHTSPEED レシーバー経由)。
//! - 応答は同じヘッダ。エラーは `[0x11, devIdx, 0xFF, featIdx, func|sw, err]`。
//! - feature は番号ではなく **index** で呼ぶ: Root (index 0) の getFeature で
//!   feature ID → index を引いてから使う (ファームごとに index が違う)。
//! - レシーバー経由でマウスが寝ている/電源オフだと応答が来ない
//!   (HID++ 1.0 のエラーは short コレクションに出るので long 側ではタイムアウト)。
//!   → `DeviceInfo.asleep` を立てる。
//!
//! 参考にした公開資料: Logitech が公開した HID++ 2.0 feature 仕様、
//! libratbag (hidpp20.c / MIT) と Solaar (GPL — コードは流用せず仕様の確認のみ)。

use devhub_core::backend::*;
use devhub_core::model::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- 定数

const REPORT_LONG: u8 = 0x11;
const REPORT_SHORT: u8 = 0x10;
const LONG_LEN: usize = 20;
/// ソフトウェア ID (0 は本体からの自律通知なので使わない)。
const SW_ID: u8 = 0x0A;

/// feature ID。
mod feat {
    pub const ROOT: u16 = 0x0000;
    pub const FW_VERSION: u16 = 0x0003;
    pub const DEVICE_NAME: u16 = 0x0005;
    pub const BATTERY_STATUS: u16 = 0x1000;
    pub const BATTERY_VOLTAGE: u16 = 0x1001;
    pub const UNIFIED_BATTERY: u16 = 0x1004;
    pub const ADJUSTABLE_DPI: u16 = 0x2201;
    pub const REPORT_RATE: u16 = 0x8060;
    pub const COLOR_LED_EFFECTS: u16 = 0x8070;
    pub const ONBOARD_PROFILES: u16 = 0x8100;
}

/// 0x8100 の関数番号。
mod ob {
    pub const GET_INFO: u8 = 0;
    pub const SET_MODE: u8 = 1;
    pub const GET_MODE: u8 = 2;
    pub const SET_CURRENT_PROFILE: u8 = 3;
    pub const GET_CURRENT_PROFILE: u8 = 4;
    pub const MEMORY_READ: u8 = 5;
    pub const MEMORY_ADDR_WRITE: u8 = 6;
    pub const MEMORY_WRITE: u8 = 7;
    pub const MEMORY_WRITE_END: u8 = 8;
    pub const GET_CURRENT_DPI_INDEX: u8 = 11;
    pub const SET_CURRENT_DPI_INDEX: u8 = 12;
    /// モード値。
    pub const MODE_ONBOARD: u8 = 1;
    pub const MODE_HOST: u8 = 2;
    /// ユーザー領域のディレクトリ / ROM (工場出荷) 領域のディレクトリ。
    pub const USER_DIR: u16 = 0x0000;
    pub const ROM_DIR: u16 = 0x0100;
}

/// プロファイルセクタ内のオフセット (profile format 1–3 共通)。
mod pf {
    pub const REPORT_RATE: usize = 0; // ms (1000 / Hz)
    pub const DEFAULT_DPI: usize = 1; // 起動時の DPI index
    pub const SWITCHED_DPI: usize = 2; // DPI シフト (スナイパー) の index
    pub const DPI: usize = 3; // u16 LE × 5
    pub const DPI_COUNT: usize = 5;
    pub const BUTTONS: usize = 0x20; // 4B × 16
    pub const MAX_BUTTONS: usize = 16;
    pub const LEDS: usize = 0xD0; // 11B × 2 ゾーン
    pub const LED_LEN: usize = 11;
}

/// 対応しているプロファイル形式 (libratbag と同じ範囲)。これ以外は書き込まない。
const PROFILE_FORMATS: &[u8] = &[1, 2, 3];

// ---------------------------------------------------------------- 純粋関数 (単体テスト対象)

/// CRC-CCITT (poly 0x1021, init 0xFFFF)。プロファイルセクタ末尾 2B に BE で入る。
pub fn crc_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// セクタの CRC が正しいか。
pub fn sector_valid(data: &[u8]) -> bool {
    if data.len() < 3 {
        return false;
    }
    let n = data.len() - 2;
    crc_ccitt(&data[..n]) == u16::from_be_bytes([data[n], data[n + 1]])
}

/// セクタ末尾 2B に CRC を入れる。
pub fn seal_sector(data: &mut [u8]) {
    let n = data.len() - 2;
    let c = crc_ccitt(&data[..n]).to_be_bytes();
    data[n] = c[0];
    data[n + 1] = c[1];
}

/// 0x2201 getSensorDpiList の応答 (先頭 sensor index を除いた部分) → (min, max, step)。
/// 値の並び (u16 BE): 単独値、または `0xE000|step` を挟んだ範囲 `lo, E0xx, hi`。0 で終端。
pub fn parse_dpi_list(p: &[u8]) -> Option<(u32, u32, u32)> {
    let mut vals: Vec<u32> = Vec::new();
    let mut step: Option<u32> = None;
    let mut i = 0;
    while i + 1 < p.len() {
        let v = u16::from_be_bytes([p[i], p[i + 1]]) as u32;
        if v == 0 {
            break;
        }
        if v >= 0xE000 {
            let s = v - 0xE000;
            if s > 0 {
                step = Some(step.map_or(s, |o: u32| o.min(s)));
            }
        } else {
            vals.push(v);
        }
        i += 2;
    }
    let min = *vals.iter().min()?;
    let max = *vals.iter().max()?;
    let step = step.unwrap_or_else(|| {
        // 範囲指定が無い (離散値の列) ときは隣接差の最小値。
        let mut v = vals.clone();
        v.sort_unstable();
        v.windows(2)
            .map(|w| w[1] - w[0])
            .filter(|&d| d > 0)
            .min()
            .unwrap_or(50)
    });
    Some((min, max, step.max(1)))
}

/// 電圧 (mV) → 残量 % (Logitech のリチウムイオン放電カーブの近似、線形補間)。
pub fn battery_from_voltage(mv: u16) -> u8 {
    const T: &[(u16, u8)] = &[
        (4186, 100),
        (4067, 90),
        (3989, 80),
        (3922, 70),
        (3859, 60),
        (3811, 50),
        (3778, 40),
        (3751, 30),
        (3717, 20),
        (3671, 10),
        (3646, 5),
        (3579, 2),
        (3500, 0),
    ];
    if mv >= T[0].0 {
        return 100;
    }
    for w in T.windows(2) {
        let (hv, hp) = w[0];
        let (lv, lp) = w[1];
        if mv >= lv {
            let f = (mv - lv) as f32 / (hv - lv) as f32;
            return (lp as f32 + f * (hp - lp) as f32).round() as u8;
        }
    }
    0
}

// ---- ボタン割り当て (4B)
//
// [0x80, 0x01, btn_hi, btn_lo]   マウスボタン (ビットマスク BE: bit0=左 …)
// [0x80, 0x02, mods, usage]      キーボード
// [0x80, 0x03, usage_hi, lo]     Consumer (メディア)
// [0x90, special, arg, 0]        特殊 (DPI 切替・プロファイル切替・横スクロール)
// [0x00, …]                      マクロ (未対応 → Unsupported で往復)
// [0xFF, 0xFF, 0xFF, 0xFF]       未割り当て

mod special {
    pub const WHEEL_LEFT: u8 = 0x01;
    pub const WHEEL_RIGHT: u8 = 0x02;
    pub const DPI_UP: u8 = 0x03;
    pub const DPI_DOWN: u8 = 0x04;
    pub const DPI_CYCLE: u8 = 0x05;
    pub const PROFILE_CYCLE: u8 = 0x07;
    pub const DPI_SHIFT: u8 = 0x09;
}

fn media_usage(m: MediaFn) -> u16 {
    match m {
        MediaFn::PlayPause => 0x00CD,
        MediaFn::Stop => 0x00B7,
        MediaFn::Next => 0x00B5,
        MediaFn::Prev => 0x00B6,
        MediaFn::VolumeUp => 0x00E9,
        MediaFn::VolumeDown => 0x00EA,
        MediaFn::Mute => 0x00E2,
        MediaFn::Calculator => 0x0192,
        MediaFn::MyComputer => 0x0194,
        MediaFn::Browser => 0x0196,
        MediaFn::Mail => 0x018A,
        MediaFn::BrightnessUp => 0x006F,
        MediaFn::BrightnessDown => 0x0070,
        MediaFn::Refresh => 0x0227,
    }
}

fn media_from_usage(u: u16) -> Option<MediaFn> {
    use MediaFn::*;
    [
        PlayPause,
        Stop,
        Next,
        Prev,
        VolumeUp,
        VolumeDown,
        Mute,
        Calculator,
        MyComputer,
        Browser,
        Mail,
        BrightnessUp,
        BrightnessDown,
        Refresh,
    ]
    .into_iter()
    .find(|m| media_usage(*m) == u)
}

/// 4B → 割り当て。`shift_dpi` は DPI シフト先の値 (表示用)。
pub fn decode_button(b: [u8; 4], shift_dpi: u32) -> ButtonAction {
    match b[0] {
        0x80 => match b[1] {
            0x01 => {
                let mask = u16::from_be_bytes([b[2], b[3]]);
                match mask {
                    0 => ButtonAction::Disabled,
                    0x0001 => ButtonAction::Mouse(MouseFn::Left),
                    0x0002 => ButtonAction::Mouse(MouseFn::Right),
                    0x0004 => ButtonAction::Mouse(MouseFn::Middle),
                    0x0008 => ButtonAction::Mouse(MouseFn::Back),
                    0x0010 => ButtonAction::Mouse(MouseFn::Forward),
                    _ => ButtonAction::Unsupported {
                        ty: b[0],
                        params: b[1..].to_vec(),
                    },
                }
            }
            0x02 => ButtonAction::Key(KeyStroke {
                mods: b[2],
                usage: b[3] as u16,
            }),
            0x03 => match media_from_usage(u16::from_be_bytes([b[2], b[3]])) {
                Some(m) => ButtonAction::Media(m),
                None => ButtonAction::Unsupported {
                    ty: b[0],
                    params: b[1..].to_vec(),
                },
            },
            _ => ButtonAction::Unsupported {
                ty: b[0],
                params: b[1..].to_vec(),
            },
        },
        0x90 => match b[1] {
            special::WHEEL_LEFT => ButtonAction::Mouse(MouseFn::ScrollLeft),
            special::WHEEL_RIGHT => ButtonAction::Mouse(MouseFn::ScrollRight),
            special::DPI_UP => ButtonAction::Dpi(DpiFn::Up),
            special::DPI_DOWN => ButtonAction::Dpi(DpiFn::Down),
            special::DPI_CYCLE => ButtonAction::Dpi(DpiFn::Cycle),
            special::PROFILE_CYCLE => ButtonAction::ProfileCycle,
            special::DPI_SHIFT => ButtonAction::Dpi(DpiFn::Shift(shift_dpi)),
            _ => ButtonAction::Unsupported {
                ty: b[0],
                params: b[1..].to_vec(),
            },
        },
        0xFF if b == [0xFF; 4] => ButtonAction::Disabled,
        _ => ButtonAction::Unsupported {
            ty: b[0],
            params: b[1..].to_vec(),
        },
    }
}

/// 割り当て → 4B。
pub fn encode_button(a: &ButtonAction) -> HubResult<[u8; 4]> {
    let mouse = |m: u16| {
        let [h, l] = m.to_be_bytes();
        [0x80, 0x01, h, l]
    };
    Ok(match a {
        // 「何もしない」は空のマウスボタン (0xFF.. は未プログラム扱いで既定動作に戻る機種がある)。
        ButtonAction::Disabled => mouse(0),
        ButtonAction::Mouse(m) => match m {
            MouseFn::Left => mouse(0x0001),
            MouseFn::Right => mouse(0x0002),
            MouseFn::Middle => mouse(0x0004),
            MouseFn::Back => mouse(0x0008),
            MouseFn::Forward => mouse(0x0010),
            MouseFn::ScrollLeft => [0x90, special::WHEEL_LEFT, 0, 0],
            MouseFn::ScrollRight => [0x90, special::WHEEL_RIGHT, 0, 0],
            other => {
                return Err(HubError::Invalid(format!(
                    "このマウスには割り当てられません: {other:?}"
                )))
            }
        },
        ButtonAction::Key(k) => {
            if k.usage > 0xFF {
                return Err(HubError::Invalid(format!("キーコード {:#x}", k.usage)));
            }
            [0x80, 0x02, k.mods, k.usage as u8]
        }
        ButtonAction::Media(m) => {
            let [h, l] = media_usage(*m).to_be_bytes();
            [0x80, 0x03, h, l]
        }
        ButtonAction::Dpi(d) => {
            let s = match d {
                DpiFn::Cycle => special::DPI_CYCLE,
                DpiFn::Up => special::DPI_UP,
                DpiFn::Down => special::DPI_DOWN,
                DpiFn::Shift(_) => special::DPI_SHIFT,
            };
            [0x90, s, 0, 0]
        }
        ButtonAction::ProfileCycle => [0x90, special::PROFILE_CYCLE, 0, 0],
        ButtonAction::Unsupported { ty, params } => {
            let mut b = [*ty, 0, 0, 0];
            for (i, v) in params.iter().take(3).enumerate() {
                b[1 + i] = *v;
            }
            b
        }
        other => {
            return Err(HubError::Invalid(format!(
                "このマウスには割り当てられません: {:?}",
                other.kind()
            )))
        }
    })
}

// ---- LED (11B)
//
// [mode, …]  mode 0x00=消灯 / 0x01=固定 [r,g,b] / 0x03=サイクル [_×5, period BE, intensity]
//            / 0x0A=呼吸 [r,g,b, period BE, waveform, intensity]

mod led {
    pub const OFF: u8 = 0x00;
    pub const FIXED: u8 = 0x01;
    pub const CYCLE: u8 = 0x03;
    pub const BREATH: u8 = 0x0A;
}

/// speed 1..=10 ⇔ 周期 ms。
fn speed_to_period(speed: u8) -> u16 {
    11_000u16.saturating_sub(speed.clamp(1, 10) as u16 * 1000)
}

fn period_to_speed(p: u16) -> u8 {
    (((11_000i32 - p as i32) as f32 / 1000.).round() as i32).clamp(1, 10) as u8
}

/// intensity 0 は「100%」の意味。
fn intensity_to_brightness(i: u8) -> u8 {
    if i == 0 {
        100
    } else {
        i.min(100)
    }
}

pub fn decode_led(b: &[u8]) -> LightingState {
    let mut s = LightingState {
        on: b[0] != led::OFF,
        effect: b[0],
        color: Rgb::new(255, 255, 255),
        brightness: 100,
        speed: 5,
        direction: 0,
        per_key: BTreeMap::new(),
        zone_on: vec![],
        palette: vec![],
        palette_sel: 0,
        color_correction: None,
    };
    match b[0] {
        led::FIXED => s.color = Rgb::new(b[1], b[2], b[3]),
        led::CYCLE => {
            s.speed = period_to_speed(u16::from_be_bytes([b[6], b[7]]));
            s.brightness = intensity_to_brightness(b[8]);
        }
        led::BREATH => {
            s.color = Rgb::new(b[1], b[2], b[3]);
            s.speed = period_to_speed(u16::from_be_bytes([b[4], b[5]]));
            s.brightness = intensity_to_brightness(b[7]);
        }
        led::OFF => s.effect = led::FIXED, // 消灯中は「固定」を選択状態として見せる
        _ => {}
    }
    s
}

pub fn encode_led(s: &LightingState) -> [u8; pf::LED_LEN] {
    let mut b = [0u8; pf::LED_LEN];
    if !s.on {
        return b;
    }
    let Rgb { r, g, b: bl } = s.color;
    match s.effect {
        led::CYCLE => {
            b[0] = led::CYCLE;
            b[6..8].copy_from_slice(&speed_to_period(s.speed).to_be_bytes());
            b[8] = s.brightness.clamp(1, 100);
        }
        led::BREATH => {
            b[0] = led::BREATH;
            b[1..4].copy_from_slice(&[r, g, bl]);
            b[4..6].copy_from_slice(&speed_to_period(s.speed).to_be_bytes());
            b[6] = 0; // waveform 既定
            b[7] = s.brightness.clamp(1, 100);
        }
        _ => {
            // 固定色は intensity を持たないので明るさを RGB に掛ける。
            let k = s.brightness.clamp(1, 100) as u16;
            let sc = |v: u8| ((v as u16 * k) / 100) as u8;
            b[0] = led::FIXED;
            b[1..4].copy_from_slice(&[sc(r), sc(g), sc(bl)]);
        }
    }
    b
}

/// ディレクトリセクタ → (セクタ番号, 有効) の列。
pub fn parse_directory(d: &[u8], max: usize) -> Vec<(u16, bool)> {
    let mut out = Vec::new();
    for e in d.chunks_exact(4).take(max) {
        let sector = u16::from_be_bytes([e[0], e[1]]);
        if sector == 0xFFFF {
            break;
        }
        out.push((sector, e[2] != 0));
    }
    out
}

// ---------------------------------------------------------------- ドライバ

#[derive(Clone, Copy, Debug)]
struct OnboardInfo {
    index: u8,
    format: u8,
    profile_count: u8,
    button_count: u8,
    sector_size: u16,
}

#[derive(Default)]
struct Features {
    /// feature ID → index。
    map: HashMap<u16, u8>,
    /// 機能探索が終わったか (寝ている間に開くと未完了のまま)。
    ready: bool,
    dpi: Option<(u32, u32, u32)>,
    /// 対応ポーリング周期 (ms)。
    rates_ms: Vec<u8>,
    onboard: Option<OnboardInfo>,
    led_zones: u8,
}

type Events = Option<Arc<dyn Fn(HubEvent) + Send + Sync>>;

pub struct HidppDriver {
    dev: Mutex<hidapi::HidDevice>,
    dev_index: u8,
    info: Mutex<DeviceInfo>,
    f: Mutex<Features>,
    events: Events,
}

pub fn open(dev: hidapi::HidDevice, info: DeviceInfo, events: Events) -> HubResult<HidppDriver> {
    let dev_index = match info.transport {
        Transport::Wired => 0xFF,
        _ => 0x01,
    };
    let d = HidppDriver {
        dev: Mutex::new(dev),
        dev_index,
        info: Mutex::new(info),
        f: Mutex::new(Features::default()),
        events,
    };
    // 寝ていても開くこと自体は成功させる (覚醒後に ensure_ready が探索する)。
    let _ = d.ensure_ready();
    Ok(d)
}

impl HidppDriver {
    fn wireless(&self) -> bool {
        self.dev_index != 0xFF
    }

    fn emit_info(&self) {
        if let Some(ev) = &self.events {
            let i = self.info.lock().unwrap_or_else(|p| p.into_inner()).clone();
            ev(HubEvent::Info(i.id.clone(), Box::new(i)));
        }
    }

    fn set_asleep(&self, asleep: bool) {
        let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner());
        if !self.wireless() || i.asleep == asleep {
            return;
        }
        i.asleep = asleep;
        drop(i);
        self.emit_info();
    }

    /// HID++ 2.0 の 1 往復。戻り値は params 16B。
    fn call(&self, feat_index: u8, func: u8, params: &[u8]) -> HubResult<[u8; 16]> {
        let fsw = (func << 4) | SW_ID;
        let mut req = [0u8; LONG_LEN];
        req[0] = REPORT_LONG;
        req[1] = self.dev_index;
        req[2] = feat_index;
        req[3] = fsw;
        let n = params.len().min(16);
        req[4..4 + n].copy_from_slice(&params[..n]);

        let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
        // 読み残しの通知を捨てる。
        let mut junk = [0u8; 64];
        while matches!(dev.read_timeout(&mut junk, 0), Ok(n) if n > 0) {}

        let timeout = if self.wireless() { 1200 } else { 600 };
        for attempt in 0..2 {
            dev.write(&req)
                .map_err(|e| HubError::Io(format!("hid++ write: {e}")))?;
            let deadline = Instant::now() + Duration::from_millis(timeout);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                let mut r = [0u8; 64];
                let got = dev
                    .read_timeout(&mut r, left.as_millis().min(200) as i32)
                    .map_err(|e| HubError::Io(format!("hid++ read: {e}")))?;
                if got < 7 || r[1] != self.dev_index {
                    continue;
                }
                // HID++ 1.0 のエラー (Linux hidraw など同一ノードで見えるとき)。
                if r[0] == REPORT_SHORT && r[2] == 0x8F && r[3] == feat_index {
                    drop(dev);
                    self.set_asleep(true);
                    return Err(HubError::Protocol(format!(
                        "hid++: デバイスが応答しません (err {:#04x})",
                        r[5]
                    )));
                }
                if r[0] != REPORT_LONG || got < LONG_LEN {
                    continue;
                }
                if r[2] == feat_index && r[3] == fsw {
                    drop(dev);
                    self.set_asleep(false);
                    let mut out = [0u8; 16];
                    out.copy_from_slice(&r[4..20]);
                    return Ok(out);
                }
                if r[2] == 0xFF && r[3] == feat_index && r[4] == fsw {
                    drop(dev);
                    self.set_asleep(false);
                    return Err(HubError::Protocol(format!(
                        "hid++ feature {feat_index:#04x}/fn{func}: {}",
                        err_name(r[5])
                    )));
                }
            }
            if attempt == 0 {
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        drop(dev);
        self.set_asleep(true);
        Err(HubError::Protocol(
            "hid++: 応答がありません (マウスがスリープ中か電源オフ)".into(),
        ))
    }

    fn feature(&self, id: u16) -> Option<u8> {
        self.f
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .map
            .get(&id)
            .copied()
    }

    fn need(&self, id: u16, what: &'static str) -> HubResult<u8> {
        self.feature(id).ok_or(HubError::Unsupported(what))
    }

    /// Root.getFeature: feature ID → index (0 なら非対応)。
    fn lookup(&self, id: u16) -> HubResult<Option<u8>> {
        let [h, l] = id.to_be_bytes();
        let r = self.call(0, 0, &[h, l])?;
        Ok((r[0] != 0 || id == feat::ROOT).then_some(r[0]))
    }

    /// 機能探索 (1 回だけ)。寝ていたら Err で、次の呼び出しで再試行する。
    fn ensure_ready(&self) -> HubResult<()> {
        if self.f.lock().unwrap_or_else(|p| p.into_inner()).ready {
            return Ok(());
        }
        // ping (Root fn1) — HID++ 2.0 であることの確認。
        let p = self.call(0, 1, &[0, 0, 0x5A])?;
        if p[0] < 2 {
            return Err(HubError::Unsupported("HID++ 1.0 デバイス"));
        }
        let mut map = HashMap::new();
        for id in [
            feat::FW_VERSION,
            feat::DEVICE_NAME,
            feat::BATTERY_STATUS,
            feat::BATTERY_VOLTAGE,
            feat::UNIFIED_BATTERY,
            feat::ADJUSTABLE_DPI,
            feat::REPORT_RATE,
            feat::COLOR_LED_EFFECTS,
            feat::ONBOARD_PROFILES,
        ] {
            if let Some(ix) = self.lookup(id)? {
                map.insert(id, ix);
            }
        }
        {
            let mut f = self.f.lock().unwrap_or_else(|p| p.into_inner());
            f.map = map;
        }

        // DPI 範囲 (0x2201 fn1 getSensorDpiList(sensor 0))。
        let dpi = match self.feature(feat::ADJUSTABLE_DPI) {
            Some(ix) => self
                .call(ix, 1, &[0])
                .ok()
                .and_then(|r| parse_dpi_list(&r[1..])),
            None => None,
        };
        // ポーリング (0x8060 fn0 getReportRateList: bit n = (n+1)ms)。
        let rates_ms = match self.feature(feat::REPORT_RATE) {
            Some(ix) => match self.call(ix, 0, &[]) {
                Ok(r) => (0..8)
                    .filter(|b| r[0] & (1 << b) != 0)
                    .map(|b| b + 1)
                    .collect(),
                Err(_) => vec![1, 2, 4, 8],
            },
            None => vec![1, 2, 4, 8],
        };
        // オンボードプロファイル (0x8100 fn0 getInfo)。
        let onboard = match self.feature(feat::ONBOARD_PROFILES) {
            Some(ix) => self.call(ix, ob::GET_INFO, &[]).ok().map(|r| OnboardInfo {
                index: ix,
                format: r[1],
                profile_count: r[3].max(1),
                button_count: r[5].min(pf::MAX_BUTTONS as u8),
                sector_size: u16::from_be_bytes([r[7], r[8]]),
            }),
            None => None,
        };
        // LED ゾーン数 (0x8070 fn0 getInfo → zone count)。
        let led_zones = match self.feature(feat::COLOR_LED_EFFECTS) {
            Some(ix) => self.call(ix, 0, &[]).map(|r| r[0].min(2)).unwrap_or(1),
            None => 0,
        };

        // 名前・ファームウェア。
        let name = self.read_name().ok();
        let fw = self.read_firmware().ok();

        {
            let mut f = self.f.lock().unwrap_or_else(|p| p.into_inner());
            f.dpi = dpi;
            f.rates_ms = rates_ms;
            f.onboard = onboard;
            f.led_zones = led_zones;
            f.ready = true;
        }
        {
            let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(n) = name.filter(|n| !n.is_empty()) {
                i.name = format!("Logicool {n}");
                i.model = model_id(&n);
            }
            i.firmware = fw;
            i.asleep = false;
        }
        // 名前/機種が確定 → UI は Job::Open で caps を取り直す。
        self.emit_info();

        // G HUB が「オンボードメモリ OFF」(ホストモード) にしたままだと
        // 本体の設定が効かない。置き換えの要なので開いた時点でオンボードへ戻す。
        if let Some(o) = onboard {
            if let Ok(r) = self.call(o.index, ob::GET_MODE, &[]) {
                if r[0] == ob::MODE_HOST {
                    let _ = self.call(o.index, ob::SET_MODE, &[ob::MODE_ONBOARD]);
                }
            }
        }
        Ok(())
    }

    fn read_name(&self) -> HubResult<String> {
        let ix = self.need(feat::DEVICE_NAME, "device name")?;
        let len = self.call(ix, 0, &[])?[0] as usize;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let r = self.call(ix, 1, &[out.len() as u8])?;
            let take = (len - out.len()).min(16);
            out.extend_from_slice(&r[..take]);
        }
        Ok(String::from_utf8_lossy(&out)
            .trim_end_matches('\0')
            .trim()
            .to_string())
    }

    fn read_firmware(&self) -> HubResult<String> {
        let ix = self.need(feat::FW_VERSION, "fw version")?;
        let count = self.call(ix, 0, &[])?[0];
        for e in 0..count {
            let r = self.call(ix, 1, &[e])?;
            // type 0 = メインアプリケーション。
            if r[0] & 0x0F == 0 {
                let prefix = String::from_utf8_lossy(&r[1..4]).trim().to_string();
                return Ok(format!(
                    "{prefix} {:02X}.{:02X}.B{:02X}{:02X}",
                    r[4], r[5], r[6], r[7]
                ));
            }
        }
        Err(HubError::Protocol("fw entity not found".into()))
    }

    // ---- オンボードメモリ

    fn onboard(&self) -> HubResult<OnboardInfo> {
        self.ensure_ready()?;
        let o = self
            .f
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .onboard
            .ok_or(HubError::Unsupported("onboard profiles"))?;
        Ok(o)
    }

    fn writable(&self, o: &OnboardInfo) -> HubResult<()> {
        if !PROFILE_FORMATS.contains(&o.format) || o.sector_size < 0xF0 {
            return Err(HubError::Unsupported(
                "このプロファイル形式への書き込み (安全のため読み取りのみ)",
            ));
        }
        Ok(())
    }

    fn read_sector(&self, o: &OnboardInfo, sector: u16) -> HubResult<Vec<u8>> {
        let size = o.sector_size as usize;
        let mut out = vec![0u8; size];
        let [sh, sl] = sector.to_be_bytes();
        let mut off = 0usize;
        while off < size {
            // セクタ末尾を越えて読むとエラーになるので、最後は size-16 から読む。
            let at = off.min(size.saturating_sub(16));
            let [oh, ol] = (at as u16).to_be_bytes();
            let r = self.call(o.index, ob::MEMORY_READ, &[sh, sl, oh, ol])?;
            let end = (at + 16).min(size);
            out[at..end].copy_from_slice(&r[..end - at]);
            off = end;
        }
        Ok(out)
    }

    fn write_sector(&self, o: &OnboardInfo, sector: u16, data: &[u8]) -> HubResult<()> {
        self.writable(o)?;
        if sector >= ob::ROM_DIR {
            return Err(HubError::Invalid("ROM 領域には書き込めません".into()));
        }
        let size = o.sector_size as usize;
        if data.len() != size || !sector_valid(data) {
            return Err(HubError::Invalid("セクタの CRC が不正".into()));
        }
        let [sh, sl] = sector.to_be_bytes();
        let [lh, ll] = (size as u16).to_be_bytes();
        self.call(o.index, ob::MEMORY_ADDR_WRITE, &[sh, sl, 0, 0, lh, ll])?;
        for chunk in data.chunks(16) {
            let mut p = [0u8; 16];
            p[..chunk.len()].copy_from_slice(chunk);
            self.call(o.index, ob::MEMORY_WRITE, &p)?;
        }
        self.call(o.index, ob::MEMORY_WRITE_END, &[])?;
        Ok(())
    }

    /// 有効なディレクトリ: (ユーザー領域か, エントリ列)。
    fn directory(&self, o: &OnboardInfo) -> HubResult<(bool, Vec<(u16, bool)>)> {
        let max = o.profile_count as usize;
        let user = self.read_sector(o, ob::USER_DIR)?;
        if sector_valid(&user) {
            let d = parse_directory(&user, max);
            if !d.is_empty() {
                return Ok((true, d));
            }
        }
        let rom = self.read_sector(o, ob::ROM_DIR)?;
        Ok((false, parse_directory(&rom, max)))
    }

    /// ユーザー領域が空 (工場出荷のまま ROM で動いている) なら、
    /// ROM のプロファイルをユーザー領域へ複製してディレクトリを作る。
    fn ensure_user_profiles(&self, o: &OnboardInfo) -> HubResult<Vec<(u16, bool)>> {
        let (user, dir) = self.directory(o)?;
        if user {
            return Ok(dir);
        }
        self.writable(o)?;
        if dir.is_empty() {
            return Err(HubError::Protocol(
                "ROM プロファイルが見つかりません".into(),
            ));
        }
        let mut new_dir = Vec::new();
        for (i, (rom_sector, enabled)) in dir.iter().enumerate() {
            let mut data = self.read_sector(o, *rom_sector)?;
            seal_sector(&mut data);
            let sector = (i + 1) as u16;
            self.write_sector(o, sector, &data)?;
            new_dir.push((sector, *enabled));
        }
        let mut d = vec![0u8; o.sector_size as usize];
        for (i, (s, en)) in new_dir.iter().enumerate() {
            d[i * 4..i * 4 + 2].copy_from_slice(&s.to_be_bytes());
            d[i * 4 + 2] = *en as u8;
        }
        let end = new_dir.len() * 4;
        d[end..end + 4].copy_from_slice(&[0xFF, 0xFF, 0, 0]);
        seal_sector(&mut d);
        self.write_sector(o, ob::USER_DIR, &d)?;
        Ok(new_dir)
    }

    /// 現在のプロファイル番号 (0 始まり)。
    fn current_profile(&self, o: &OnboardInfo) -> HubResult<usize> {
        let r = self.call(o.index, ob::GET_CURRENT_PROFILE, &[])?;
        Ok((r[1] as usize).saturating_sub(1))
    }

    /// 現在のプロファイルのセクタ内容 (読み取り)。
    fn current_sector(&self, o: &OnboardInfo) -> HubResult<(usize, Vec<u8>)> {
        let (_, dir) = self.directory(o)?;
        let cur = self.current_profile(o)?.min(dir.len().saturating_sub(1));
        let sector = dir
            .get(cur)
            .map(|e| e.0)
            .ok_or_else(|| HubError::Protocol("プロファイルがありません".into()))?;
        Ok((cur, self.read_sector(o, sector)?))
    }

    /// 現在のプロファイルを読み→書き換え→保存→再選択 (即反映)。
    fn edit_profile(
        &self,
        f: impl FnOnce(&mut [u8], &OnboardInfo) -> HubResult<()>,
    ) -> HubResult<()> {
        let o = self.onboard()?;
        self.writable(&o)?;
        let dir = self.ensure_user_profiles(&o)?;
        let cur = self.current_profile(&o)?.min(dir.len() - 1);
        let sector = dir[cur].0;
        let mut data = self.read_sector(&o, sector)?;
        f(&mut data, &o)?;
        seal_sector(&mut data);
        self.write_sector(&o, sector, &data)?;
        // 書いたプロファイルを選び直すと即座に反映される。
        self.call(o.index, ob::SET_CURRENT_PROFILE, &[0, (cur + 1) as u8])?;
        Ok(())
    }

    fn dpi_range(&self) -> (u32, u32, u32) {
        self.f
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .dpi
            .unwrap_or((100, 25600, 50))
    }
}

fn err_name(code: u8) -> String {
    match code {
        1 => "unknown".into(),
        2 => "invalid argument".into(),
        3 => "out of range".into(),
        4 => "hardware error".into(),
        5 => "logitech internal".into(),
        6 => "invalid feature index".into(),
        7 => "invalid function".into(),
        8 => "busy".into(),
        9 => "unsupported".into(),
        c => format!("error {c:#04x}"),
    }
}

/// デバイス名 → model 識別子 ("PRO Wireless" → "logi-pro-wireless")。
fn model_id(name: &str) -> String {
    let s: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("logi-{}", s.trim_matches('-'))
}

/// ボタン表示名 (PRO Wireless の並び。他機種は番号で出す)。
fn button_names(count: usize, model: &str) -> Vec<ButtonSlot> {
    let pro: &[&str] = &[
        "左クリック",
        "右クリック",
        "ホイールクリック",
        "戻る (左サイド後)",
        "進む (左サイド前)",
        "右サイド後",
        "右サイド前",
        "DPI ボタン (底面)",
    ];
    let generic: &[&str] = &[
        "左クリック",
        "右クリック",
        "ホイールクリック",
        "戻る",
        "進む",
    ];
    let names = if model.contains("pro-wireless") {
        pro
    } else {
        generic
    };
    (0..count)
        .map(|i| ButtonSlot {
            name: names
                .get(i)
                .map(|s| (*s).to_string())
                .unwrap_or_else(|| format!("ボタン {}", i + 1)),
        })
        .collect()
}

fn hz_label(hz: i64) -> String {
    if hz >= 1000 {
        format!("{}K", hz / 1000)
    } else {
        hz.to_string()
    }
}

impl DeviceDriver for HidppDriver {
    fn info(&self) -> DeviceInfo {
        let _ = self.ensure_ready();
        if let Ok(b) = self.battery() {
            self.info.lock().unwrap_or_else(|p| p.into_inner()).battery = Some(b);
        }
        self.info.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn caps(&self) -> Capabilities {
        use section::*;
        let _ = self.ensure_ready();
        let f = self.f.lock().unwrap_or_else(|p| p.into_inner());
        let model = self
            .info
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .model
            .clone();
        let mut c = Capabilities::empty();
        c.battery = f.map.contains_key(&feat::BATTERY_VOLTAGE)
            || f.map.contains_key(&feat::UNIFIED_BATTERY)
            || f.map.contains_key(&feat::BATTERY_STATUS);
        if !f.ready {
            // 寝ている間に開いた: 起きたら Info → Job::Open で取り直される。
            return c;
        }
        let Some(o) = f.onboard else {
            // オンボードメモリが無い機種は対象外 (G HUB 常駐が前提の機種)。
            return c;
        };
        let writable = PROFILE_FORMATS.contains(&o.format) && o.sector_size >= 0xF0;

        let mut rates: Vec<ChoiceOpt> = f
            .rates_ms
            .iter()
            .map(|&ms| {
                let hz = 1000 / ms as i64;
                ChoiceOpt::new(hz, hz_label(hz))
            })
            .collect();
        rates.sort_by_key(|o| o.value);
        c.settings.push(
            SettingSpec::choice("polling", PERFORMANCE, "ポーリングレート", rates)
                .help("オンボードプロファイルに保存されます (G HUB 不要)"),
        );
        c.settings.push(
            SettingSpec::toggle("onboard_mode", SYSTEM, "オンボードメモリモード").help(
                "ON: マウス本体に保存した設定で動作 (推奨)。OFF はホストモード — \
                 G HUB などのソフトが常駐して制御する方式で、Device HUB では使いません",
            ),
        );
        if writable {
            c.actions.push(
                ActionSpec::new("rom_restore", SYSTEM, "このプロファイルを工場出荷時に戻す")
                    .help("ROM に残っている出荷時のプロファイルを現在のプロファイルへ書き戻します")
                    .danger("現在のプロファイルの DPI・ボタン・LED が出荷時の設定に戻ります"),
            );
        }
        if o.profile_count > 1 {
            c.profiles = Some(ProfileCaps {
                count: o.profile_count,
                renamable: false,
            });
        }
        let (min, max, step) = f.dpi.unwrap_or((100, 25600, 50));
        c.mouse = Some(MouseCaps {
            dpi_min: min,
            dpi_max: max,
            dpi_step: step,
            min_stages: 1,
            max_stages: pf::DPI_COUNT,
            xy_split: false,
            stage_colors: false,
            stage_toggle: false,
            buttons: button_names(o.button_count as usize, &model),
            button_kinds: vec![
                ButtonActionKind::Mouse,
                ButtonActionKind::Key,
                ButtonActionKind::Media,
                ButtonActionKind::Dpi,
                ButtonActionKind::ProfileCycle,
                ButtonActionKind::Disabled,
            ],
            require_left_click: true,
            macros: None,
            live_buttons: vec![],
            adaptive_dpi: None,
        });
        for z in 0..f.led_zones {
            c.lighting.push(LightArea {
                id: z,
                name: if f.led_zones == 1 {
                    "ロゴ LED".into()
                } else {
                    format!("LED ゾーン {}", z + 1)
                },
                effects: vec![
                    LightEffect::new(led::FIXED, "固定").color().still(),
                    LightEffect::new(led::BREATH, "呼吸").color(),
                    LightEffect::new(led::CYCLE, "カラーサイクル"),
                ],
                brightness_max: 100,
                speed_max: 10,
                directions: vec![],
                switchable: true,
                zones: vec![],
                per_key: false,
                direct_drive: false,
                live_readback: false,
            });
        }
        c
    }

    fn read(&self) -> HubResult<DeviceState> {
        let o = self.onboard()?;
        let mut settings = BTreeMap::new();
        if let Ok(r) = self.call(o.index, ob::GET_MODE, &[]) {
            settings.insert(
                "onboard_mode".into(),
                SettingValue::Bool(r[0] != ob::MODE_HOST),
            );
        }
        let (cur, data) = self.current_sector(&o)?;
        if !sector_valid(&data) {
            return Err(HubError::Protocol(
                "プロファイルの CRC が不正です (「工場出荷時に戻す」で修復できます)".into(),
            ));
        }
        let ms = data[pf::REPORT_RATE].max(1) as i64;
        settings.insert("polling".into(), SettingValue::Choice(1000 / ms));

        // DPI ステージ: 0 / 0xFFFF は空き。
        let mut stages = Vec::new();
        let mut map_idx = Vec::new(); // stages[i] → セクタ内の index
        for i in 0..pf::DPI_COUNT {
            let at = pf::DPI + i * 2;
            let v = u16::from_le_bytes([data[at], data[at + 1]]);
            if v != 0 && v != 0xFFFF {
                stages.push(DpiStage {
                    x: v as u32,
                    y: None,
                    color: Rgb::default(),
                    enabled: true,
                });
                map_idx.push(i);
            }
        }
        // 現在 DPI index はランタイム値 (本体のボタンで変わる) を優先。
        let runtime = self
            .call(o.index, ob::GET_CURRENT_DPI_INDEX, &[])
            .map(|r| r[0] as usize)
            .unwrap_or(data[pf::DEFAULT_DPI] as usize);
        let active = map_idx.iter().position(|&i| i == runtime).unwrap_or(0);
        let shift_ix = data[pf::SWITCHED_DPI] as usize;
        let shift_dpi = if shift_ix < pf::DPI_COUNT {
            u16::from_le_bytes([
                data[pf::DPI + shift_ix * 2],
                data[pf::DPI + shift_ix * 2 + 1],
            ]) as u32
        } else {
            0
        };

        let buttons = (0..o.button_count as usize)
            .map(|i| {
                let at = pf::BUTTONS + i * 4;
                decode_button(
                    [data[at], data[at + 1], data[at + 2], data[at + 3]],
                    shift_dpi,
                )
            })
            .collect();

        let zones = self.f.lock().unwrap_or_else(|p| p.into_inner()).led_zones;
        let mut lighting = BTreeMap::new();
        for z in 0..zones as usize {
            let at = pf::LEDS + z * pf::LED_LEN;
            lighting.insert(z as u8, decode_led(&data[at..at + pf::LED_LEN]));
        }

        Ok(DeviceState {
            profile: ProfileState {
                active: cur as u8,
                names: vec![String::new(); o.profile_count as usize],
            },
            settings,
            mouse: Some(MouseState {
                stages,
                active_stage: active,
                buttons,
                macros: vec![],
                adaptive: None,
            }),
            keyboard: None,
            lighting,
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        match change {
            Change::Setting { id, value } => match id.as_str() {
                "polling" => {
                    let hz = value.as_choice().max(1);
                    let ms = (1000 / hz).clamp(1, 255) as u8;
                    let ok = self
                        .f
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .rates_ms
                        .contains(&ms);
                    if !ok {
                        return Err(HubError::Invalid(format!("{hz}Hz には対応していません")));
                    }
                    self.edit_profile(|d, _| {
                        d[pf::REPORT_RATE] = ms;
                        Ok(())
                    })
                }
                "onboard_mode" => {
                    let o = self.onboard()?;
                    let m = if value.as_bool() {
                        ob::MODE_ONBOARD
                    } else {
                        ob::MODE_HOST
                    };
                    self.call(o.index, ob::SET_MODE, &[m]).map(|_| ())
                }
                _ => Err(HubError::Unsupported("setting")),
            },
            Change::Profile(p) => {
                let o = self.onboard()?;
                let (_, dir) = self.directory(&o)?;
                match dir.get(*p as usize) {
                    Some((_, true)) => self
                        .call(o.index, ob::SET_CURRENT_PROFILE, &[0, p + 1])
                        .map(|_| ()),
                    Some((_, false)) => Err(HubError::Invalid(
                        "このプロファイルは本体で無効になっています".into(),
                    )),
                    None => Err(HubError::Invalid("プロファイル番号".into())),
                }
            }
            Change::DpiStages { stages, active } => {
                let (min, max, step) = self.dpi_range();
                if stages.is_empty() || stages.len() > pf::DPI_COUNT {
                    return Err(HubError::Invalid("DPI ステージ数".into()));
                }
                let active = (*active).min(stages.len() - 1);
                self.edit_profile(|d, _| {
                    for i in 0..pf::DPI_COUNT {
                        let v = stages
                            .get(i)
                            .map(|s| {
                                let x = s.x.clamp(min, max);
                                (min + (x - min) / step * step) as u16
                            })
                            .unwrap_or(0);
                        d[pf::DPI + i * 2..pf::DPI + i * 2 + 2].copy_from_slice(&v.to_le_bytes());
                    }
                    d[pf::DEFAULT_DPI] = active as u8;
                    if d[pf::SWITCHED_DPI] as usize >= stages.len() {
                        d[pf::SWITCHED_DPI] = 0;
                    }
                    Ok(())
                })?;
                let o = self.onboard()?;
                let _ = self.call(o.index, ob::SET_CURRENT_DPI_INDEX, &[active as u8]);
                Ok(())
            }
            Change::ActiveStage(i) => {
                let o = self.onboard()?;
                // セクタ内 index へ変換 (空きステージを飛ばした並びなので)。
                let (_, data) = self.current_sector(&o)?;
                let slot = (0..pf::DPI_COUNT)
                    .filter(|k| {
                        let v =
                            u16::from_le_bytes([data[pf::DPI + k * 2], data[pf::DPI + k * 2 + 1]]);
                        v != 0 && v != 0xFFFF
                    })
                    .nth(*i)
                    .ok_or_else(|| HubError::Invalid("DPI ステージ".into()))?;
                self.call(o.index, ob::SET_CURRENT_DPI_INDEX, &[slot as u8])
                    .map(|_| ())
            }
            Change::Button { slot, action } => {
                let bytes = encode_button(action)?;
                let shift_target = match action {
                    ButtonAction::Dpi(DpiFn::Shift(v)) => Some(*v),
                    _ => None,
                };
                self.edit_profile(|d, o| {
                    if *slot >= o.button_count as usize {
                        return Err(HubError::Invalid("ボタン番号".into()));
                    }
                    let at = pf::BUTTONS + slot * 4;
                    d[at..at + 4].copy_from_slice(&bytes);
                    // DPI シフト: 指定 DPI に一番近いステージをシフト先にする。
                    if let Some(v) = shift_target {
                        let best = (0..pf::DPI_COUNT)
                            .filter_map(|k| {
                                let x = u16::from_le_bytes([
                                    d[pf::DPI + k * 2],
                                    d[pf::DPI + k * 2 + 1],
                                ]);
                                (x != 0 && x != 0xFFFF).then_some((k, (x as i64 - v as i64).abs()))
                            })
                            .min_by_key(|e| e.1);
                        if let Some((k, _)) = best {
                            d[pf::SWITCHED_DPI] = k as u8;
                        }
                    }
                    Ok(())
                })
            }
            Change::Lighting { area, state } => {
                let zones = self.f.lock().unwrap_or_else(|p| p.into_inner()).led_zones;
                if *area >= zones {
                    return Err(HubError::Invalid("LED ゾーン".into()));
                }
                let bytes = encode_led(state);
                self.edit_profile(|d, _| {
                    let at = pf::LEDS + *area as usize * pf::LED_LEN;
                    d[at..at + pf::LED_LEN].copy_from_slice(&bytes);
                    Ok(())
                })
            }
            _ => Err(HubError::Unsupported("change")),
        }
    }

    fn run_action(&self, id: &str, _file: Option<&std::path::Path>) -> HubResult<ActionOutcome> {
        match id {
            "rom_restore" => {
                let o = self.onboard()?;
                let rom = self.read_sector(&o, ob::ROM_DIR)?;
                let rom_dir = parse_directory(&rom, o.profile_count as usize);
                let cur = self.current_profile(&o)?;
                let (rom_sector, _) = *rom_dir
                    .get(cur)
                    .or(rom_dir.first())
                    .ok_or_else(|| HubError::Protocol("ROM プロファイルがありません".into()))?;
                let rom_data = self.read_sector(&o, rom_sector)?;
                self.edit_profile(|d, _| {
                    let n = d.len() - 2;
                    d[..n].copy_from_slice(&rom_data[..n]);
                    Ok(())
                })?;
                Ok(ActionOutcome {
                    message: Some("工場出荷時のプロファイルに戻しました".into()),
                    reload: true,
                })
            }
            _ => Err(HubError::Unsupported("action")),
        }
    }

    fn battery(&self) -> HubResult<Battery> {
        self.ensure_ready()?;
        if let Some(ix) = self.feature(feat::UNIFIED_BATTERY) {
            // fn1 getStatus → [soc%, level, status, external]
            let r = self.call(ix, 1, &[])?;
            return Ok(Battery {
                percent: Some(r[0].min(100)),
                charging: matches!(r[2], 1 | 2 | 4),
            });
        }
        if let Some(ix) = self.feature(feat::BATTERY_VOLTAGE) {
            // fn0 getBatteryInfo → [mV hi, mV lo, flags] (flags bit7 = 充電中)
            let r = self.call(ix, 0, &[])?;
            let mv = u16::from_be_bytes([r[0], r[1]]);
            return Ok(Battery {
                percent: Some(battery_from_voltage(mv)),
                charging: r[2] & 0x80 != 0,
            });
        }
        if let Some(ix) = self.feature(feat::BATTERY_STATUS) {
            // fn0 getBatteryLevelStatus → [level%, next%, status]
            let r = self.call(ix, 0, &[])?;
            return Ok(Battery {
                percent: Some(r[0].min(100)),
                charging: matches!(r[2], 1 | 2 | 4),
            });
        }
        Err(HubError::Unsupported("battery"))
    }
}

// ---------------------------------------------------------------- テスト

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known_vector() {
        // CRC-16/CCITT-FALSE の標準チェック値。
        assert_eq!(crc_ccitt(b"123456789"), 0x29B1);
    }

    #[test]
    fn seal_and_validate() {
        let mut s = vec![0x5Au8; 255];
        assert!(!sector_valid(&s));
        seal_sector(&mut s);
        assert!(sector_valid(&s));
        s[10] ^= 1;
        assert!(!sector_valid(&s));
    }

    #[test]
    fn dpi_list_range() {
        // HERO 25K: 100, step 50, 25600
        let p = [0x00, 0x64, 0xE0, 0x32, 0x64, 0x00, 0x00, 0x00];
        assert_eq!(parse_dpi_list(&p), Some((100, 25600, 50)));
        // 離散値
        let p = [0x01, 0x90, 0x03, 0x20, 0x06, 0x40, 0, 0];
        assert_eq!(parse_dpi_list(&p), Some((400, 1600, 400)));
        assert_eq!(parse_dpi_list(&[0, 0]), None);
    }

    #[test]
    fn battery_curve() {
        assert_eq!(battery_from_voltage(4200), 100);
        assert_eq!(battery_from_voltage(3811), 50);
        assert_eq!(battery_from_voltage(3400), 0);
        let mid = battery_from_voltage(3835);
        assert!((50..=60).contains(&mid));
    }

    #[test]
    fn buttons_roundtrip() {
        let cases = [
            ButtonAction::Mouse(MouseFn::Left),
            ButtonAction::Mouse(MouseFn::Forward),
            ButtonAction::Mouse(MouseFn::ScrollLeft),
            ButtonAction::Key(KeyStroke {
                mods: 0x01,
                usage: 0x06,
            }),
            ButtonAction::Media(MediaFn::VolumeUp),
            ButtonAction::Dpi(DpiFn::Cycle),
            ButtonAction::Dpi(DpiFn::Up),
            ButtonAction::ProfileCycle,
            ButtonAction::Disabled,
        ];
        for a in cases {
            let b = encode_button(&a).unwrap();
            assert_eq!(decode_button(b, 0), a, "{b:02x?}");
        }
        assert_eq!(
            decode_button(
                encode_button(&ButtonAction::Dpi(DpiFn::Shift(400))).unwrap(),
                400
            ),
            ButtonAction::Dpi(DpiFn::Shift(400))
        );
        assert_eq!(decode_button([0xFF; 4], 0), ButtonAction::Disabled);
        // マクロは解釈せずそのまま往復。
        let m = decode_button([0x00, 0x01, 0x00, 0x20], 0);
        assert_eq!(encode_button(&m).unwrap(), [0x00, 0x01, 0x00, 0x20]);
        assert!(encode_button(&ButtonAction::Mouse(MouseFn::DoubleClick)).is_err());
    }

    #[test]
    fn led_roundtrip() {
        let mut s = decode_led(&[0u8; 11]);
        assert!(!s.on);
        s.on = true;
        s.effect = led::BREATH;
        s.color = Rgb::new(10, 20, 30);
        s.speed = 7;
        s.brightness = 80;
        let back = decode_led(&encode_led(&s));
        assert_eq!(
            (
                back.on,
                back.effect,
                back.color,
                back.speed,
                back.brightness
            ),
            (true, led::BREATH, s.color, 7, 80)
        );
        s.effect = led::CYCLE;
        let back = decode_led(&encode_led(&s));
        assert_eq!(
            (back.effect, back.speed, back.brightness),
            (led::CYCLE, 7, 80)
        );
        s.effect = led::FIXED;
        s.brightness = 100;
        let back = decode_led(&encode_led(&s));
        assert_eq!((back.effect, back.color), (led::FIXED, s.color));
    }

    #[test]
    fn directory_parse() {
        let mut d = vec![0u8; 255];
        d[..12].copy_from_slice(&[0, 1, 1, 0, 0, 2, 0, 0, 0xFF, 0xFF, 0, 0]);
        assert_eq!(parse_directory(&d, 5), vec![(1, true), (2, false)]);
    }

    #[test]
    fn model_ids() {
        assert_eq!(model_id("PRO Wireless"), "logi-pro-wireless");
        assert_eq!(
            button_names(8, "logi-pro-wireless")[7].name,
            "DPI ボタン (底面)"
        );
        assert_eq!(button_names(6, "logi-g703")[5].name, "ボタン 6");
    }
}
