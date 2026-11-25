//! The frame rate and what the frame costs, drawn into the top left corner
//! of every frame shown.
//!
//! The window's caption has the frame rate, but a capture of the window's
//! picture (a stream) does not carry the caption. Here the numbers are part
//! of the picture: a small box of text copied onto the swap chain's back
//! buffer right after the title's frame, before it is presented. The
//! title's own frame is not touched, so saved frames and everything read
//! back from it are as before.
//!
//! Lines, updated once a second by the presenter's meter: frame rate and
//! milliseconds a frame; the processor time of this process (of all logical
//! processors) and the share of the frame spent waiting for the GPU's work
//! (the frame line's "on the GPU": near its busy share, not a counter of
//! the card's); this process's
//! memory and the system's; video memory used of the driver's budget; draws
//! a frame. From fxceless/victory-fork ca8829f (the one-line counter),
//! lines added here.
//!
//! The text is drawn on the CPU with a font of its own (5 by 7 dots a
//! glyph) into a buffer the card copies from; a buffer is made once for a
//! text and never written again, so a present still in the air reads what
//! it was recorded with (the last [`KEEP`] texts are kept). On by default;
//! `VICTORY_FPS_COUNTER=0` leaves the frame as the title drew it.

use std::collections::VecDeque;
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// The lines last measured; `None` before the first measurement.
static LINES: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// Texts' buffers kept: made about once a second, so the oldest is long
/// out of every present in the air.
const KEEP: usize = 16;

/// Whether the counter is drawn.
fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FPS_COUNTER").is_none_or(|v| v != "0"))
}

/// What the presenter's meter measured over its last second.
pub(crate) struct Frame {
    pub(crate) fps: f64,
    /// Milliseconds a frame.
    pub(crate) ms: f64,
    /// Milliseconds a frame spent waiting for the GPU's work.
    pub(crate) gpu_ms: f64,
    pub(crate) draws: f64,
}

/// What the system says of this process and its memory, sampled with the
/// frame's numbers.
#[derive(Default)]
struct Machine {
    /// Of all logical processors, percent.
    cpu: Option<f64>,
    /// This process's memory in RAM, and the system's used and total, GB.
    ram: Option<(f64, f64, f64)>,
    /// Video memory used and the driver's budget, GB.
    vram: Option<(f64, f64)>,
}

/// The frame just measured: its lines, with the machine's numbers now.
pub(crate) fn set(frame: Frame) {
    if !on() || !frame.fps.is_finite() || frame.fps < 0.0 {
        return;
    }
    let lines = lines(&frame, &machine());
    *LINES.lock().unwrap_or_else(|e| e.into_inner()) = Some(lines);
}

/// The box's lines (upper case: the font has no lower case).
fn lines(f: &Frame, m: &Machine) -> Vec<String> {
    let gb = |v: f64| format!("{v:.1}");
    let mut lines = vec![format!("{:.1} FPS  {:.1} MS", f.fps, f.ms)];
    let gpu = if f.ms > 0.0 { (f.gpu_ms / f.ms * 100.0).clamp(0.0, 100.0) } else { 0.0 };
    lines.push(match m.cpu {
        Some(cpu) => format!("CPU {cpu:.0}%  GPU {gpu:.0}%"),
        None => format!("GPU {gpu:.0}%"),
    });
    if let Some((own, used, total)) = m.ram {
        lines.push(format!("RAM {} GB  SYS {}/{} GB", gb(own), gb(used), total.round()));
    }
    if let Some((used, budget)) = m.vram {
        lines.push(format!("VRAM {}/{} GB", gb(used), gb(budget)));
    }
    lines.push(format!("DRAWS {:.0}", f.draws));
    lines
}

/// This process's processor use since the last sample, its memory and the
/// system's, and the video memory.
fn machine() -> Machine {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    Machine {
        cpu: cpu_percent(),
        ram: ram(),
        vram: crate::host::video_memory().map(|[(used, budget), _]| (used as f64 / GB, budget as f64 / GB)),
    }
}

/// This process's processor time since the last call, as a percent of all
/// logical processors over the same wall time; `None` the first time.
fn cpu_percent() -> Option<f64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    static LAST: Mutex<Option<(std::time::Instant, u64)>> = Mutex::new(None);
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: our own process; four out structures.
    if unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return None;
    }
    let ticks = |t: FILETIME| u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime);
    // 100 ns ticks.
    let busy = ticks(kernel) + ticks(user);
    let now = std::time::Instant::now();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let before = last.replace((now, busy))?;
    let wall = now.duration_since(before.0).as_secs_f64();
    let processors = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    (wall > 0.0).then(|| (busy.saturating_sub(before.1) as f64 / 1e7 / wall / processors * 100.0).clamp(0.0, 100.0))
}

/// This process's working set, and the system's memory in use and in all,
/// GB.
fn ram() -> Option<(f64, f64, f64)> {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    // SAFETY: zeroed plain structures with their sizes set, our own process.
    unsafe {
        let mut own: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        own.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut own, own.cb) == 0 {
            return None;
        }
        let mut system: MEMORYSTATUSEX = std::mem::zeroed();
        system.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut system) == 0 {
            return None;
        }
        let total = system.ullTotalPhys as f64;
        Some((own.WorkingSetSize as f64 / GB, (total - system.ullAvailPhys as f64) / GB, total / GB))
    }
}

/// A glyph's seven rows, five dots each (bit 4 is the left one).
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        '.' => [0, 0, 0, 0, 0, 0b01100, 0b01100],
        '/' => [0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000],
        '%' => [0b11001, 0b11010, 0b00010, 0b00100, 0b01000, 0b01011, 0b10011],
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'D' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010],
        'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        _ => [0; 7],
    }
}

/// Bytes a pixel of a swap chain format this layer presents in, or none.
fn pixel_bytes(format: DXGI_FORMAT) -> Option<usize> {
    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_R10G10B10A2_UNORM => Some(4),
        DXGI_FORMAT_R16G16B16A16_FLOAT => Some(8),
        _ => None,
    }
}

/// One pixel of `format`: of the text (`lit`) or of the box behind it.
fn pixel(format: DXGI_FORMAT, lit: bool, out: &mut [u8]) {
    // The text: a bright yellow. The box: nearly black.
    let (r, g, b): (f32, f32, f32) = if lit { (1.0, 0.92, 0.16) } else { (0.03, 0.03, 0.03) };
    let byte = |v: f32| (v * 255.0).round() as u8;
    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM => out.copy_from_slice(&[byte(r), byte(g), byte(b), 255]),
        DXGI_FORMAT_B8G8R8A8_UNORM => out.copy_from_slice(&[byte(b), byte(g), byte(r), 255]),
        DXGI_FORMAT_R10G10B10A2_UNORM => {
            let ten = |v: f32| (v * 1023.0).round() as u32;
            out.copy_from_slice(&(ten(r) | ten(g) << 10 | ten(b) << 20 | 3 << 30).to_le_bytes());
        }
        _ => {
            for (i, v) in [r, g, b, 1.0].into_iter().enumerate() {
                out[i * 2..i * 2 + 2].copy_from_slice(&half(v).to_le_bytes());
            }
        }
    }
}

/// A value from 0 to 1 as a 16-bit float.
fn half(v: f32) -> u16 {
    let bits = v.clamp(0.0, 1.0).to_bits();
    if v <= 0.0 {
        return 0;
    }
    // Sign 0; the exponent moves from a bias of 127 to one of 15; the top
    // ten bits of the fraction stay. Values under 2^-14 (none of ours) are 0.
    let exponent = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
    if exponent <= 0 {
        return 0;
    }
    ((exponent as u32) << 10 | (bits >> 13) & 0x3FF) as u16
}

/// The box for `lines` at `scale` dots a pixel: its width and height in
/// pixels and, row by row from the top, whether each pixel is text. Lines
/// are 9 dots apart (7 of glyph, 2 of gap).
fn bitmap(lines: &[String], scale: u32) -> (u32, u32, Vec<bool>) {
    let pad = 2 * scale;
    let widest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u32;
    let width = 2 * pad + (widest * 6).saturating_sub(1) * scale;
    let rows = lines.len() as u32;
    let height = 2 * pad + (rows * 9).saturating_sub(2) * scale;
    let mut lit = vec![false; (width * height) as usize];
    for (line, text) in lines.iter().enumerate() {
        for (i, c) in text.chars().enumerate() {
            for (row, bits) in glyph(c).into_iter().enumerate() {
                for column in 0..5u32 {
                    if bits >> (4 - column) & 1 == 0 {
                        continue;
                    }
                    let x0 = pad + (i as u32 * 6 + column) * scale;
                    let y0 = pad + (line as u32 * 9 + row as u32) * scale;
                    for y in y0..y0 + scale {
                        for x in x0..x0 + scale {
                            lit[(y * width + x) as usize] = true;
                        }
                    }
                }
            }
        }
    }
    (width, height, lit)
}

/// A buffer the card copies a text's box from, with its footprint.
struct Made {
    key: (u64, i32, u32),
    buffer: ID3D12Resource,
    width: u32,
    height: u32,
    pitch: u32,
}

// SAFETY: a free-threaded D3D12 object and plain numbers.
unsafe impl Send for Made {}

/// The boxes made last (the newest at the back), by text, format and scale.
static MADE: Mutex<VecDeque<Made>> = Mutex::new(VecDeque::new());

/// Copies the box onto `back`, a back buffer of `width` by `height` pixels
/// of `format` that `list` holds as a copy's destination. Nothing if the
/// counter is off, nothing was measured yet, the format is not one of ours
/// or the frame is too small for the box.
pub(crate) fn draw(host: &crate::host::Host, list: &ID3D12GraphicsCommandList, back: &ID3D12Resource, (width, height, format): (u32, u32, DXGI_FORMAT)) {
    if !on() {
        return;
    }
    let Some(lines) = LINES.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
        return;
    };
    let Some(bytes) = pixel_bytes(format) else {
        return;
    };
    // About a fortieth of the frame's height a glyph: 6 dots a pixel at
    // 2160 lines, 3 at 1080.
    let scale = (height / 360).clamp(2, 12);
    let key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        lines.hash(&mut h);
        (h.finish(), format.0, scale)
    };
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    if !made.iter().any(|m| m.key == key) {
        let (w, h, lit) = bitmap(&lines, scale);
        let pitch = (w as usize * bytes).next_multiple_of(D3D12_TEXTURE_DATA_PITCH_ALIGNMENT as usize);
        let Some((buffer, data)) = host.shared_buffer((pitch * h as usize) as u64) else {
            return;
        };
        // SAFETY: the buffer's own mapped bytes, `pitch * h` of them.
        let out = unsafe { std::slice::from_raw_parts_mut(data, pitch * h as usize) };
        for y in 0..h as usize {
            for x in 0..w as usize {
                let at = y * pitch + x * bytes;
                pixel(format, lit[y * w as usize + x], &mut out[at..at + bytes]);
            }
        }
        if made.len() >= KEEP {
            made.pop_front();
        }
        made.push_back(Made { key, buffer, width: w, height: h, pitch: pitch as u32 });
    }
    let Some(m) = made.iter().find(|m| m.key == key) else {
        return;
    };
    let margin = 4 * scale;
    if m.width + 2 * margin > width || m.height + 2 * margin > height {
        return;
    }
    // SAFETY: an open list; the back buffer is a copy's destination in it
    // and the box lies inside it; our buffer holds `pitch * height` bytes
    // and is kept until [`KEEP`] newer texts were made.
    unsafe {
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&m.buffer),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: format, Width: m.width, Height: m.height, Depth: 1, RowPitch: m.pitch },
                },
            },
        };
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(back),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        list.CopyTextureRegion(&to, margin, margin, 0, &from, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Frame {
        Frame { fps: 5.04, ms: 199.9, gpu_ms: 68.0, draws: 6790.4 }
    }

    #[test]
    fn the_lines_say_rate_time_processor_gpu_memory_and_draws() {
        let m = Machine { cpu: Some(47.2), ram: Some((12.13, 26.31, 31.9)), vram: Some((11.71, 13.8)) };
        assert_eq!(
            lines(&frame(), &m),
            ["5.0 FPS  199.9 MS", "CPU 47%  GPU 34%", "RAM 12.1 GB  SYS 26.3/32 GB", "VRAM 11.7/13.8 GB", "DRAWS 6790"]
        );
        // What the system did not say is left out.
        assert_eq!(lines(&frame(), &Machine::default()), ["5.0 FPS  199.9 MS", "GPU 34%", "DRAWS 6790"]);
    }

    #[test]
    fn every_character_of_the_lines_has_a_glyph() {
        let m = Machine { cpu: Some(1.0), ram: Some((1.0, 1.0, 1.0)), vram: Some((1.0, 1.0)) };
        for line in lines(&frame(), &m) {
            for c in line.chars().filter(|&c| c != ' ') {
                assert!(glyph(c).iter().any(|&r| r != 0), "{c:?} in {line:?}");
            }
        }
    }

    #[test]
    fn the_box_holds_every_line_with_a_margin() {
        let text = ["10.3 FPS".to_owned(), "DRAWS 6".to_owned()];
        let (w, h, lit) = bitmap(&text, 3);
        // The widest line (eight glyphs: five dots and a gap each, no gap
        // after the last), two lines nine dots apart less the last gap, a
        // pad of two dots all round.
        assert_eq!((w, h), ((8 * 6 - 1 + 4) * 3, (2 * 9 - 2 + 4) * 3));
        assert_eq!(lit.len(), (w * h) as usize);
        assert!(lit[..(w * 6) as usize].iter().all(|&l| !l), "the top pad");
        assert!((0..h).all(|y| !lit[(y * w) as usize] && !lit[(y * w + w - 1) as usize]), "the side pads");
        let at = |x: u32, y: u32| lit[(y * w + x) as usize];
        // Line 0, glyph 0 ("1"): dot column 2 of row 0 is lit, column 0 is not.
        assert!(at(6 + 2 * 3, 6) && !at(6, 6));
        // Line 1 begins nine dots lower: "D" has its left column lit.
        assert!(at(6, 6 + 9 * 3));
    }

    #[test]
    fn pixels_are_in_the_formats_the_layer_presents_in() {
        let mut four = [0u8; 4];
        pixel(DXGI_FORMAT_R8G8B8A8_UNORM, true, &mut four);
        assert_eq!(four, [255, 235, 41, 255]);
        pixel(DXGI_FORMAT_B8G8R8A8_UNORM, true, &mut four);
        assert_eq!(four, [41, 235, 255, 255]);
        pixel(DXGI_FORMAT_R10G10B10A2_UNORM, true, &mut four);
        let v = u32::from_le_bytes(four);
        assert_eq!((v & 1023, v >> 10 & 1023, v >> 20 & 1023, v >> 30), (1023, 941, 164, 3));
        let mut eight = [0u8; 8];
        pixel(DXGI_FORMAT_R16G16B16A16_FLOAT, true, &mut eight);
        assert_eq!(u16::from_le_bytes([eight[0], eight[1]]), 0x3C00, "1.0 as a half");
        assert_eq!(u16::from_le_bytes([eight[6], eight[7]]), 0x3C00);
        assert_eq!(half(0.5), 0x3800);
        assert_eq!(half(0.0), 0);
        assert_eq!(pixel_bytes(DXGI_FORMAT_R16G16B16A16_FLOAT), Some(8));
        assert_eq!(pixel_bytes(DXGI_FORMAT_D32_FLOAT), None);
    }
}
